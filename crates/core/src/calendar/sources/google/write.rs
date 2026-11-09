//! Calendar ops on Google: `events.insert` at an event id Jolta picks,
//! `events.patch` of only the changed fields, and `events.delete`, each
//! with the `sendUpdates` the op's notify asks for and every patch and
//! delete guarded by the event's ETag.

use chrono_tz::Tz;
use serde_json::{Value, json};

use super::super::{OpWrite, Tell, Version, Written, cannot, changed_fields, created_fields, encode, merge_json, already_written};
use super::add::when;
use super::{BASE, Google, GoogleEvent, Reminder, event, failure};
use crate::calendar::change::Field;
use crate::calendar::ical_edit::Prop;
use crate::calendar::ops::Notify;
use crate::calendar::{Alarm, AlarmTrigger, Calendar, EventData, EventStatus, MeetingProvider, Moment, Response, Role, ShowAs, Visibility, tz};
use crate::{Error, Result};

const GOOGLE: &str = "Google Calendar";

/// What a calendar op asks of Google: `Google` sends it, a test answers it.
pub(super) trait Http {
    async fn request(&self, method: reqwest::Method, url: &str, body: Option<&Value>, if_match: Option<&str>) -> Result<(u16, Vec<u8>)>;
    /// The account's address, the user's entry among the guests.
    fn me(&self) -> &str;
    /// Waits between reads of a Meet still being made.
    async fn pause(&self, time: std::time::Duration);
}

impl Http for Google {
    async fn request(&self, method: reqwest::Method, url: &str, body: Option<&Value>, if_match: Option<&str>) -> Result<(u16, Vec<u8>)> {
        Google::request(self, method, url, body, if_match).await
    }

    fn me(&self) -> &str {
        &self.account.address
    }

    async fn pause(&self, time: std::time::Duration) {
        tokio::time::sleep(time).await;
    }
}

impl Google {
    pub async fn write(&self, calendar: &Calendar, write: &OpWrite<'_>, tell: Tell<'_>) -> Result<Written> {
        send(self, calendar, write, tell, tz::here()).await
    }

    pub async fn fetch(&self, calendar: &Calendar, remote_id: &str, alarms: &[Alarm]) -> Result<Option<Version>> {
        fetch(self, calendar, remote_id, alarms).await
    }

    /// The instance of the series `series` at `rid`, changed or not.
    pub async fn fetch_occurrence(&self, calendar: &Calendar, series: &str, rid: &Moment, alarms: &[Alarm]) -> Result<Option<Version>> {
        fetch(self, calendar, &instance_id(series, rid, tz::here()), alarms).await
    }
}

/// The id Google gives an event inserted with this UID and no id of its
/// own: an underscore and the UID in base32hex, lowercase (seen on the
/// Google test account, 2026-11-07). The stored event is filed under it
/// until Google's answer names the id it made.
pub(crate) fn event_id(uid: &str) -> String {
    const DIGITS: &[u8; 32] = b"0123456789abcdefghijklmnopqrstuv";
    let bits: Vec<u8> = uid.bytes().flat_map(|byte| (0..8).rev().map(move |i| (byte >> i) & 1)).collect();
    std::iter::once('_').chain(bits.chunks(5).map(|five| DIGITS[(five.iter().fold(0, |n, b| n << 1 | b) << (5 - five.len())) as usize] as char)).collect()
}

pub(crate) fn writable(write: &OpWrite<'_>, here: Tz) -> Result<()> {
    match write {
        OpWrite::Create { events, key, extra } => {
            let (master, changed) = split_series(events)?;
            insert_body(std::slice::from_ref(master), *extra, key, here)?;
            changed.iter().try_for_each(|e| occurrence_body(master, e, here).map(drop))
        }
        OpWrite::Update { base, event, extra, .. } | OpWrite::Occurrence { base, event, extra, .. } => patch_body(base, event, *extra, "", here).map(drop),
        OpWrite::Delete { .. } | OpWrite::Cancel { .. } | OpWrite::Move { .. } => Ok(()),
    }
}

/// The id Google gives the instance of the series `series` that starts at
/// `rid`: the series' id, an underscore and the start in UTC, or the day
/// for an all-day series (seen on the Google test account, 2026-11-07).
pub(crate) fn instance_id(series: &str, rid: &Moment, here: Tz) -> String {
    match rid {
        Moment::Date(day) => format!("{series}_{}", day.format("%Y%m%d")),
        Moment::Time(t) => format!("{series}_{}", t.instant(here).format("%Y%m%dT%H%M%SZ")),
    }
}

/// A series and its changed occurrences, as a create takes them.
fn split_series(events: &[EventData]) -> Result<(&EventData, Vec<&EventData>)> {
    let master = events.iter().find(|e| e.recurrence_id.is_none()).ok_or_else(|| cannot(GOOGLE, "take a changed occurrence without its series"))?;
    Ok((master, events.iter().filter(|e| e.recurrence_id.is_some()).collect()))
}

/// The patch that makes the occurrence `changed` of a new series `master`
/// what it is; None when it was taken out instead.
fn occurrence_body(master: &EventData, changed: &EventData, here: Tz) -> Result<Option<Value>> {
    if changed.status == EventStatus::Cancelled {
        return Ok(None);
    }
    let rid = changed.recurrence_id.as_ref().ok_or_else(|| cannot(GOOGLE, "change an occurrence without its start"))?;
    let base = crate::calendar::series::occurrence(std::slice::from_ref(master), rid, here).ok_or_else(|| cannot(GOOGLE, "move an all-day occurrence to a time"))?;
    patch_body(&base, changed, None, "", here).map(Some)
}

/// Google's `sendUpdates` for the op's notify. Google mails only the guests
/// a change of people adds or removes, also with `all` (Google test
/// account, 2026-11-07), so new people only is `all` too.
fn send_updates(notify: Notify) -> &'static str {
    match notify {
        Notify::All | Notify::Changed => "all",
        Notify::Outside => "externalOnly",
        Notify::Nobody => "none",
    }
}

