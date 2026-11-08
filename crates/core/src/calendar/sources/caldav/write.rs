//! Calendar ops on CalDAV: a new resource PUT with `If-None-Match: *` at a
//! name of Jolta's, an edit written into the server's own text through
//! `ical_edit` so every line Jolta doesn't model stays byte for byte, and a
//! DELETE, the edit and the delete guarded by the resource's ETag.

use chrono_tz::Tz;
use reqwest::Url;

use super::super::{OpWrite, Tell, Version, Written, cannot, changed_fields};
use super::add::resource_name;
use super::{CalDav, href_key, join, path_escape};
use crate::calendar::change::Field;
use crate::calendar::ical_edit::{Component, Object, Prop, utc_stamp, vevent, write_event, write_field};
use crate::calendar::ops::{Extra, Notify};
use crate::calendar::{Calendar, EventData, Person, ical, organizer, rsvp, tz};
use crate::{Error, Result};

const PRODID: &str = concat!("-//Jolta//Jolta ", env!("CARGO_PKG_VERSION"), "//EN");

impl CalDav {
    pub async fn write(&self, calendar: &Calendar, write: &OpWrite<'_>, tell: Tell<'_>) -> Result<Written> {
        // iCloud mails a new time or place on its own but not a new title, so
        // Send asks it to tell every guest.
        let send = (tell.notify == Notify::All).then_some(());
        let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        match *write {
            OpWrite::Create { events, key, extra } => {
                let url = join(&collection, &path_escape(key))?;
                let text = organized(&created_text(events, extra)?, &events[0].uid, &self.organizer().await);
                let headers = [("Content-Type", "text/calendar; charset=utf-8"), ("If-None-Match", "*")];
                let reply = self.exchange("PUT", &url, &headers, &text).await?;
                match reply.status {
                    200..300 => Ok(Written::Saved(Some(self.version(key, reply.etag, text).await?))),
                    // A retry whose first try reached the server: the resource is there.
                    412 => match self.fetch(calendar, key).await? {
                        Some(v) if v.events.iter().any(|e| e.uid == events[0].uid) => Ok(Written::Saved(Some(v))),
                        _ => Err(Error::Other("Your calendar already has another event where Jolta would put this one.".into())),
                    },
                    status => Err(Error::Other(format!("CalDAV PUT {url}: {status} {}", reply.body.trim()))),
                }
            }
            OpWrite::Update { held, base, event, extra, dropped } => {
                let (Some(etag), Some(raw)) = (held.etag.as_deref(), held.raw.as_deref()) else { return Ok(Written::Stale) };
                let text = edited_text(&String::from_utf8_lossy(raw), base, event, extra, dropped, tz::here())?;
                let text = organized(&text, &event.uid, &self.organizer().await);
                self.put_over(&collection, held, etag, text, send.map(|_| event.uid.as_str())).await
            }
            OpWrite::Occurrence { series, base, event, extra } => {
                let (Some(etag), Some(raw)) = (series.etag.as_deref(), series.raw.as_deref()) else { return Ok(Written::Stale) };
                let text = edited_text(&String::from_utf8_lossy(raw), base, event, extra, &[], tz::here())?;
                let text = organized(&text, &event.uid, &self.organizer().await);
                self.put_over(&collection, series, etag, text, send.map(|_| event.uid.as_str())).await
            }
            OpWrite::Cancel { held, occurrence, .. } => {
                let (Some(etag), Some(raw)) = (held.etag.as_deref(), held.raw.as_deref()) else { return Ok(Written::Stale) };
                let text = cancelled_text(&String::from_utf8_lossy(raw), occurrence, tz::here())?;
                self.put_over(&collection, held, etag, text, None).await
            }
            OpWrite::Delete { held } => {
                let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
                let url = join(&collection, &path_escape(&held.remote_id))?;
                let reply = self.exchange("DELETE", &url, &[("If-Match", etag)], "").await?;
                match reply.status {
                    200..300 => Ok(Written::Saved(None)),
                    412 => Ok(Written::Stale),
                    404 | 410 => Ok(Written::Gone),
                    status => Err(Error::Other(format!("CalDAV DELETE {url}: {status} {}", reply.body.trim()))),
                }
            }
            OpWrite::Move { held, to } => self.move_to(&collection, held, to).await,
        }
    }

