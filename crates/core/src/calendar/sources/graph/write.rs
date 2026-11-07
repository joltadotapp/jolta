//! Calendar ops on Microsoft 365: a POST with a `transactionId`, so a
//! retried create returns the event the first try made, a PATCH of only the
//! changed fields and a DELETE, each guarded by the change key Jolta read.
//! The new event keeps Jolta's UID through the Global Object ID, as Add to
//! calendar's copies do.

use chrono_tz::Tz;
use serde_json::{Value, json};

use super::super::{OpWrite, Version, Written, cannot, changed_fields, created_fields, encode, merge_json};
use super::add::{GLOBAL_OBJECT_IDS, global_object_id, if_match, pattern, when};
use super::{Api, FIELDS, GraphCalendar, GraphEvent, event, get_ok, instance};
use crate::calendar::change::Field;
use crate::calendar::ops::Extra;
use crate::calendar::{AlarmTrigger, Calendar, EventData, EventStatus, ShowAs, Span, Visibility, tz};
use crate::{Error, Result};

const MICROSOFT: &str = "Microsoft 365";

impl GraphCalendar {
    pub async fn write(&self, calendar: &Calendar, write: &OpWrite<'_>) -> Result<Written> {
        send(self, calendar, write, &self.account.address, tz::here()).await
    }

    pub async fn fetch(&self, remote_id: &str, uid: &str) -> Result<Option<Version>> {
        fetch(self, remote_id, uid, &self.account.address, tz::here()).await
    }
}

pub(crate) fn writable(write: &OpWrite<'_>, here: Tz) -> Result<()> {
    match write {
        OpWrite::Create { events, key, extra } => {
            let (master, changed) = split_series(events)?;
            create_body(std::slice::from_ref(master), key, *extra, here)?;
            changed.iter().try_for_each(|e| occurrence_body(master, e, here).map(drop))
        }
        OpWrite::Update { base, event, extra, .. } | OpWrite::Occurrence { base, event, extra, .. } => patch_body(base, event, *extra, here).map(drop),
        OpWrite::Delete { .. } | OpWrite::Cancel { .. } => Ok(()),
    }
}

/// A series and its changed occurrences, as a create takes them.
fn split_series(events: &[EventData]) -> Result<(&EventData, Vec<&EventData>)> {
    let master = events.iter().find(|e| e.recurrence_id.is_none()).ok_or_else(|| cannot(MICROSOFT, "take a changed occurrence without its series"))?;
    Ok((master, events.iter().filter(|e| e.recurrence_id.is_some()).collect()))
}

/// The PATCH that makes the occurrence `changed` of a new series `master`
/// what it is; None when it was taken out instead.
fn occurrence_body(master: &EventData, changed: &EventData, here: Tz) -> Result<Option<Value>> {
    if changed.status == EventStatus::Cancelled {
        return Ok(None);
    }
    let rid = changed.recurrence_id.as_ref().ok_or_else(|| cannot(MICROSOFT, "change an occurrence without its start"))?;
    let base = crate::calendar::series::occurrence(std::slice::from_ref(master), rid, here).ok_or_else(|| cannot(MICROSOFT, "move an all-day occurrence to a time"))?;
    patch_body(&base, changed, None, here).map(Some)
}