async fn send(google: &impl Http, calendar: &Calendar, write: &OpWrite<'_>, tell: Tell<'_>, here: Tz) -> Result<Written> {
    let events_url = format!("{BASE}/calendars/{}/events", encode(&calendar.remote_id));
    let updates = send_updates(tell.notify);
    match *write {
        OpWrite::Create { events, key, extra } => {
            let (master, changed) = split_series(events)?;
            let mut written = create(google, calendar, master, extra, key, &events_url, updates, here).await?;
            // The series' changed occurrences follow as writes of their
            // instances. A retry finds the series made and leaves the
            // instances the first try wrote alone, as each write may mail
            // the guests.
            if let Written::Saved(Some(series)) = &mut written {
                for e in changed {
                    let id = instance_id(&series.remote_id, e.recurrence_id.as_ref().expect("a changed occurrence"), here);
                    let url = format!("{events_url}/{}?sendUpdates={updates}", encode(&id));
                    let (method, body) = match occurrence_body(master, e, here)? {
                        Some(body) => (reqwest::Method::PATCH, Some(body)),
                        None => (reqwest::Method::DELETE, None),
                    };
                    let now = fetch(google, calendar, &id, &e.alarms).await?;
                    let done = match (&now, &body) {
                        (None, None) => true,
                        (Some(v), Some(_)) => v.events.first().is_some_and(|current| already_written(master, e, current, here)),
                        _ => false,
                    };
                    if done {
                        series.instances.extend(now.filter(|_| body.is_some()));
                        continue;
                    }
                    let (status, reply) = google.request(method.clone(), &url, body.as_ref(), None).await?;
                    if !matches!(status, 200..300 | 404 | 410) {
                        return Err(failure(status, &reply));
                    }
                    if method == reqwest::Method::PATCH && (200..300).contains(&status) {
                        series.instances.push(version(google, calendar, &reply, &e.alarms)?);
                    }
                }
            }
            Ok(written)
        }
        OpWrite::Occurrence { series, base, event, extra } => {
            let Some(etag) = series.etag.as_deref() else { return Ok(Written::Stale) };
            let rid = event.recurrence_id.as_ref().ok_or_else(|| cannot(GOOGLE, "change an occurrence without its start"))?;
            let id = instance_id(&series.remote_id, rid, here);
            let body = patch_body(base, event, extra, &request_id(&id, etag), here)?;
            let url = format!("{events_url}/{}?sendUpdates={updates}{}", encode(&id), conference_version(&body));
            let (status, reply) = google.request(reqwest::Method::PATCH, &url, Some(&body), Some(etag)).await?;
            match status {
                200..300 => Ok(Written::Saved(Some(version(google, calendar, &settled(google, calendar, &body, reply).await?, &event.alarms)?))),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                _ => Err(failure(status, &reply)),
            }
        }
        OpWrite::Cancel { held, occurrence, changed } => {
            let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
            let rid = occurrence.recurrence_id.as_ref().ok_or_else(|| cannot(GOOGLE, "delete an occurrence without its start"))?;
            let id = if changed { held.remote_id.clone() } else { instance_id(&held.remote_id, rid, here) };
            let url = format!("{events_url}/{}?sendUpdates={updates}", encode(&id));
            let (status, reply) = google.request(reqwest::Method::DELETE, &url, None, Some(etag)).await?;
            match status {
                200..300 => Ok(Written::Saved(None)),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                _ => Err(failure(status, &reply)),
            }
        }
        OpWrite::Update { held, base, event, extra, .. } => {
            let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
            let body = patch_body(base, event, extra, &request_id(&held.remote_id, etag), here)?;
            let url = format!("{events_url}/{}?sendUpdates={updates}{}", encode(&held.remote_id), conference_version(&body));
            let (status, reply) = google.request(reqwest::Method::PATCH, &url, Some(&body), Some(etag)).await?;
            match status {
                200..300 => Ok(Written::Saved(Some(version(google, calendar, &settled(google, calendar, &body, reply).await?, &event.alarms)?))),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                _ => Err(failure(status, &reply)),
            }
        }
        OpWrite::Delete { held } => {
            let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
            let url = format!("{events_url}/{}?sendUpdates={updates}", encode(&held.remote_id));
            let (status, reply) = google.request(reqwest::Method::DELETE, &url, None, Some(etag)).await?;
            match status {
                200..300 => Ok(Written::Saved(None)),
                412 => Ok(Written::Stale),
                404 | 410 => Ok(Written::Gone),
                _ => Err(failure(status, &reply)),
            }
        }
        OpWrite::Move { held, to } => {
            let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
            let alarms = &held.data.alarms;
            // Google's move names no version to hold it to, so the event is
            // read first: changed since, nothing moves.
            match fetch(google, calendar, &held.remote_id, alarms).await? {
                None => return moved_before(google, to, held).await,
                Some(now) if now.etag.as_deref() != Some(etag) => return Ok(Written::Stale),
                Some(_) => {}
            }
            let url = format!("{events_url}/{}/move?destination={}&sendUpdates=none", encode(&held.remote_id), encode(&to.remote_id));
            let (status, reply) = google.request(reqwest::Method::POST, &url, None, Some(etag)).await?;
            match status {
                200..300 => Ok(Written::Saved(Some(version(google, to, &reply, alarms)?))),
                412 => Ok(Written::Stale),
                404 | 410 => moved_before(google, to, held).await,
                _ => Err(failure(status, &reply)),
            }
        }
    }
}

/// An event no longer in its calendar: in `to` when an earlier try of the
/// move reached Google and its answer was lost, else gone.
async fn moved_before(google: &impl Http, to: &Calendar, held: &crate::calendar::rsvp::Stored) -> Result<Written> {
    match fetch(google, to, &held.remote_id, &held.data.alarms).await? {
        Some(v) if v.events.iter().any(|e| e.uid == held.data.uid) => Ok(Written::Saved(Some(v))),
        _ => Ok(Written::Gone),
    }
}