    /// MOVE of the resource `held` into the collection of `to`, under the
    /// same name. iCloud moves a resource whatever ETag If-Match names
    /// (iCloud test account, 2026-11-08), so it is read first: gone, or at
    /// another version, nothing moves. A resource gone because an earlier
    /// try moved it is found in `to`.
    async fn move_to(&self, collection: &Url, held: &rsvp::Stored, to: &Calendar) -> Result<Written> {
        let Some(etag) = held.etag.as_deref() else { return Ok(Written::Stale) };
        let url = join(collection, &path_escape(&held.remote_id))?;
        let dest = moved_path(to, &held.remote_id)?;
        let dest_url = join(&Url::parse(&to.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", to.remote_id)))?, &path_escape(&dest))?;
        let moved_before = || async {
            Ok(match self.fetch(to, &dest).await? {
                Some(v) if v.events.iter().any(|e| e.uid == held.data.uid) => Written::Saved(Some(v)),
                _ => Written::Gone,
            })
        };
        let now = self.exchange("GET", &url, &[], "").await?;
        match now.status {
            404 | 410 => return moved_before().await,
            200..300 if now.etag.as_deref() == Some(etag) => {}
            200..300 => return Ok(Written::Stale),
            status => return Err(Error::Other(format!("CalDAV GET {url}: {status}"))),
        }
        let headers = [("Destination", dest_url.as_str()), ("Overwrite", "F"), ("If-Match", etag)];
        let reply = self.exchange("MOVE", &url, &headers, "").await?;
        match reply.status {
            200..300 => match self.fetch(to, &dest).await? {
                Some(v) => Ok(Written::Saved(Some(v))),
                None => Err(Error::Other(format!("CalDAV MOVE {url}: the server lost the event"))),
            },
            412 => Ok(Written::Stale),
            404 | 410 => moved_before().await,
            status => Err(Error::Other(format!("CalDAV MOVE {url}: {status} {}", reply.body.trim()))),
        }
    }

    /// The user as the organizer of a meeting made here: the account's
    /// address when the server lists it among the principal's, else the
    /// first the server lists, which a scheduling server takes as its own.
    async fn organizer(&self) -> Person {
        let me = self.me().await;
        let address = me.iter().find(|a| a.eq_ignore_ascii_case(&self.address)).or(me.first()).cloned().unwrap_or_else(|| self.address.clone());
        Person { address, name: String::new() }
    }

    /// Writes `text` over the resource `held` while it is still at `etag`.
    /// With `force_send`, a meeting's UID, the body sent asks the server to
    /// mail every guest of it (`forced`); the version kept is `text`.
    async fn put_over(&self, collection: &Url, held: &rsvp::Stored, etag: &str, text: String, force_send: Option<&str>) -> Result<Written> {
        let url = join(collection, &path_escape(&held.remote_id))?;
        // iCloud takes a PUT with the ETag of a resource deleted
        // since and makes it again (iCloud test account,
        // 2026-11-07), so the resource is read first: gone, or at
        // another version, nothing is written.
        let now = self.exchange("GET", &url, &[], "").await?;
        match now.status {
            404 | 410 => return Ok(Written::Gone),
            200..300 if now.etag.as_deref() == Some(etag) => {}
            200..300 => return Ok(Written::Stale),
            status => return Err(Error::Other(format!("CalDAV GET {url}: {status}"))),
        }
        let headers = [("Content-Type", "text/calendar; charset=utf-8"), ("If-Match", etag)];
        let body = force_send.map_or_else(|| text.clone(), |uid| forced(&text, uid));
        let reply = self.exchange("PUT", &url, &headers, &body).await?;
        match reply.status {
            200..300 => Ok(Written::Saved(Some(self.version(&held.remote_id, reply.etag, text).await?))),
            412 => Ok(Written::Stale),
            404 | 410 => Ok(Written::Gone),
            status => Err(Error::Other(format!("CalDAV PUT {url}: {status} {}", reply.body.trim()))),
        }
    }

    /// The resource as the server has it, None once it is gone. A server
    /// that gives it no ETag can't have it written over safely, which the
    /// error says.
    pub async fn fetch(&self, calendar: &Calendar, remote_id: &str) -> Result<Option<Version>> {
        let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        let url = join(&collection, &path_escape(remote_id))?;
        match self.versioned(&url, "Change it").await? {
            None => Ok(None),
            Some((text, etag)) => Ok(Some(self.version(remote_id, Some(etag), text).await?)),
        }
    }

    async fn version(&self, remote_id: &str, etag: Option<String>, text: String) -> Result<Version> {
        let events = ical::read(&text, &self.me().await)?;
        Ok(Version { remote_id: remote_id.to_string(), etag, events, raw: Some(text.into_bytes()), instances: Vec::new() })
    }
}

/// Where a new event of `calendar` goes: the path of a resource named from
/// `key`, as a sync names it, so the stored event and the synced one match.
pub(crate) fn resource_path(calendar: &Calendar, key: &str) -> Result<String> {
    let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
    Ok(href_key(join(&collection, &resource_name(key))?.path()))
}

/// Where MOVE puts the resource at `path` in `calendar`: the same name in
/// its collection.
pub(crate) fn moved_path(calendar: &Calendar, path: &str) -> Result<String> {
    let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
    let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or(path);
    Ok(href_key(join(&collection, &path_escape(name))?.path()))
}

pub(crate) fn writable(write: &OpWrite<'_>) -> Result<()> {
    match write {
        OpWrite::Create { events, extra, .. } => created_text(events, *extra).map(drop),
        OpWrite::Update { base, event, extra, .. } | OpWrite::Occurrence { base, event, extra, .. } => {
            lines(*extra)?;
            changed_fields(base, event).into_iter().try_for_each(|f| editable(f))
        }
        OpWrite::Delete { .. } | OpWrite::Cancel { .. } | OpWrite::Move { .. } => Ok(()),
    }
}

/// The RECURRENCE-ID value of the VEVENT of `uid` in `obj` that names `rid`.
fn rid_value(obj: &Object, uid: &str, rid: &crate::calendar::Moment, here: Tz) -> Option<String> {
    obj.calendar()
        .into_iter()
        .flat_map(Component::components)
        .filter(|c| c.name() == "VEVENT" && c.prop("UID").is_some_and(|u| u.text() == uid))
        .find_map(|c| c.prop("RECURRENCE-ID").filter(|p| rsvp::prop_moment(p).is_some_and(|m| m.names(rid, here))))
        .map(|p| p.value().to_string())
}

/// `text` with the occurrence taken out of its series: an EXDATE on the
/// master and its changed VEVENT, if any, gone. The master's SEQUENCE
/// goes up, as for any change to its rule.
pub(crate) fn cancelled_text(text: &str, occurrence: &EventData, here: Tz) -> Result<String> {
    let mut obj = Object::parse(text);
    let rid = occurrence.recurrence_id.as_ref().ok_or_else(|| Error::Other("An occurrence without its start.".into()))?;
    if let Some(value) = rid_value(&obj, &occurrence.uid, rid, here) {
        obj.remove_event(&occurrence.uid, Some(&value));
    }
    let master = obj.event_mut(&occurrence.uid, None).ok_or_else(|| Error::Other("The calendar's copy no longer has this series.".into()))?;
    master.add(Prop::moment("EXDATE", rid));
    let sequence = master.prop("SEQUENCE").and_then(|p| p.value().trim().parse::<i64>().ok()).unwrap_or(0);
    master.set("SEQUENCE", &(sequence + 1).to_string());
    master.set("DTSTAMP", &utc_stamp());
    Ok(obj.to_string())
}

/// The fields Jolta writes into a resource. Video calls wait for the unit
/// that makes meetings.
fn editable(field: Field) -> Result<()> {
    match field {
        Field::MeetingLink => Err(cannot("Jolta", "change the video call on this calendar yet")),
        _ => Ok(()),
    }
}

/// `text` with `organizer` on each VEVENT of `uid` that has guests and no
/// ORGANIZER: a scheduling server invites the guests of a meeting only when
/// one of the account's addresses organizes it (RFC 6638, section 3.2).
pub(crate) fn organized(text: &str, uid: &str, organizer: &Person) -> String {
    let mut obj = Object::parse(text);
    let mut changed = false;
    for event in obj.events_mut(uid) {
        let guests = event.props().any(|p| p.is("ATTENDEE"));
        if guests && event.prop("ORGANIZER").is_none() {
            event.add(Prop::new("ORGANIZER", &format!("mailto:{}", organizer.address)));
            changed = true;
        }
    }
    if changed { obj.to_string() } else { text.to_string() }
}

fn lines(extra: Option<&Extra>) -> Result<Vec<Prop>> {
    match extra {
        None => Ok(Vec::new()),
        Some(Extra::Lines(lines)) => lines.iter().map(|l| Prop::parse(l).ok_or_else(|| Error::Other(format!("{l:?} isn't an iCalendar line")))).collect(),
        Some(Extra::Json(_)) => Err(Error::Other("A CalDAV calendar takes extra fields as iCalendar lines, not JSON".into())),
    }
}

/// Adds each extra line whose property the event doesn't have yet, so a
/// line never replaces what Jolta wrote from the event.
fn add_lines(event: &mut Component, extra: Option<&Extra>) -> Result<()> {
    for line in lines(extra)? {
        if event.prop(line.name()).is_none() {
            event.add(line);
        }
    }
    Ok(())
}

/// A new resource: the series or single event, its changed occurrences
/// after it, and the extra lines on the first.
pub(crate) fn created_text(events: &[EventData], extra: Option<&Extra>) -> Result<String> {
    let master = events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()).ok_or_else(|| Error::Other("nothing to create".into()))?;
    let mut obj = Object::parse(&write_event(master, PRODID));
    for e in events.iter().filter(|e| e.recurrence_id.is_some() && !std::ptr::eq(*e, master)) {
        obj.add_after_master(vevent(e));
    }
    match obj.event_mut(&master.uid, None) {
        Some(first) => add_lines(first, extra)?,
        // A changed occurrence alone has no master to carry the lines.
        None => lines(extra).map(drop)?,
    }
    Ok(obj.to_string())
}

/// `text`, the resource as the server gave it, with the fields `event`
/// changed from `base` written into the VEVENT it names. Every other line
/// stays as it was. A new time, repeat rule or status raises SEQUENCE, as
/// RFC 5546 section 2.1.4 asks of an organizer's change.
pub(crate) fn edited_text(text: &str, base: &EventData, event: &EventData, extra: Option<&Extra>, dropped: &[EventData], here: Tz) -> Result<String> {
    let mut obj = Object::parse(text);
    for gone in dropped {
        if let Some(value) = gone.recurrence_id.as_ref().and_then(|rid| rid_value(&obj, &gone.uid, rid, here)) {
            obj.remove_event(&gone.uid, Some(&value));
        }
    }
    let rid = match &event.recurrence_id {
        None => None,
        Some(rid) => match rid_value(&obj, &event.uid, rid, here) {
            Some(value) => Some(value),
            // An occurrence changed for the first time gets a VEVENT of
            // its own after the series'.
            None => {
                if obj.event(&event.uid, None).is_none() {
                    return Err(Error::Other("The calendar's copy no longer has this series.".into()));
                }
                let mut changed = vevent(event);
                add_lines(&mut changed, extra)?;
                obj.add_after_master(changed);
                return Ok(obj.to_string());
            }
        },
    };
    let fields = changed_fields(base, event);
    // A change to the whole series reaches each changed occurrence that
    // kept the series' value for that field.
    if rid.is_none() {
        let overrides: Vec<EventData> = ical::read(&obj.to_string(), &[])?.into_iter().filter(|e| e.uid == event.uid && e.recurrence_id.is_some()).collect();
        for o in overrides {
            let carried = crate::calendar::change::carried(base, event, &o);
            let Some(value) = o.recurrence_id.as_ref().and_then(|r| rid_value(&obj, &o.uid, r, here)) else { continue };
            let Some(component) = obj.event_mut(&o.uid, Some(&value)) else { continue };
            for f in changed_fields(&o, &carried) {
                editable(f)?;
                write_field(component, f, &carried);
            }
            if organizer::asks_again(&o, &carried, here) {
                ask_again(component, carried.options.request_responses);
            }
        }
    }
    let component = obj.event_mut(&event.uid, rid.as_deref()).ok_or_else(|| Error::Other("The calendar's copy no longer has this event.".into()))?;
    for &field in &fields {
        editable(field)?;
        write_field(component, field, event);
    }
    if organizer::asks_again(base, event, here) {
        ask_again(component, event.options.request_responses);
    }
    if fields.iter().any(|f| matches!(f, Field::Time | Field::Repeat | Field::Status)) {
        let sequence = component.prop("SEQUENCE").and_then(|p| p.value().trim().parse::<i64>().ok()).unwrap_or(0);
        component.set("SEQUENCE", &(sequence + 1).to_string());
    }
    let now = utc_stamp();
    component.set("DTSTAMP", &now);
    if component.prop("LAST-MODIFIED").is_some() {
        component.set("LAST-MODIFIED", &now);
    }
    add_lines(component, extra)?;
    Ok(obj.to_string())
}

/// `text` with each guest of the meeting `uid` but the organizer marked
/// SCHEDULE-FORCE-SEND=REQUEST (RFC 6638 7.3): the server mails them the
/// change whatever it changed. The server drops the parameter.
fn forced(text: &str, uid: &str) -> String {
    let mut obj = Object::parse(text);
    for component in obj.events_mut(uid) {
        let organizer = component.prop("ORGANIZER").and_then(|p| p.address()).map(str::to_ascii_lowercase);
        if organizer.is_none() {
            continue;
        }
        component.update("ATTENDEE", |p| p.address().map(str::to_ascii_lowercase) != organizer, |p| p.set_param("SCHEDULE-FORCE-SEND", "REQUEST"));
    }
    obj.to_string()
}

/// Every guest's answer back to NEEDS-ACTION, as the organizer's client
/// does for a server that schedules (RFC 6638): the server mails what the
/// copy says. The organizer's own line stays.
fn ask_again(component: &mut Component, rsvp: bool) {
    let organizer = component.prop("ORGANIZER").and_then(|p| p.address()).map(str::to_ascii_lowercase);
    component.update(
        "ATTENDEE",
        |p| p.address().map(str::to_ascii_lowercase) != organizer,
        |p| {
            p.set_param("PARTSTAT", "NEEDS-ACTION");
            if rsvp {
                p.set_param("RSVP", "TRUE");
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;
    use chrono_tz::Europe::Brussels;

    use super::super::tests::caldav_server;
    use super::*;
    use crate::calendar::rsvp::Stored;
    use crate::calendar::{EventId, Local, Moment, Span, Zone};

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    /// A resource another app wrote, with lines Jolta doesn't model: Apple's
    /// structured location, a property with parameters, a folded line.
    const APPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Apple Inc.//macOS 15.0//EN\r\nBEGIN:VEVENT\r\nUID:standup@example.com\r\n\
        DTSTAMP:20261101T120000Z\r\nDTSTART;TZID=Europe/Brussels:20261124T090000\r\nDTEND;TZID=Europe/Brussels:20261124T093000\r\n\
        SUMMARY:Standup\r\nLOCATION:Room 1\r\nX-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-APPLE-RADIUS=70;X-TITLE=\"Room 1\":geo:50.85,4.35\r\n\
        X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\nDESCRIPTION:A long description that someone wrote in Calendar on the Mac and that \r\n folds onto a second line\r\n\
        SEQUENCE:2\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT10M\r\nX-WR-ALARMUID:A1\r\nEND:VALARM\r\nEND:VEVENT\r\n\
        BEGIN:VEVENT\r\nUID:standup@example.com\r\nRECURRENCE-ID;TZID=Europe/Brussels:20261201T090000\r\nDTSTART;TZID=Europe/Brussels:20261201T110000\r\n\
        DTEND;TZID=Europe/Brussels:20261201T113000\r\nSUMMARY:Standup moved\r\nX-CUSTOM;A=1:keep me\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    /// `APPLE` as a weekly series of four.
    fn weekly_apple() -> String {
        APPLE.replacen("SEQUENCE:2\r\n", "RRULE:FREQ=WEEKLY;COUNT=4\r\nSEQUENCE:2\r\n", 1)
    }

    fn read(text: &str) -> Vec<EventData> {
        ical::read(text, &[]).unwrap()
    }

    #[test]
    fn an_edit_changes_only_its_own_lines() {
        let events = read(APPLE);
        let base = events.iter().find(|e| e.recurrence_id.is_none()).unwrap().clone();
        let mine = EventData { title: "Daily".into(), ..base.clone() };
        let text = edited_text(APPLE, &base, &mine, None, &[], Brussels).unwrap();
        let untouched = |t: &str| t.lines().filter(|l| !l.starts_with("SUMMARY") && !l.starts_with("DTSTAMP")).map(str::to_string).collect::<Vec<_>>();
        assert_eq!(untouched(&text), untouched(APPLE), "every other line byte for byte, folds and the override included");
        assert!(text.contains("SUMMARY:Daily\r\n") && text.contains("SEQUENCE:2\r\n"), "a new title raises no SEQUENCE: {text}");

        let moved = EventData { span: Span::Timed { start: at("2026-11-24 10:00"), end: at("2026-11-24 10:30") }, ..base.clone() };
        let text = edited_text(APPLE, &base, &moved, None, &[], Brussels).unwrap();
        assert!(text.contains("DTSTART;TZID=Europe/Brussels:20261124T100000\r\n") && text.contains("SEQUENCE:3\r\n"), "{text}");
        assert!(text.contains("X-APPLE-STRUCTURED-LOCATION;VALUE=URI;X-APPLE-RADIUS=70;X-TITLE=\"Room 1\":geo:50.85,4.35\r\n"));
        assert_eq!(read(&text).iter().find(|e| e.recurrence_id.is_none()).unwrap().span, moved.span);
    }

    /// Check 2: an edit of one changed occurrence writes only its VEVENT.
    #[test]
    fn an_occurrence_edit_writes_only_its_own_vevent() {
        let events = read(APPLE);
        let base = events.iter().find(|e| e.recurrence_id.is_some()).unwrap().clone();
        let mine = EventData { title: "Standup, later".into(), ..base.clone() };
        let text = edited_text(APPLE, &base, &mine, None, &[], Brussels).unwrap();
        let after = read(&text);
        assert_eq!(after.iter().find(|e| e.recurrence_id.is_none()).unwrap().title, "Standup", "the series is the same");
        assert_eq!(after.iter().find(|e| e.recurrence_id.is_some()).unwrap().title, "Standup, later");
        assert!(text.contains("X-CUSTOM;A=1:keep me\r\n"));
        let first_time = EventData { recurrence_id: Some(Moment::Time(at("2026-12-08 09:00"))), span: Span::Timed { start: at("2026-12-08 11:00"), end: at("2026-12-08 11:30") }, ..mine.clone() };
        let text = edited_text(APPLE, &base, &first_time, None, &[], Brussels).unwrap();
        let after = read(&text);
        assert_eq!(after.iter().filter(|e| e.recurrence_id.is_some()).count(), 2, "an occurrence changed for the first time gets a VEVENT: {text}");
        assert!(after.iter().any(|e| e.recurrence_id == first_time.recurrence_id && e.span == first_time.span));
        assert!(text.contains("X-CUSTOM;A=1:keep me\r\n"), "the other lines stay");
        let stranger = EventData { uid: "someone-else".into(), ..first_time };
        assert!(edited_text(APPLE, &base, &stranger, None, &[], Brussels).is_err(), "a copy without that series");
    }

    /// Check 2: taking one occurrence out writes an EXDATE and drops its
    /// changed VEVENT; the series and every other line stay.
    #[test]
    fn a_cancelled_occurrence_leaves_as_an_exdate() {
        let weekly = weekly_apple();
        let events = read(&weekly);
        let changed = events.iter().find(|e| e.recurrence_id.is_some()).unwrap().clone();
        let text = cancelled_text(&weekly, &changed, Brussels).unwrap();
        let after = read(&text);
        assert!(after.iter().all(|e| e.recurrence_id.is_none()), "the changed VEVENT is gone: {text}");
        let master = after.iter().find(|e| e.recurrence_id.is_none()).unwrap();
        let rid = changed.recurrence_id.unwrap();
        assert!(master.recurrence.as_ref().unwrap().exdates.iter().any(|x| x.names(&rid, Brussels)));
        assert!(text.contains("X-APPLE-STRUCTURED-LOCATION"), "the series' own lines stay");

        let master = events.iter().find(|e| e.recurrence_id.is_none()).unwrap().clone();
        let fourth = crate::calendar::series::occurrence(std::slice::from_ref(&master), &Moment::Time(at("2026-12-15 09:00")), Brussels).unwrap();
        let text = cancelled_text(&weekly, &fourth, Brussels).unwrap();
        assert_eq!(read(&text).iter().filter(|e| e.recurrence_id.is_some()).count(), 1, "an unchanged occurrence leaves the changed one alone");
    }

    /// A series whose new rule no longer holds a changed occurrence loses its VEVENT.
    #[test]
    fn a_dropped_occurrence_leaves_the_resource() {
        let weekly = weekly_apple();
        let events = read(&weekly);
        let master = events.iter().find(|e| e.recurrence_id.is_none()).unwrap().clone();
        let changed = events.iter().find(|e| e.recurrence_id.is_some()).unwrap().clone();
        let mine = EventData { title: "Standup, renamed".into(), ..master.clone() };
        let text = edited_text(&weekly, &master, &mine, None, std::slice::from_ref(&changed), Brussels).unwrap();
        let after = read(&text);
        assert_eq!(after.len(), 1, "{text}");
        assert_eq!(after[0].title, "Standup, renamed");
    }

    #[test]
    fn extra_lines_reach_the_resource_and_never_replace_a_modelled_one() {
        let e = EventData { title: "Hold".into(), ..EventData::new("h@jolta.app", Span::Timed { start: at("2026-11-24 09:00"), end: at("2026-11-24 10:00") }) };
        let extra = Extra::Lines(vec!["X-JOLTA-HOLD:proposal-1".into(), "SUMMARY:Not this".into(), "X-MICROSOFT-CDO-BUSYSTATUS:TENTATIVE".into()]);
        let text = created_text(std::slice::from_ref(&e), Some(&extra)).unwrap();
        assert!(text.contains("X-JOLTA-HOLD:proposal-1\r\n") && text.contains("X-MICROSOFT-CDO-BUSYSTATUS:TENTATIVE\r\n"), "{text}");
        assert!(text.contains("SUMMARY:Hold\r\n") && !text.contains("Not this"), "the title is the event's: {text}");
        assert!(created_text(std::slice::from_ref(&e), Some(&Extra::Json(Default::default()))).is_err());
    }

    fn held(etag: Option<&str>) -> (Stored, EventData) {
        let data = read(APPLE).into_iter().find(|e| e.recurrence_id.is_none()).unwrap();
        (Stored { id: EventId(1), remote_id: "/cal/standup.ics".into(), etag: etag.map(str::to_string), raw: Some(APPLE.as_bytes().to_vec()), data: data.clone() }, data)
    }

    #[tokio::test]
    async fn an_edit_puts_over_the_held_etag_and_reads_412_and_404() {
        let (stored, base) = held(Some("\"1\""));
        let mine = EventData { title: "Daily".into(), ..base.clone() };
        let update = OpWrite::Update { held: &stored, base: &base, event: &mine, extra: None, dropped: &[] };
        for (status, expected) in [("412 Precondition Failed", Written::Stale), ("404 Not Found", Written::Gone)] {
            let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"1\""), Some(APPLE.into())), (status, None, None)]).await;
            assert_eq!(dav.write(&calendar, &update, Tell::NOBODY).await.unwrap(), expected);
            assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics", "PUT /cal/standup.ics If-Match \"1\""]);
        }
        let (dav, calendar, server) = caldav_server(vec![("404 Not Found", None, None)]).await;
        assert_eq!(dav.write(&calendar, &update, Tell::NOBODY).await.unwrap(), Written::Gone);
        assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics"], "deleted since: no PUT, which iCloud would take and make it again");
        let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"5\""), Some(APPLE.into()))]).await;
        assert_eq!(dav.write(&calendar, &update, Tell::NOBODY).await.unwrap(), Written::Stale);
        assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics"], "changed since: no PUT");
        let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"1\""), Some(APPLE.into())), ("204 No Content", Some("\"2\""), None)]).await;
        let Written::Saved(Some(v)) = dav.write(&calendar, &update, Tell::NOBODY).await.unwrap() else { panic!("saved") };
        assert_eq!((v.etag.as_deref(), v.events.len()), (Some("\"2\""), 2), "the series and its changed occurrence");
        server.await.unwrap();

        let (unread, _) = held(None);
        let (dav, calendar, server) = caldav_server(vec![]).await;
        assert_eq!(dav.write(&calendar, &OpWrite::Update { held: &unread, base: &base, event: &mine, extra: None, dropped: &[] }, Tell::NOBODY).await.unwrap(), Written::Stale, "nothing without an ETag");
        assert!(server.await.unwrap().is_empty());
    }

    /// Check 4: a create retried after its first try reached the server
    /// keeps that resource and makes no second one.
    #[tokio::test]
    async fn a_retried_create_takes_the_resource_its_first_try_made() {
        let e = EventData { title: "Lunch".into(), ..EventData::new("l@jolta.app", Span::Timed { start: at("2026-11-24 12:00"), end: at("2026-11-24 13:00") }) };
        let first = created_text(std::slice::from_ref(&e), None).unwrap();
        let create = OpWrite::Create { events: std::slice::from_ref(&e), key: "/cal/jolta-l.ics", extra: None };
        let (dav, calendar, server) = caldav_server(vec![("412 Precondition Failed", None, None), ("200 OK", Some("\"7\""), Some(first.clone()))]).await;
        let Written::Saved(Some(v)) = dav.write(&calendar, &create, Tell::NOBODY).await.unwrap() else { panic!("saved") };
        assert_eq!((v.remote_id.as_str(), v.etag.as_deref(), v.events[0].title.as_str()), ("/cal/jolta-l.ics", Some("\"7\""), "Lunch"));
        assert_eq!(server.await.unwrap(), ["PUT /cal/jolta-l.ics If-None-Match *", "GET /cal/jolta-l.ics"]);

        let other = first.replace("l@jolta.app", "someone-else");
        let (dav, calendar, _) = caldav_server(vec![("412 Precondition Failed", None, None), ("200 OK", Some("\"7\""), Some(other))]).await;
        assert!(dav.write(&calendar, &create, Tell::NOBODY).await.is_err(), "another event at that name is left alone");
    }

    /// iCloud moves a resource whatever ETag If-Match names, so it is read
    /// first; a resource an earlier try moved is found in the other
    /// calendar (check 4).
    #[tokio::test]
    async fn a_move_reads_the_resource_first_and_finds_one_moved_before() {
        let (stored, _) = held(Some("\"1\""));
        let home = |calendar: &Calendar| Calendar { remote_id: calendar.remote_id.replace("/cal/", "/home/"), ..calendar.clone() };
        let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"1\""), Some(APPLE.into())), ("201 Created", None, None), ("200 OK", Some("\"2\""), Some(APPLE.into()))]).await;
        let Written::Saved(Some(v)) = dav.write(&calendar, &OpWrite::Move { held: &stored, to: &home(&calendar) }, Tell::NOBODY).await.unwrap() else { panic!("moved") };
        assert_eq!((v.remote_id.as_str(), v.etag.as_deref()), ("/home/standup.ics", Some("\"2\"")), "the same name in the other calendar, at the version it has there");
        assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics", "MOVE /cal/standup.ics If-Match \"1\"", "GET /home/standup.ics"]);

        let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"5\""), Some(APPLE.into()))]).await;
        assert_eq!(dav.write(&calendar, &OpWrite::Move { held: &stored, to: &home(&calendar) }, Tell::NOBODY).await.unwrap(), Written::Stale);
        assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics"], "changed since: no MOVE");

        let (dav, calendar, server) = caldav_server(vec![("404 Not Found", None, None), ("200 OK", Some("\"2\""), Some(APPLE.into()))]).await;
        assert!(matches!(dav.write(&calendar, &OpWrite::Move { held: &stored, to: &home(&calendar) }, Tell::NOBODY).await.unwrap(), Written::Saved(Some(_))), "the first try moved it");
        assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics", "GET /home/standup.ics"]);
        let (dav, calendar, _) = caldav_server(vec![("404 Not Found", None, None), ("404 Not Found", None, None)]).await;
        assert_eq!(dav.write(&calendar, &OpWrite::Move { held: &stored, to: &home(&calendar) }, Tell::NOBODY).await.unwrap(), Written::Gone);
    }

    #[tokio::test]
    async fn a_delete_carries_the_held_etag() {
        let (stored, _) = held(Some("\"1\""));
        let (dav, calendar, server) = caldav_server(vec![("204 No Content", None, None)]).await;
        assert_eq!(dav.write(&calendar, &OpWrite::Delete { held: &stored }, Tell::NOBODY).await.unwrap(), Written::Saved(None));
        assert_eq!(server.await.unwrap(), ["DELETE /cal/standup.ics If-Match \"1\""]);
    }
    /// iCloud mails a new time or place on its own, but not a new title
    /// (C3 U11, R3 A10: the guests never heard). With Send, every guest who
    /// stays asks the server to send (SCHEDULE-FORCE-SEND, RFC 6638 7.3),
    /// which iCloud does; without it nothing is forced. The copy Jolta keeps
    /// holds no such parameter, so a later edit forces nothing again.
    #[tokio::test]
    async fn send_makes_the_server_tell_every_guest() {
        let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:m@example.com\r\nDTSTAMP:20261101T120000Z\r\n\
            DTSTART:20261124T090000Z\r\nDTEND:20261124T093000Z\r\nSUMMARY:Review\r\nORGANIZER:mailto:me@example.com\r\n\
            ATTENDEE;PARTSTAT=ACCEPTED:mailto:me@example.com\r\nATTENDEE;CN=Ann;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:ann@example.com\r\n\
            SEQUENCE:1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let base = read(text).remove(0);
        let stored = Stored { id: EventId(1), remote_id: "/cal/m.ics".into(), etag: Some("\"1\"".into()), raw: Some(text.as_bytes().to_vec()), data: base.clone() };
        let renamed = EventData { title: "Design review".into(), ..base.clone() };
        let update = OpWrite::Update { held: &stored, base: &base, event: &renamed, extra: None, dropped: &[] };
        for (notify, forced) in [(Notify::All, true), (Notify::Changed, false)] {
            let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"1\""), Some(text.into())), ("204 No Content", Some("\"2\""), None)]).await;
            let Written::Saved(Some(v)) = dav.write(&calendar, &update, Tell { notify, message: None }).await.unwrap() else { panic!("saved") };
            let put = format!("PUT /cal/m.ics If-Match \"1\"{}", if forced { " forces a send" } else { "" });
            assert_eq!(server.await.unwrap(), ["GET /cal/m.ics".to_string(), put], "{notify:?}");
            assert!(!String::from_utf8_lossy(v.raw.as_deref().unwrap()).contains("SCHEDULE-FORCE-SEND"), "the copy kept");
        }
    }

    /// A meeting the server schedules takes its guests' answers from the
    /// organizer's copy: a new time asks them again, as Apple's Calendar
    /// does, and iCloud kept "accepted" for a guest who never saw the new
    /// time (C3 U11, R3 A4). A new place keeps the answers, and so does a
    /// series that only ends sooner; the organizer's own line stays.
    #[test]
    fn a_new_time_asks_the_guests_again() {
        let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:m@example.com\r\nDTSTAMP:20261101T120000Z\r\n\
            DTSTART:20261124T090000Z\r\nDTEND:20261124T093000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Review\r\nORGANIZER:mailto:me@icloud.com\r\n\
            ATTENDEE;PARTSTAT=ACCEPTED:mailto:me@icloud.com\r\n\
            ATTENDEE;CN=Ann;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:ann@example.com\r\nSEQUENCE:1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let base = read(text).remove(0);
        let ann = |out: &str| out.replace("\r\n ", "").lines().find(|l| l.contains("ann@example.com")).unwrap().to_string();
        let placed = EventData { location: "Room 4".into(), ..base.clone() };
        assert!(ann(&edited_text(text, &base, &placed, None, &[], Brussels).unwrap()).contains("PARTSTAT=ACCEPTED"), "a new place");
        let shorter = EventData { recurrence: Some(crate::calendar::Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=2".into()], ..Default::default() }), ..base.clone() };
        assert!(ann(&edited_text(text, &base, &shorter, None, &[], Brussels).unwrap()).contains("PARTSTAT=ACCEPTED"), "fewer occurrences");
        let mut moved = base.clone();
        let utc = |s: &str| Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(chrono_tz::UTC) };
        moved.span = Span::Timed { start: utc("2026-11-24 11:00"), end: utc("2026-11-24 11:30") };
        let out = edited_text(text, &base, &moved, None, &[], Brussels).unwrap();
        assert!(ann(&out).contains("PARTSTAT=NEEDS-ACTION") && ann(&out).contains("RSVP=TRUE"), "{}", ann(&out));
        assert!(out.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:me@icloud.com\r\n"), "the organizer's own line: {out}");
    }

    /// A meeting iCloud scheduled: a guest who stays keeps their line with
    /// what the server added, a new one gets RSVP from Request responses, a
    /// removed one goes, and only the user organizes.
    #[test]
    fn guests_change_line_by_line() {
        let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:m@example.com\r\nDTSTAMP:20261101T120000Z\r\n\
            DTSTART:20261124T090000Z\r\nDTEND:20261124T093000Z\r\nSUMMARY:Review\r\nORGANIZER:mailto:me@icloud.com\r\n\
            ATTENDEE;CN=Ann;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:ann@example.com\r\n\
            ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bo@example.com\r\nSEQUENCE:1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let base = read(text).remove(0);
        let mut e = base.clone();
        e.attendees.retain(|a| a.person.address != "bo@example.com");
        e.attendees.push(crate::calendar::Attendee {
            person: crate::calendar::Person { address: "cy@example.com".into(), name: String::new() },
            role: crate::calendar::Role::Optional,
            response: crate::calendar::Response::NeedsAction,
            comment: String::new(),
            is_me: false,
        });
        let out = edited_text(text, &base, &e, None, &[], Brussels).unwrap().replace("\r\n ", "");
        assert!(out.contains("ATTENDEE;CN=Ann;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:ann@example.com\r\n"), "kept byte for byte: {out}");
        assert!(!out.contains("bo@example.com"));
        assert!(out.contains("ATTENDEE;CUTYPE=INDIVIDUAL;ROLE=OPT-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:cy@example.com"), "{out}");
        assert_eq!(organized(&out, "m@example.com", &Person { address: "other@icloud.com".into(), name: String::new() }), out, "an organizer stays");

        let mut quiet = e.clone();
        quiet.options.request_responses = false;
        quiet.options.allow_new_times = false;
        let out = edited_text(text, &base, &quiet, None, &[], Brussels).unwrap().replace("\r\n ", "");
        assert!(out.contains("X-MICROSOFT-DISALLOW-COUNTER:TRUE") && out.contains("RSVP=FALSE:mailto:cy@example.com"), "{out}");
        assert_eq!(read(&out)[0].options.allow_new_times, false, "read back");

        let new = EventData { attendees: e.attendees.clone(), ..EventData::new("n@jolta.app", base.span) };
        let created = organized(&created_text(std::slice::from_ref(&new), None).unwrap(), "n@jolta.app", &Person { address: "me@icloud.com".into(), name: String::new() });
        assert!(created.contains("ORGANIZER:mailto:me@icloud.com\r\n"), "a new meeting names the user as organizer: {created}");
    }

}