/// The id of the instance of `series` at `rid`, None when the series has none there.
async fn instance_at(api: &impl Api, series: &crate::calendar::rsvp::Stored, rid: &crate::calendar::Moment) -> Result<Option<String>> {
    match instance(api, series, rid).await {
        Ok(i) => Ok(Some(i.id)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

async fn send(api: &impl Api, calendar: &Calendar, write: &OpWrite<'_>, me: &str, here: Tz) -> Result<Written> {
    match *write {
        OpWrite::Create { events, key, extra } => {
            let (master, changed) = split_series(events)?;
            let body = create_body(std::slice::from_ref(master), key, extra, here)?;
            let (status, reply) = api.request("POST", &format!("/me/calendars/{}/events", encode(&calendar.remote_id)), Some(&body), &[]).await?;
            if !(200..300).contains(&status) {
                return Err(api.failure(status, &reply));
            }
            // Graph's pattern holds no EXDATE, and the series' changed
            // occurrences are writes of their own; a retry gets the series
            // the first try made back for its transactionId and writes them
            // again, which changes nothing more.
            let series = crate::calendar::rsvp::Stored {
                id: crate::calendar::EventId(0),
                remote_id: reply["id"].as_str().unwrap_or_default().to_string(),
                etag: None,
                raw: serde_json::to_vec(&reply).ok(),
                data: master.clone(),
            };
            let taken_out = master.recurrence.iter().flat_map(|r| &r.exdates);
            for rid in taken_out.chain(changed.iter().filter(|e| e.status == EventStatus::Cancelled).filter_map(|e| e.recurrence_id.as_ref())) {
                if let Some(id) = instance_at(api, &series, rid).await? {
                    let (status, reply) = api.request("DELETE", &format!("/me/events/{}", encode(&id)), None, &[]).await?;
                    if !matches!(status, 200..300 | 404 | 410) {
                        return Err(api.failure(status, &reply));
                    }
                }
            }
            for e in changed {
                let (Some(body), Some(rid)) = (occurrence_body(master, e, here)?, e.recurrence_id.as_ref()) else { continue };
                if let Some(id) = instance_at(api, &series, rid).await? {
                    let (status, reply) = api.request("PATCH", &format!("/me/events/{}", encode(&id)), Some(&body), &[]).await?;
                    if !(200..300).contains(&status) {
                        return Err(api.failure(status, &reply));
                    }
                }
            }
            Ok(Written::Saved(Some(version(api, reply, &master.uid, me, here).await?)))
        }
        OpWrite::Occurrence { series, base, event, extra } => {
            let Some(etag) = series.etag.as_deref() else { return Ok(Written::Stale) };
            let rid = event.recurrence_id.as_ref().ok_or_else(|| cannot(MICROSOFT, "change an occurrence without its start"))?;
            let Some(id) = instance_at(api, series, rid).await? else { return Ok(Written::Gone) };
            let body = patch_body(base, event, extra, here)?;
            let guard = if_match(etag);
            let (status, reply) = api.request("PATCH", &format!("/me/events/{}", encode(&id)), Some(&body), &[("If-Match", &guard)]).await?;
            match status {
                200..300 => Ok(Written::Saved(Some(version(api, reply, &event.uid, me, here).await?))),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                _ => Err(api.failure(status, &reply)),
            }
        }
        OpWrite::Cancel { held, occurrence, changed } => {
            let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
            // Exchange deletes an instance whatever change key If-Match
            // names (Microsoft 365 test account, 2026-11-07), so the key is
            // compared first.
            let (status, now) = api.request("GET", &format!("/me/events/{}?$select=changeKey", encode(&held.remote_id)), None, &[]).await?;
            match status {
                200..300 if now["changeKey"].as_str() == Some(etag) => {}
                200..300 => return Ok(Written::Stale),
                404 | 410 => return Ok(Written::Gone),
                _ => return Err(api.failure(status, &now)),
            }
            let rid = occurrence.recurrence_id.as_ref().ok_or_else(|| cannot(MICROSOFT, "delete an occurrence without its start"))?;
            let id = match changed {
                true => held.remote_id.clone(),
                false => match instance_at(api, held, rid).await? {
                    Some(id) => id,
                    None => return Ok(Written::Gone),
                },
            };
            let guard = if_match(etag);
            let (status, reply) = api.request("DELETE", &format!("/me/events/{}", encode(&id)), None, &[("If-Match", &guard)]).await?;
            match status {
                200..300 => Ok(Written::Saved(None)),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                _ => Err(api.failure(status, &reply)),
            }
        }
        OpWrite::Update { held, base, event, extra, .. } => {
            let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
            let body = patch_body(base, event, extra, here)?;
            let guard = if_match(etag);
            let (status, reply) = api.request("PATCH", &format!("/me/events/{}", encode(&held.remote_id)), Some(&body), &[("If-Match", &guard)]).await?;
            match status {
                200..300 => Ok(Written::Saved(Some(version(api, reply, &event.uid, me, here).await?))),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                400 if deleted(api, &held.remote_id).await? => Ok(Written::Gone),
                _ => Err(api.failure(status, &reply)),
            }
        }
        OpWrite::Delete { held } => {
            let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
            let guard = if_match(etag);
            let (status, reply) = api.request("DELETE", &format!("/me/events/{}", encode(&held.remote_id)), None, &[("If-Match", &guard)]).await?;
            match status {
                200..300 => Ok(Written::Saved(None)),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                400 if deleted(api, &held.remote_id).await? => Ok(Written::Gone),
                _ => Err(api.failure(status, &reply)),
            }
        }
    }
}

/// Whether the event left its calendar. Exchange moves a deleted event to
/// Deleted Items, where a PATCH by its id answers 400 ("binding to a
/// non-calendar folder") and a GET 404 (Microsoft 365 test account,
/// 2026-11-07).
async fn deleted(api: &impl Api, remote_id: &str) -> Result<bool> {
    let (status, _) = api.request("GET", &format!("/me/events/{}?$select=id", encode(remote_id)), None, &[]).await?;
    Ok(matches!(status, 404 | 410))
}

/// The event as Microsoft 365 has it, None once deleted or cancelled.
async fn fetch(api: &impl Api, remote_id: &str, uid: &str, me: &str, here: Tz) -> Result<Option<Version>> {
    let (status, reply) = api.request("GET", &format!("/me/events/{}?$select={FIELDS}", encode(remote_id)), None, &[]).await?;
    match status {
        200..300 if reply["isCancelled"] == true => Ok(None),
        200..300 => Ok(Some(version(api, reply, uid, me, here).await?)),
        404 | 410 => Ok(None),
        _ => Err(api.failure(status, &reply)),
    }
}

/// Graph's answer as the event to store, under the stored event's UID. A
/// series master is read again with the occurrences it cancelled, which
/// only a GET that names them returns; a changed occurrence is read on its
/// series' clock, so its series comes too.
async fn version(api: &impl Api, reply: Value, uid: &str, me: &str, here: Tz) -> Result<Version> {
    let reply = match (reply["type"].as_str(), reply["id"].as_str()) {
        (Some("seriesMaster"), Some(id)) => get_ok(api, &format!("/me/events/{}?$select={FIELDS},cancelledOccurrences", encode(id))).await?,
        _ => reply,
    };
    let raw = serde_json::to_vec(&reply).ok();
    let g: GraphEvent = serde_json::from_value(reply).map_err(|e| Error::Other(format!("Microsoft 365 reply: {e}")))?;
    let master: Option<GraphEvent> = match (&g.series_master_id, g.kind.as_str()) {
        (Some(id), "exception" | "occurrence") => {
            let m = get_ok(api, &format!("/me/events/{}?$select={FIELDS}", encode(id))).await?;
            Some(serde_json::from_value(m).map_err(|e| Error::Other(format!("Microsoft 365 reply: {e}")))?)
        }
        _ => None,
    };
    let data = event(&g, uid.to_string(), master.as_ref(), &[me.to_string()], here)
        .ok_or_else(|| Error::Other("Microsoft 365 returned an event without a start.".into()))?;
    Ok(Version { remote_id: g.id, etag: g.change_key, events: vec![data], raw })
}

/// The POST's body: every field the event sets, the `transactionId`
/// `key`, and the UID as its Global Object ID. A series' changed
/// occurrences are writes of their own.
pub(crate) fn create_body(events: &[EventData], key: &str, extra: Option<&Extra>, here: Tz) -> Result<Value> {
    let [e] = events else { return Err(cannot(MICROSOFT, "take a repeating event with changed occurrences from Jolta yet")) };
    let mut v = json!({ "transactionId": key });
    for field in created_fields(e) {
        set(&mut v, field, e, here, true)?;
    }
    let mut properties: Vec<Value> = GLOBAL_OBJECT_IDS.iter().map(|id| json!({ "id": id, "value": global_object_id(&e.uid) })).collect();
    if let Some(Extra::Json(fields)) = extra
        && let Some(theirs) = fields.get("singleValueExtendedProperties").and_then(Value::as_array)
    {
        properties.extend(theirs.iter().filter(|p| !GLOBAL_OBJECT_IDS.iter().any(|id| p["id"] == *id)).cloned());
    }
    v["singleValueExtendedProperties"] = json!(properties);
    merge_json(&mut v, extra, MICROSOFT)?;
    Ok(v)
}

/// The PATCH's body: only the fields that changed from `base`.
pub(crate) fn patch_body(base: &EventData, e: &EventData, extra: Option<&Extra>, here: Tz) -> Result<Value> {
    let mut v = json!({});
    for field in changed_fields(base, e) {
        set(&mut v, field, e, here, false)?;
    }
    merge_json(&mut v, extra, MICROSOFT)?;
    Ok(v)
}

/// Puts `field` of `e` into the body the way Graph takes it.
fn set(v: &mut Value, field: Field, e: &EventData, here: Tz, creating: bool) -> Result<()> {
    match field {
        Field::Time => {
            v["start"] = when(&e.span, true, here);
            v["end"] = when(&e.span, false, here);
            v["isAllDay"] = json!(matches!(e.span, Span::AllDay { .. }));
            // The repeat pattern starts on the event's day.
            if e.recurrence.is_some() {
                set(v, Field::Repeat, e, here, creating)?;
            }
        }
        Field::Repeat => {
            v["recurrence"] = match &e.recurrence {
                None => Value::Null,
                Some(r) => pattern(r, &e.span, here).ok_or_else(|| cannot(MICROSOFT, "repeat an event this way"))?,
            }
        }
        Field::Location => v["location"] = json!({ "displayName": e.location }),
        Field::Title => v["subject"] = json!(e.title),
        Field::Description => v["body"] = json!({ "contentType": if e.description_html { "html" } else { "text" }, "content": e.description }),
        Field::Status if e.status == EventStatus::Confirmed => {}
        Field::Status => return Err(cannot(MICROSOFT, "mark an event tentative or cancelled by an edit")),
        Field::Alarms => match e.alarms.as_slice() {
            [] => v["isReminderOn"] = json!(false),
            [a] if !a.email && matches!(a.trigger, AlarmTrigger::MinutesBefore(m) if m >= 0) => {
                let AlarmTrigger::MinutesBefore(m) = a.trigger else { unreachable!("matched above") };
                v["isReminderOn"] = json!(true);
                v["reminderMinutesBeforeStart"] = json!(m);
            }
            _ => return Err(cannot(MICROSOFT, "hold more than one alert, an email alert, or one at a set time")),
        },
        Field::Color => return Err(cannot(MICROSOFT, "color one event; it colors events by category")),
        Field::Categories => v["categories"] = json!(e.categories),
        Field::ShowAs => {
            v["showAs"] = json!(match e.show_as {
                ShowAs::Free => "free",
                ShowAs::Tentative => "tentative",
                ShowAs::Busy => "busy",
                ShowAs::OutOfOffice => "oof",
                ShowAs::Elsewhere => "workingElsewhere",
            })
        }
        Field::Visibility => {
            v["sensitivity"] = json!(match e.visibility {
                Visibility::Default => "normal",
                Visibility::Public if creating => "normal",
                Visibility::Public => return Err(cannot(MICROSOFT, "mark an event public")),
                Visibility::Private => "private",
                Visibility::Confidential => "confidential",
            })
        }
        Field::People => return Err(cannot(MICROSOFT, "change guests from Jolta yet")),
        Field::MeetingLink if creating => {}
        Field::MeetingLink => return Err(cannot(MICROSOFT, "change the video call from Jolta yet")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use chrono::NaiveDateTime;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::rsvp::Stored;
    use crate::calendar::{Access, Alarm, CalendarId, CalendarSource, EventId, Local, Recurrence, Zone};

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn standup() -> EventData {
        EventData { title: "Standup".into(), ..EventData::new("u1@jolta.app", Span::Timed { start: at("2026-11-24 09:00"), end: at("2026-11-24 09:30") }) }
    }

    #[test]
    fn a_create_sends_the_transaction_id_the_uid_and_what_the_event_sets() {
        let e = EventData { categories: vec!["Blue category".into()], show_as: ShowAs::Tentative, ..standup() };
        let body = create_body(std::slice::from_ref(&e), "tx1", None, Brussels).unwrap();
        let goid = global_object_id("u1@jolta.app");
        assert_eq!(
            body,
            json!({
                "transactionId": "tx1",
                "subject": "Standup",
                "start": { "dateTime": "2026-11-24T09:00:00", "timeZone": "Europe/Brussels" },
                "end": { "dateTime": "2026-11-24T09:30:00", "timeZone": "Europe/Brussels" },
                "isAllDay": false,
                "categories": ["Blue category"],
                "showAs": "tentative",
                "singleValueExtendedProperties": [{ "id": GLOBAL_OBJECT_IDS[0], "value": goid }, { "id": GLOBAL_OBJECT_IDS[1], "value": goid }],
            })
        );
    }

    #[test]
    fn a_patch_sends_only_the_changed_fields() {
        let base = standup();
        let e = EventData { location: "Room 2".into(), alarms: vec![Alarm { trigger: AlarmTrigger::MinutesBefore(15), email: false }], ..base.clone() };
        assert_eq!(
            patch_body(&base, &e, None, Brussels).unwrap(),
            json!({ "location": { "displayName": "Room 2" }, "isReminderOn": true, "reminderMinutesBeforeStart": 15 })
        );
        let weekly = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;BYDAY=TU".into()], ..Default::default() }), ..base.clone() };
        let moved = EventData { span: Span::Timed { start: at("2026-11-24 10:00"), end: at("2026-11-24 10:30") }, ..weekly.clone() };
        let body = patch_body(&weekly, &moved, None, Brussels).unwrap();
        assert!(body["recurrence"]["range"]["startDate"].is_string(), "a series moved in time carries its pattern: {body}");
        assert!(patch_body(&base, &EventData { color: Some("#ff0000".into()), ..base.clone() }, None, Brussels).is_err());
        let two = EventData { alarms: vec![Alarm { trigger: AlarmTrigger::MinutesBefore(5), email: false }; 2], ..base.clone() };
        assert!(patch_body(&base, &two, None, Brussels).is_err(), "Graph keeps one reminder");
    }

    #[test]
    fn extra_fields_reach_the_body_and_never_replace_a_modelled_one() {
        let hold = json!({ "id": "String {00020329-0000-0000-C000-000000000046} Name JoltaHold", "value": "1" });
        let extra = Extra::Json(json!({ "singleValueExtendedProperties": [hold], "subject": "Not this", "sensitivity": "private" }).as_object().unwrap().clone());
        let body = create_body(&[standup()], "tx1", Some(&extra), Brussels).unwrap();
        let properties = body["singleValueExtendedProperties"].as_array().unwrap();
        assert_eq!(properties.len(), 3, "the UID's two and the hold: {body}");
        assert_eq!(properties[2], hold, "passed through untouched");
        assert_eq!(body["subject"], "Standup");
        assert_eq!(body["sensitivity"], "private", "a field the event leaves at its default can come from extra");
    }

    struct Fake {
        replies: Mutex<VecDeque<(u16, Value)>>,
        seen: Mutex<Vec<String>>,
    }

    impl Api for Fake {
        async fn request(&self, method: &str, url: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<(u16, Value)> {
            let guard = headers.iter().find(|(k, _)| *k == "If-Match").map(|(_, v)| format!(" If-Match {v}")).unwrap_or_default();
            let tx = body.and_then(|b| b["transactionId"].as_str()).map(|t| format!(" tx {t}")).unwrap_or_default();
            self.seen.lock().unwrap().push(format!("{method} {}{guard}{tx}", url.split('?').next().unwrap()));
            Ok(self.replies.lock().unwrap().pop_front().expect("a reply"))
        }

        fn failure(&self, status: u16, _: &Value) -> Error {
            Error::Other(format!("Graph {status}"))
        }

        fn zone(&self) -> Tz {
            Brussels
        }
    }

    fn calendar() -> Calendar {
        Calendar {
            id: CalendarId(1),
            account_id: None,
            source: CalendarSource::Graph,
            remote_id: "cal1".into(),
            name: "Calendar".into(),
            color: None,
            zone: None,
            access: Access::Owner,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: true,
            cursor: None,
            refresh: None,
            synced: None,
        }
    }

    fn graph(id: &str, change_key: &str) -> Value {
        json!({ "id": id, "changeKey": change_key, "type": "singleInstance", "iCalUId": "040000008200E00074C5B7101A82E008", "subject": "Standup",
                "start": { "dateTime": "2026-11-24T08:00:00.0000000", "timeZone": "UTC" }, "end": { "dateTime": "2026-11-24T08:30:00.0000000", "timeZone": "UTC" },
                "originalStartTimeZone": "Europe/Brussels", "originalEndTimeZone": "Europe/Brussels" })
    }

    async fn sent(write: OpWrite<'_>, replies: Vec<(u16, Value)>) -> (Result<Written>, Vec<String>) {
        let fake = Fake { replies: Mutex::new(replies.into()), seen: Mutex::new(Vec::new()) };
        let done = send(&fake, &calendar(), &write, "me@example.com", Brussels).await;
        (done, fake.seen.into_inner().unwrap())
    }

    /// Check 4: a retry sends the same transactionId, which Graph answers
    /// with the event the first try made.
    #[tokio::test]
    async fn a_retried_create_sends_the_same_transaction_id() {
        let e = standup();
        let create = OpWrite::Create { events: std::slice::from_ref(&e), key: "tx1", extra: None };
        let (first, seen_first) = sent(create, vec![(201, graph("AAMk1", "ck1"))]).await;
        let (again, seen_again) = sent(create, vec![(201, graph("AAMk1", "ck1"))]).await;
        assert_eq!(seen_first, seen_again);
        assert_eq!(seen_first, ["POST /me/calendars/cal1/events tx tx1"]);
        let Written::Saved(Some(v)) = first.unwrap() else { panic!("saved") };
        assert_eq!((v.remote_id.as_str(), v.etag.as_deref(), v.events[0].uid.as_str()), ("AAMk1", Some("ck1"), "u1@jolta.app"), "the stored event keeps Jolta's UID");
        assert_eq!(again.unwrap(), Written::Saved(Some(v)));
    }

    /// A PATCH answer leaves out the occurrences a series cancelled, so the
    /// series is read again with them before it is stored.
    #[tokio::test]
    async fn a_series_is_stored_with_the_occurrences_it_cancelled() {
        let base = standup();
        let held = Stored { id: EventId(1), remote_id: "AAMk1".into(), etag: Some("ck1".into()), raw: None, data: base.clone() };
        let mine = EventData { title: "Daily".into(), ..base.clone() };
        let master = |cancelled: Value| {
            let mut m = graph("AAMk1", "ck2");
            m["type"] = json!("seriesMaster");
            m["recurrence"] = json!({ "pattern": { "type": "daily", "interval": 1 }, "range": { "type": "noEnd", "startDate": "2026-11-24", "recurrenceTimeZone": "Europe/Brussels" } });
            m["cancelledOccurrences"] = cancelled;
            m
        };
        let (done, seen) = sent(OpWrite::Update { held: &held, base: &base, event: &mine, extra: None, dropped: &[] }, vec![(200, master(json!([]))), (200, master(json!(["OID.AAMk1.2026-11-25"])))]).await;
        let Written::Saved(Some(v)) = done.unwrap() else { panic!("saved") };
        assert_eq!(seen.last().unwrap(), "GET /me/events/AAMk1");
        assert_eq!(v.events[0].recurrence.as_ref().unwrap().exdates.len(), 1, "the cancelled 25th stays cancelled");
    }

    #[tokio::test]
    async fn every_patch_and_delete_carries_the_held_change_key() {
        let base = standup();
        let held = Stored { id: EventId(1), remote_id: "AAMk1".into(), etag: Some("ck1".into()), raw: None, data: base.clone() };
        let mine = EventData { title: "Daily".into(), ..base.clone() };
        let update = OpWrite::Update { held: &held, base: &base, event: &mine, extra: None, dropped: &[] };
        for (status, expected) in [(412, Written::Stale), (404, Written::Gone)] {
            let (done, seen) = sent(update, vec![(status, json!({}))]).await;
            assert_eq!((done.unwrap(), seen), (expected, vec!["PATCH /me/events/AAMk1 If-Match W/\"ck1\"".to_string()]));
        }
        let (done, seen) = sent(update, vec![(400, json!({})), (404, json!({}))]).await;
        assert_eq!(done.unwrap(), Written::Gone, "an event in Deleted Items is deleted");
        assert_eq!(seen.last().unwrap(), "GET /me/events/AAMk1");
        let (done, _) = sent(update, vec![(400, json!({})), (200, json!({ "id": "AAMk1" }))]).await;
        assert!(done.is_err(), "a 400 on an event still there is a failure");
        let (done, seen) = sent(OpWrite::Delete { held: &held }, vec![(204, Value::Null)]).await;
        assert_eq!((done.unwrap(), seen), (Written::Saved(None), vec!["DELETE /me/events/AAMk1 If-Match W/\"ck1\"".to_string()]));
    }
}