/// `events.insert` of a series or single event, with the UID Google keeps.
#[allow(clippy::too_many_arguments)]
async fn create(google: &impl Http, calendar: &Calendar, master: &EventData, extra: Option<&crate::calendar::ops::Extra>, key: &str, events_url: &str, updates: &str, here: Tz) -> Result<Written> {
    let mut body = insert_body(std::slice::from_ref(master), extra, key, here)?;
    let (uid, alarms) = (&master.uid, &master.alarms);
    let meet = conference_version(&body);
    let insert = format!("{events_url}?sendUpdates={updates}{meet}");
    let (mut status, mut reply) = google.request(reqwest::Method::POST, &insert, Some(&body), None).await?;
    if let Some(plain) = refused_status_event(status, &reply, &body) {
        body = plain;
        (status, reply) = google.request(reqwest::Method::POST, &insert, Some(&body), None).await?;
    }
    match status {
        200..300 => Ok(Written::Saved(Some(version(google, calendar, &settled(google, calendar, &body, reply).await?, alarms)?))),
        // Google holds the UID already: a retry whose first try
        // reached it keeps that event as it is now; one deleted
        // before, as for Undo of a delete, comes back through
        // `events.import`, which a second insert can't do and which
        // emails nobody.
        409 => match by_uid(google, calendar, uid, alarms).await? {
            Some((v, false)) => Ok(Written::Saved(Some(v))),
            Some((_, true)) => {
                let (status, reply) = google.request(reqwest::Method::POST, &format!("{events_url}/import{}", meet.replacen('&', "?", 1)), Some(&body), None).await?;
                match status {
                    200..300 => Ok(Written::Saved(Some(version(google, calendar, &settled(google, calendar, &body, reply).await?, alarms)?))),
                    _ => Err(failure(status, &reply)),
                }
            }
            None => Err(Error::Other("Google Calendar already has another event where Jolta would put this one.".into())),
        },
        _ => Err(failure(status, &reply)),
    }
}

/// Google offers out of office and focus time only to some accounts, and
/// refuses the others' with a 400 whose reason starts with `malformed`
/// ("malformedFocusTimeEvent", Google Workspace test account, 2026-11-07).
/// The same event as a plain busy one, when that is why `body` failed.
fn refused_status_event(status: u16, reply: &[u8], body: &Value) -> Option<Value> {
    let kind = body["eventType"].as_str().filter(|t| ["outOfOffice", "focusTime"].contains(t))?;
    let v: Value = serde_json::from_slice(reply).ok()?;
    let reasons = v["error"]["errors"].as_array().into_iter().flatten().filter_map(|e| e["reason"].as_str());
    let refused = reasons.into_iter().any(|r| r.starts_with("malformed") && r.to_ascii_lowercase().contains(&kind.to_ascii_lowercase()));
    if status != 400 || !refused {
        return None;
    }
    let mut plain = body.clone();
    let fields = plain.as_object_mut()?;
    for k in ["eventType", "outOfOfficeProperties", "focusTimeProperties"] {
        fields.remove(k);
    }
    Some(plain)
}

/// The event with this UID, deleted ones included, and whether it was
/// deleted: Google keeps a deleted event as cancelled.
async fn by_uid(google: &impl Http, calendar: &Calendar, uid: &str, alarms: &[Alarm]) -> Result<Option<(Version, bool)>> {
    let url = format!("{BASE}/calendars/{}/events?iCalUID={}&showDeleted=true", encode(&calendar.remote_id), encode(uid));
    let (status, reply) = google.request(reqwest::Method::GET, &url, None, None).await?;
    if !(200..300).contains(&status) {
        return Err(failure(status, &reply));
    }
    let page: Value = serde_json::from_slice(&reply).map_err(|e| Error::Other(format!("Google Calendar reply: {e}")))?;
    let Some(item) = page["items"].as_array().and_then(|items| items.iter().find(|i| i["recurringEventId"].is_null())) else { return Ok(None) };
    let raw = serde_json::to_vec(item).expect("JSON serializes");
    Ok(Some((version(google, calendar, &raw, alarms)?, item["status"] == "cancelled")))
}

/// The event as Google has it, None once deleted: Google keeps a deleted
/// event as cancelled.
async fn fetch(google: &impl Http, calendar: &Calendar, remote_id: &str, alarms: &[Alarm]) -> Result<Option<Version>> {
    let url = format!("{BASE}/calendars/{}/events/{}", encode(&calendar.remote_id), encode(remote_id));
    let (status, reply) = google.request(reqwest::Method::GET, &url, None, None).await?;
    match status {
        200..300 => {
            let v = version(google, calendar, &reply, alarms)?;
            let cancelled = serde_json::from_slice::<Value>(&reply).is_ok_and(|e| e["status"] == "cancelled");
            Ok((!cancelled).then_some(v))
        }
        404 | 410 => Ok(None),
        _ => Err(failure(status, &reply)),
    }
}

/// Google's answer as the event to store. An event on the calendar's
/// default reminders gets `alarms`, the ones it had: the event alone
/// doesn't say what the defaults are, and the sync that follows does.
fn version(google: &impl Http, calendar: &Calendar, reply: &[u8], alarms: &[Alarm]) -> Result<Version> {
    let g: GoogleEvent = serde_json::from_slice(reply).map_err(|e| Error::Other(format!("Google Calendar reply: {e}")))?;
    let defaults: Vec<Reminder> = alarms
        .iter()
        .filter_map(|a| match a.trigger {
            AlarmTrigger::MinutesBefore(minutes) => Some(Reminder { method: if a.email { "email" } else { "popup" }.into(), minutes }),
            AlarmTrigger::At(_) => None,
        })
        .collect();
    let uid = g.ical_uid.clone().ok_or_else(|| Error::Other("Google Calendar returned an event without its UID.".into()))?;
    let zone = calendar.zone.as_deref().and_then(tz::zone);
    let me = [google.me().to_string()];
    let data = event(&g, uid, &defaults, zone, &me).ok_or_else(|| Error::Other("Google Calendar returned an event without a start.".into()))?;
    Ok(Version { remote_id: g.id, etag: g.etag, events: vec![data], raw: Some(reply.to_vec()), instances: Vec::new() })
}

/// `events.insert`'s body: the UID and every field the event sets. Google
/// refuses an id of Jolta's next to the UID ("Invalid resource id value",
/// seen on the Google test account, 2026-11-07), so it picks the id. A
/// series' changed occurrences are writes of their own.
pub(crate) fn insert_body(events: &[EventData], extra: Option<&crate::calendar::ops::Extra>, request: &str, here: Tz) -> Result<Value> {
    let [e] = events else { return Err(cannot(GOOGLE, "take a repeating event with changed occurrences from Jolta yet")) };
    let mut v = json!({ "iCalUID": e.uid });
    for field in created_fields(e) {
        set(&mut v, field, e, request, here, true)?;
    }
    merge_json(&mut v, extra, GOOGLE)?;
    Ok(v)
}

/// `events.patch`'s body: only the fields that changed from `base`.
pub(crate) fn patch_body(base: &EventData, e: &EventData, extra: Option<&crate::calendar::ops::Extra>, request: &str, here: Tz) -> Result<Value> {
    let mut v = json!({});
    for field in changed_fields(base, e) {
        set(&mut v, field, e, request, here, false)?;
    }
    merge_json(&mut v, extra, GOOGLE)?;
    Ok(v)
}

/// One guest as Google takes it. A room is a resource. Google replaces the
/// whole list on a patch, so each guest keeps the answer Jolta last read;
/// a guest who answered since changed the ETag, and the write meets a 412.
fn attendee(a: &crate::calendar::Attendee) -> Value {
    let mut v = json!({ "email": a.person.address, "responseStatus": match a.response {
        Response::Accepted => "accepted",
        Response::Tentative => "tentative",
        Response::Declined => "declined",
        Response::NeedsAction | Response::Delegated => "needsAction",
    } });
    if !a.person.name.is_empty() {
        v["displayName"] = json!(a.person.name);
    }
    match a.role {
        Role::Optional => v["optional"] = json!(true),
        Role::Room | Role::Resource => v["resource"] = json!(true),
        Role::Required | Role::Chair | Role::Fyi => {}
    }
    if !a.comment.is_empty() {
        v["comment"] = json!(a.comment);
    }
    v
}

/// The id of the conference request a write of the object `id` at the
/// version `etag` makes: every try of one op sends the same, so Google
/// makes one Meet, and a later request on the same event is a new one.
fn request_id(id: &str, etag: &str) -> String {
    format!("{id}-{}", etag.chars().filter(char::is_ascii_alphanumeric).collect::<String>())
}

/// The query Google needs to read `conferenceData` in a request body; a
/// body without it leaves the event's conference as it is.
fn conference_version(body: &Value) -> &'static str {
    if body.get("conferenceData").is_some() { "&conferenceDataVersion=1" } else { "" }
}

/// Google's answer once the Meet a request asked for is made. Google may
/// answer `pending`; the event is then read again every 2 seconds for up
/// to 20 seconds, and a Meet still pending after that comes with a later
/// sync. The test accounts answered `success` at once (C3 spec, section 2).
async fn settled(google: &impl Http, calendar: &Calendar, body: &Value, mut reply: Vec<u8>) -> Result<Vec<u8>> {
    if body.get("conferenceData").is_none() {
        return Ok(reply);
    }
    for _ in 0..10 {
        let now: Value = serde_json::from_slice(&reply).unwrap_or_default();
        let (Some("pending"), Some(id)) = (now.pointer("/conferenceData/createRequest/status/statusCode").and_then(Value::as_str), now["id"].as_str()) else { break };
        google.pause(std::time::Duration::from_secs(2)).await;
        let url = format!("{BASE}/calendars/{}/events/{}", encode(&calendar.remote_id), encode(id));
        let (status, again) = google.request(reqwest::Method::GET, &url, None, None).await?;
        if !(200..300).contains(&status) {
            break;
        }
        reply = again;
    }
    Ok(reply)
}

/// Puts `field` of `e` into the body the way Google takes it; a Meet asked
/// for goes with `request` as its request id.
fn set(v: &mut Value, field: Field, e: &EventData, request: &str, here: Tz, creating: bool) -> Result<()> {
    match field {
        Field::Time => {
            v["start"] = when(&e.span, true, here);
            v["end"] = when(&e.span, false, here);
        }
        Field::Repeat => {
            let lines: Vec<String> = e
                .recurrence
                .iter()
                .flat_map(|r| {
                    r.rules
                        .iter()
                        .map(|rule| format!("RRULE:{rule}"))
                        .chain(r.exdates.iter().map(|m| Prop::moment("EXDATE", m).to_string()))
                        .chain(r.rdates.iter().map(|m| Prop::moment("RDATE", m).to_string()))
                })
                .collect();
            v["recurrence"] = json!(lines);
        }
        Field::Location => v["location"] = json!(e.location),
        Field::Title => v["summary"] = json!(e.title),
        Field::Description => v["description"] = json!(e.description),
        Field::Status => {
            v["status"] = json!(match e.status {
                EventStatus::Confirmed => "confirmed",
                EventStatus::Tentative => "tentative",
                EventStatus::Cancelled => return Err(cannot(GOOGLE, "cancel an event by an edit; delete it instead")),
            })
        }
        Field::Alarms => {
            if e.alarms.len() > 5 {
                return Err(cannot(GOOGLE, "hold more than 5 alerts on one event"));
            }
            let overrides = e
                .alarms
                .iter()
                .map(|a| match a.trigger {
                    AlarmTrigger::MinutesBefore(m) if m >= 0 => Ok(json!({ "method": if a.email { "email" } else { "popup" }, "minutes": m })),
                    _ => Err(cannot(GOOGLE, "alert at a set time or after the start")),
                })
                .collect::<Result<Vec<_>>>()?;
            v["reminders"] = json!({ "useDefault": false, "overrides": overrides });
        }
        Field::Color => {
            v["colorId"] = match &e.color {
                None => Value::Null,
                Some(hex) => json!(super::color_id(hex).ok_or_else(|| cannot(GOOGLE, "show an event in a color outside its own 11"))?),
            }
        }
        Field::ShowAs => {
            v["transparency"] = json!(match e.show_as {
                ShowAs::Free => "transparent",
                ShowAs::Busy => "opaque",
                // An out-of-office event Google lists comes back as one only
                // through its own `eventType`, passed as an extra field.
                _ if creating => "opaque",
                _ => return Err(cannot(GOOGLE, "show an event as anything but free or busy")),
            })
        }
        Field::Visibility => {
            v["visibility"] = json!(match e.visibility {
                Visibility::Default => "default",
                Visibility::Public => "public",
                Visibility::Private => "private",
                Visibility::Confidential => "confidential",
            })
        }
        Field::Categories => return Err(cannot(GOOGLE, "hold categories")),
        Field::People => v["attendees"] = json!(e.attendees.iter().map(attendee).collect::<Vec<_>>()),
        Field::Options => {
            v["guestsCanSeeOtherGuests"] = json!(!e.options.hide_guests);
            v["guestsCanInviteOthers"] = json!(e.options.guests_invite);
            v["guestsCanModify"] = json!(e.options.guests_modify);
        }
        Field::MeetingLink => match (e.video, &e.meeting) {
            (Some(MeetingProvider::Meet), None) => {
                v["conferenceData"] = json!({ "createRequest": { "requestId": request, "conferenceSolutionKey": { "type": "hangoutsMeet" } } })
            }
            (Some(_), None) => return Err(cannot(GOOGLE, "make this video call; only Google Meet")),
            // A new event's meeting stays in its description.
            _ if creating => {}
            _ => return Err(cannot(GOOGLE, "change or remove a video call from Jolta yet")),
        },
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
    use crate::calendar::ops::Extra;
    use crate::calendar::rsvp::Stored;
    use crate::calendar::{Access, AutoDecline, CalendarId, CalendarSource, EventId, EventKind, Local, Span, Zone};

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn standup() -> EventData {
        EventData { title: "Standup".into(), ..EventData::new("u1@jolta.app", Span::Timed { start: at("2026-11-24 09:00"), end: at("2026-11-24 09:30") }) }
    }

    #[test]
    fn the_stored_id_is_the_one_google_gives_the_uid() {
        assert_eq!(event_id("probe-u2@jolta.app"), "_e1p6uoj55lqj4g3adtm78o9ec5o70", "as Google named it");
    }

    #[test]
    fn a_create_sends_the_uid_and_what_the_event_sets() {
        let e = EventData { location: "Room 1".into(), show_as: ShowAs::Free, ..standup() };
        let body = insert_body(std::slice::from_ref(&e), None, "r", Brussels).unwrap();
        assert_eq!(
            body,
            json!({
                "iCalUID": "u1@jolta.app",
                "summary": "Standup",
                "location": "Room 1",
                "start": { "dateTime": "2026-11-24T09:00:00", "timeZone": "Europe/Brussels" },
                "end": { "dateTime": "2026-11-24T09:30:00", "timeZone": "Europe/Brussels" },
                "transparency": "transparent",
            })
        );
    }

    /// U11: the personal Zoom link goes on and off in the location and
    /// description alone; conference data takes outside providers only
    /// through a Workspace add-on.
    #[test]
    fn a_personal_zoom_link_patches_the_location_and_description() {
        let link = "https://zoom.us/my/jane.doe";
        let block = format!("Join Zoom meeting: {link}");
        let location = format!("Zoom: {link}");
        let zoom = EventData { meeting: crate::calendar::meeting::detect(&[], None, &location, &block), location: location.clone(), description: block.clone(), ..standup() };
        assert_eq!(patch_body(&standup(), &zoom, None, "r", Brussels).unwrap(), json!({ "location": location, "description": block }));
        assert_eq!(patch_body(&zoom, &standup(), None, "r", Brussels).unwrap(), json!({ "location": "", "description": "" }));
        assert!(insert_body(std::slice::from_ref(&zoom), None, "r", Brussels).unwrap().get("conferenceData").is_none());
    }

    #[test]
    fn a_patch_sends_only_the_changed_fields() {
        let base = standup();
        let moved = EventData { title: "Daily".into(), span: Span::Timed { start: at("2026-11-24 10:00"), end: at("2026-11-24 10:15") }, ..base.clone() };
        let body = patch_body(&base, &moved, None, "r", Brussels).unwrap();
        assert_eq!(
            body,
            json!({
                "summary": "Daily",
                "start": { "dateTime": "2026-11-24T10:00:00", "timeZone": "Europe/Brussels" },
                "end": { "dateTime": "2026-11-24T10:15:00", "timeZone": "Europe/Brussels" },
            })
        );
        let alerted = EventData { alarms: vec![Alarm { trigger: AlarmTrigger::MinutesBefore(10), email: false }], ..base.clone() };
        assert_eq!(patch_body(&base, &alerted, None, "r", Brussels).unwrap(), json!({ "reminders": { "useDefault": false, "overrides": [{ "method": "popup", "minutes": 10 }] } }));
        assert!(patch_body(&base, &EventData { show_as: ShowAs::OutOfOffice, ..base.clone() }, None, "r", Brussels).is_err(), "Google shows free or busy");
        assert!(patch_body(&base, &EventData { categories: vec!["Work".into()], ..base.clone() }, None, "r", Brussels).is_err());
    }

    /// C4 creates out of office through `extra`; the fields Jolta models
    /// stay its own.
    #[test]
    fn extra_fields_reach_the_body_and_never_replace_a_modelled_one() {
        let extra = Extra::Json(
            json!({
                "eventType": "outOfOffice",
                "outOfOfficeProperties": { "autoDeclineMode": "declineOnlyNewConflictingInvitations", "declineMessage": "Away" },
                "summary": "Not this",
                "transparency": "transparent",
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        let e = EventData { title: "Out of office".into(), show_as: ShowAs::OutOfOffice, ..standup() };
        let body = insert_body(std::slice::from_ref(&e), Some(&extra), "r", Brussels).unwrap();
        assert_eq!(body["eventType"], "outOfOffice");
        assert_eq!(body["outOfOfficeProperties"]["declineMessage"], "Away", "passed through untouched");
        assert_eq!((body["summary"].as_str(), body["transparency"].as_str()), (Some("Out of office"), Some("opaque")), "the title and show as are Jolta's");
        assert!(insert_body(std::slice::from_ref(&e), Some(&Extra::Lines(vec!["X-A:1".into()])), "r", Brussels).is_err());
    }

    struct Fake {
        replies: Mutex<VecDeque<(u16, Value)>>,
        seen: Mutex<Vec<String>>,
        bodies: Mutex<Vec<Value>>,
    }

    impl Fake {
        fn new(replies: Vec<(u16, Value)>) -> Fake {
            Fake { replies: Mutex::new(replies.into()), seen: Mutex::new(Vec::new()), bodies: Mutex::new(Vec::new()) }
        }
    }

    impl Http for Fake {
        async fn request(&self, method: reqwest::Method, url: &str, body: Option<&Value>, if_match: Option<&str>) -> Result<(u16, Vec<u8>)> {
            self.bodies.lock().unwrap().extend(body.cloned());
            let path = url.trim_start_matches(BASE);
            self.seen.lock().unwrap().push(format!("{method} {path}{}", if_match.map(|e| format!(" If-Match {e}")).unwrap_or_default()));
            let (status, body) = self.replies.lock().unwrap().pop_front().expect("a reply");
            Ok((status, body.to_string().into_bytes()))
        }

        fn me(&self) -> &str {
            "me@example.com"
        }

        async fn pause(&self, time: std::time::Duration) {
            self.seen.lock().unwrap().push(format!("WAIT {}s", time.as_secs()));
        }
    }

    #[tokio::test]
    async fn an_account_without_status_events_gets_a_busy_event() {
        let refused = json!({ "error": { "code": 400, "message": "Organizer is not an enterprise account.",
                                         "errors": [{ "reason": "malformedOutOfOfficeEvent" }] } });
        let made = json!({ "id": "e1", "etag": "\"1\"", "iCalUID": "u1@jolta.app", "status": "confirmed", "summary": "Out of office",
                           "start": { "dateTime": "2026-11-24T09:00:00+01:00" }, "end": { "dateTime": "2026-11-24T09:30:00+01:00" } });
        let fake = Fake::new(vec![(400, refused), (200, made)]);
        let extra = Extra::Json(json!({ "eventType": "outOfOffice", "outOfOfficeProperties": { "autoDeclineMode": "declineNone" },
                                        "extendedProperties": { "private": { "joltaHold": "away" } } }).as_object().unwrap().clone());
        let e = EventData { title: "Out of office".into(), show_as: ShowAs::OutOfOffice, ..standup() };
        let written = send(&fake, &calendar(), &OpWrite::Create { events: std::slice::from_ref(&e), key: "k", extra: Some(&extra) }, Tell::NOBODY, Brussels).await.unwrap();
        let Written::Saved(Some(v)) = written else { panic!("saved") };
        assert_eq!(v.events[0].show_as, ShowAs::Busy);
        let bodies = fake.bodies.lock().unwrap().clone();
        assert_eq!(bodies[0]["eventType"], "outOfOffice");
        assert!(bodies[1].get("eventType").is_none() && bodies[1].get("outOfOfficeProperties").is_none(), "{}", bodies[1]);
        assert_eq!((bodies[1]["transparency"].as_str(), bodies[1]["extendedProperties"]["private"]["joltaHold"].as_str()), (Some("opaque"), Some("away")));

        let other = Fake::new(vec![(400, json!({ "error": { "errors": [{ "reason": "invalid" }] } }))]);
        assert!(send(&other, &calendar(), &OpWrite::Create { events: std::slice::from_ref(&e), key: "k", extra: Some(&extra) }, Tell::NOBODY, Brussels).await.is_err());
        assert_eq!(other.bodies.lock().unwrap().len(), 1, "any other 400 is a failure");
    }

    /// Focus time as `focus::plan` makes it, refused with the answer the
    /// Google Workspace test account gave (2026-11-07).
    #[tokio::test]
    async fn focus_time_an_account_lacks_becomes_a_busy_event() {
        let refused = json!({ "error": { "code": 400, "message": "Organizer is not an enterprise account, or does not have the focus time feature enabled.",
                                         "errors": [{ "domain": "calendar", "reason": "malformedFocusTimeEvent" }] } });
        let made = json!({ "id": "e1", "etag": "\"1\"", "iCalUID": "u1@jolta.app", "status": "confirmed", "summary": "Focus time",
                           "start": { "dateTime": "2026-11-18T14:00:00+01:00" }, "end": { "dateTime": "2026-11-18T16:00:00+01:00" } });
        let fake = Fake::new(vec![(400, refused), (200, made)]);
        let request = crate::calendar::focus::Request { from: chrono::Utc::now(), until: chrono::Utc::now() + chrono::Duration::hours(2), declines: AutoDecline::New, do_not_disturb: true };
        let planned = crate::calendar::focus::plan(crate::calendar::focus::Holds::Status, &request, Brussels);
        let e = EventData { uid: "u1@jolta.app".into(), ..planned.event };
        let written = send(&fake, &calendar(), &OpWrite::Create { events: std::slice::from_ref(&e), key: "k", extra: planned.extra.as_ref() }, Tell::NOBODY, Brussels).await.unwrap();
        let Written::Saved(Some(v)) = written else { panic!("saved") };
        assert_eq!((v.events[0].show_as, &v.events[0].kind), (ShowAs::Busy, &EventKind::Default), "what focus::landed reads as refused");
        let bodies = fake.bodies.lock().unwrap().clone();
        assert_eq!((bodies[0]["eventType"].as_str(), bodies[0]["focusTimeProperties"]["chatStatus"].as_str()), (Some("focusTime"), Some("doNotDisturb")));
        assert!(bodies[1].get("eventType").is_none() && bodies[1].get("focusTimeProperties").is_none(), "{}", bodies[1]);
        assert_eq!((bodies[1]["summary"].as_str(), bodies[1]["transparency"].as_str()), (Some("Focus time"), Some("opaque")));
    }

    fn calendar() -> Calendar {
        Calendar {
            id: CalendarId(1),
            account_id: None,
            source: CalendarSource::Google,
            remote_id: "primary".into(),
            name: "Work".into(),
            color: None,
            zone: Some("Europe/Brussels".into()),
            access: Access::Owner,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: true,
            cursor: None,
            refresh: None,
            synced: None,
            video: None,
        }
    }

    fn google(id: &str, etag: &str, uid: &str, status: &str) -> Value {
        json!({ "id": id, "etag": etag, "iCalUID": uid, "status": status, "summary": "Standup",
                "start": { "dateTime": "2026-11-24T09:00:00+01:00", "timeZone": "Europe/Brussels" },
                "end": { "dateTime": "2026-11-24T09:30:00+01:00", "timeZone": "Europe/Brussels" } })
    }

    async fn sent(write: OpWrite<'_>, replies: Vec<(u16, Value)>) -> (Result<Written>, Vec<String>) {
        told(write, Tell::NOBODY, replies).await
    }

    async fn told(write: OpWrite<'_>, tell: Tell<'_>, replies: Vec<(u16, Value)>) -> (Result<Written>, Vec<String>) {
        let fake = Fake::new(replies);
        let done = send(&fake, &calendar(), &write, tell, Brussels).await;
        (done, fake.seen.into_inner().unwrap())
    }

    /// Check 4: a create retried after its first try reached Google keeps
    /// that event as it is now and makes no second one; a UID deleted
    /// before comes back through import.
    #[tokio::test]
    async fn a_retried_create_takes_the_event_its_first_try_made() {
        let e = standup();
        let id = event_id("u1@jolta.app");
        let create = OpWrite::Create { events: std::slice::from_ref(&e), key: &id, extra: None };
        let listed = |status: &str| json!({ "items": [google(&id, "\"5\"", "u1@jolta.app", status)] });
        let (done, seen) = sent(create, vec![(409, json!({})), (200, listed("confirmed"))]).await;
        let Written::Saved(Some(v)) = done.unwrap() else { panic!("saved") };
        assert_eq!((v.remote_id.as_str(), v.etag.as_deref(), v.events[0].title.as_str()), (id.as_str(), Some("\"5\""), "Standup"));
        assert_eq!(seen, ["POST /calendars/primary/events?sendUpdates=none", "GET /calendars/primary/events?iCalUID=u1%40jolta.app&showDeleted=true"]);

        let (done, seen) = sent(create, vec![(409, json!({})), (200, listed("cancelled")), (200, google(&id, "\"6\"", "u1@jolta.app", "confirmed"))]).await;
        assert!(matches!(done.unwrap(), Written::Saved(Some(Version { etag: Some(e), .. })) if e == "\"6\""));
        assert_eq!(seen.last().unwrap(), "POST /calendars/primary/events/import", "a deleted UID comes back, telling nobody");

        let (done, _) = sent(create, vec![(409, json!({})), (200, json!({ "items": [] }))]).await;
        assert!(done.is_err(), "a UID Google won't show is never taken as this event");
    }

    fn with_meet(event: Value, status: &str) -> Value {
        let mut v = event;
        v["conferenceData"] = json!({
            "createRequest": { "requestId": "any", "conferenceSolutionKey": { "type": "hangoutsMeet" }, "status": { "statusCode": status } },
            "entryPoints": if status == "success" { json!([{ "entryPointType": "video", "uri": "https://meet.google.com/abc-defg-hij" }]) } else { json!([]) },
        });
        v
    }

    /// Add video call on Google: the insert asks for a Meet under the op's
    /// key, and a retry asks under the same key, so Google makes one
    /// conference (check 4). A Meet still pending is read again.
    #[tokio::test]
    async fn a_meet_is_asked_for_once_per_op() {
        let e = EventData { video: Some(MeetingProvider::Meet), ..standup() };
        let id = event_id("u1@jolta.app");
        let create = OpWrite::Create { events: std::slice::from_ref(&e), key: &id, extra: None };
        let ask = |fake: &Fake| fake.bodies.lock().unwrap()[0]["conferenceData"].clone();
        let first = Fake::new(vec![(500, json!({}))]);
        assert!(send(&first, &calendar(), &create, Tell::NOBODY, Brussels).await.is_err(), "the first try's answer is lost");
        let again = Fake::new(vec![(200, with_meet(google(&id, "\"5\"", "u1@jolta.app", "confirmed"), "success"))]);
        let Written::Saved(Some(v)) = send(&again, &calendar(), &create, Tell::NOBODY, Brussels).await.unwrap() else { panic!("saved") };
        assert_eq!(ask(&first), json!({ "createRequest": { "requestId": id, "conferenceSolutionKey": { "type": "hangoutsMeet" } } }));
        assert_eq!(ask(&first), ask(&again), "the retry asks under the same request id");
        assert_eq!(again.seen.lock().unwrap()[0], "POST /calendars/primary/events?sendUpdates=none&conferenceDataVersion=1");
        assert_eq!(v.events[0].meeting.as_ref().map(|m| m.join_url.as_str()), Some("https://meet.google.com/abc-defg-hij"), "the join link Jolta reads back");
        assert_eq!(v.events[0].video, None, "the answer replaces the request");

        let made = with_meet(google(&id, "\"6\"", "u1@jolta.app", "confirmed"), "success");
        let (done, seen) = sent(create, vec![(200, with_meet(google(&id, "\"5\"", "u1@jolta.app", "confirmed"), "pending")), (200, made)]).await;
        assert!(matches!(done.unwrap(), Written::Saved(Some(Version { etag: Some(e), .. })) if e == "\"6\""));
        assert_eq!(&seen[1..], ["WAIT 2s".to_string(), format!("GET /calendars/primary/events/{id}")], "a pending Meet is read again");

        let pending = || with_meet(google(&id, "\"5\"", "u1@jolta.app", "confirmed"), "pending");
        let (done, seen) = sent(create, std::iter::repeat_with(|| (200, pending())).take(11).collect()).await;
        assert!(done.is_ok() && seen.iter().filter(|s| s.starts_with("WAIT")).count() == 10, "it waits 20 seconds at most");

        let plain = Fake::new(vec![(200, google(&id, "\"5\"", "u1@jolta.app", "confirmed"))]);
        send(&plain, &calendar(), &OpWrite::Create { events: &[standup()], key: &id, extra: None }, Tell::NOBODY, Brussels).await.unwrap();
        assert_eq!(plain.seen.lock().unwrap()[0], "POST /calendars/primary/events?sendUpdates=none", "no conference version without a call");
    }

    /// A Meet added to an event already on Google: its request id comes
    /// from the version the PATCH writes over, so adding one again later is
    /// a new request.
    #[tokio::test]
    async fn a_meet_added_later_is_asked_for_against_the_held_version() {
        async fn ask(etag: &str) -> (Value, String) {
            let base = standup();
            let mine = EventData { video: Some(MeetingProvider::Meet), ..base.clone() };
            let held = Stored { id: EventId(1), remote_id: "abc".into(), etag: Some(etag.into()), raw: None, data: base.clone() };
            let fake = Fake::new(vec![(412, json!({}))]);
            send(&fake, &calendar(), &OpWrite::Update { held: &held, base: &base, event: &mine, extra: None, dropped: &[] }, Tell::NOBODY, Brussels).await.unwrap();
            let line = fake.seen.lock().unwrap()[0].clone();
            (fake.bodies.lock().unwrap()[0].clone(), line)
        }
        let (body, line) = ask("\"3\"").await;
        assert_eq!(body["conferenceData"]["createRequest"]["requestId"], "abc-3");
        assert_eq!(line, "PATCH /calendars/primary/events/abc?sendUpdates=none&conferenceDataVersion=1 If-Match \"3\"");
        assert_ne!(ask("\"7\"").await.0, body, "another version, another request");
        assert!(insert_body(&[EventData { video: Some(MeetingProvider::Teams), ..standup() }], None, "r", Brussels).is_err(), "Google makes Meet only");
    }

    #[tokio::test]
    async fn every_patch_and_delete_carries_the_held_etag() {
        let base = standup();
        let held = Stored { id: EventId(1), remote_id: "abc".into(), etag: Some("\"3\"".into()), raw: None, data: base.clone() };
        let mine = EventData { title: "Daily".into(), ..base.clone() };
        let update = OpWrite::Update { held: &held, base: &base, event: &mine, extra: None, dropped: &[] };
        for (status, expected) in [(412, Written::Stale), (404, Written::Gone), (410, Written::Gone)] {
            let (done, seen) = sent(update, vec![(status, json!({}))]).await;
            assert_eq!(done.unwrap(), expected, "{status}");
            assert_eq!(seen, ["PATCH /calendars/primary/events/abc?sendUpdates=none If-Match \"3\""]);
        }
        let (done, _) = sent(update, vec![(200, google("abc", "\"4\"", "u1@jolta.app", "confirmed"))]).await;
        assert!(matches!(done.unwrap(), Written::Saved(Some(Version { etag: Some(e), .. })) if e == "\"4\""));

        let (done, seen) = sent(OpWrite::Delete { held: &held }, vec![(204, Value::Null)]).await;
        assert_eq!((done.unwrap(), seen), (Written::Saved(None), vec!["DELETE /calendars/primary/events/abc?sendUpdates=none If-Match \"3\"".to_string()]));

        let unread = Stored { etag: None, ..held.clone() };
        let (done, seen) = sent(OpWrite::Delete { held: &unread }, vec![]).await;
        assert_eq!((done.unwrap(), seen.len()), (Written::Stale, 0), "nothing goes out without a version to guard it");
    }

    /// `events.move` names no version to hold it to, so the event is read
    /// first; one an earlier try moved is found in the other calendar
    /// (check 4).
    #[tokio::test]
    async fn a_move_reads_the_event_first_and_finds_one_moved_before() {
        let held = Stored { id: EventId(1), remote_id: "abc".into(), etag: Some("\"3\"".into()), raw: None, data: standup() };
        let to = Calendar { remote_id: "home".into(), ..calendar() };
        let moving = OpWrite::Move { held: &held, to: &to };
        let now = google("abc", "\"3\"", "u1@jolta.app", "confirmed");
        let (done, seen) = sent(moving, vec![(200, now.clone()), (200, google("abc", "\"4\"", "u1@jolta.app", "confirmed"))]).await;
        assert!(matches!(done.unwrap(), Written::Saved(Some(Version { etag: Some(e), .. })) if e == "\"4\""));
        assert_eq!(seen, ["GET /calendars/primary/events/abc", "POST /calendars/primary/events/abc/move?destination=home&sendUpdates=none If-Match \"3\""]);

        let (done, seen) = sent(moving, vec![(200, google("abc", "\"9\"", "u1@jolta.app", "confirmed"))]).await;
        assert_eq!((done.unwrap(), seen.len()), (Written::Stale, 1), "changed since: nothing moves");

        let (done, seen) = sent(moving, vec![(404, json!({})), (200, google("abc", "\"4\"", "u1@jolta.app", "confirmed"))]).await;
        assert!(matches!(done.unwrap(), Written::Saved(Some(_))), "the first try moved it");
        assert_eq!(seen, ["GET /calendars/primary/events/abc", "GET /calendars/home/events/abc"]);
        let (done, _) = sent(moving, vec![(200, google("abc", "\"3\"", "u1@jolta.app", "cancelled")), (404, json!({}))]).await;
        assert_eq!(done.unwrap(), Written::Gone, "deleted, and in neither calendar");
    }

    #[tokio::test]
    async fn a_deleted_event_fetches_as_gone() {
        let fake = Fake::new(vec![(200, google("abc", "\"9\"", "u1@jolta.app", "cancelled"))]);
        assert_eq!(fetch(&fake, &calendar(), "abc", &[]).await.unwrap(), None);
    }
    /// The op's notify picks `sendUpdates`, and a patch of the guests sends
    /// the whole list with each guest's answer, a room as a resource.
    #[tokio::test]
    async fn guests_and_send_updates_follow_the_op() {
        let base = standup();
        let guest = |address: &str, role, response| crate::calendar::Attendee {
            person: crate::calendar::Person { address: address.into(), name: String::new() },
            role,
            response,
            comment: String::new(),
            is_me: false,
        };
        let mut e = base.clone();
        e.attendees = vec![guest("ann@example.com", Role::Optional, Response::Accepted), guest("room@example.com", Role::Room, Response::NeedsAction)];
        e.options.guests_invite = false;
        let body = patch_body(&base, &e, None, "r", Brussels).unwrap();
        assert_eq!(
            body["attendees"],
            json!([
                { "email": "ann@example.com", "responseStatus": "accepted", "optional": true },
                { "email": "room@example.com", "responseStatus": "needsAction", "resource": true },
            ])
        );
        assert_eq!((body["guestsCanInviteOthers"].as_bool(), body["guestsCanSeeOtherGuests"].as_bool()), (Some(false), Some(true)));

        let held = Stored { id: EventId(1), remote_id: "abc".into(), etag: Some("\"3\"".into()), raw: None, data: base.clone() };
        let update = OpWrite::Update { held: &held, base: &base, event: &e, extra: None, dropped: &[] };
        for (notify, query) in [(Notify::All, "all"), (Notify::Changed, "all"), (Notify::Outside, "externalOnly"), (Notify::Nobody, "none")] {
            let (_, seen) = told(update, Tell { notify, message: None }, vec![(412, json!({}))]).await;
            assert_eq!(seen, [format!("PATCH /calendars/primary/events/abc?sendUpdates={query} If-Match \"3\"")], "{notify:?}");
        }
        let (_, seen) = told(OpWrite::Delete { held: &held }, Tell { notify: Notify::All, message: Some("ignored") }, vec![(204, Value::Null)]).await;
        assert_eq!(seen, ["DELETE /calendars/primary/events/abc?sendUpdates=all If-Match \"3\""]);
    }

}
