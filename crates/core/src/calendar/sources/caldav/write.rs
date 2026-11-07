//! Calendar ops on CalDAV: a new resource PUT with `If-None-Match: *` at a
//! name of Jolta's, an edit written into the server's own text through
//! `ical_edit` so every line Jolta doesn't model stays byte for byte, and a
//! DELETE, the edit and the delete guarded by the resource's ETag.

use chrono_tz::Tz;
use reqwest::Url;

use super::super::{OpWrite, Version, Written, cannot, changed_fields};
use super::add::resource_name;
use super::{CalDav, href_key, join, path_escape};
use crate::calendar::change::Field;
use crate::calendar::ical_edit::{Component, Object, Prop, utc_stamp, vevent, write_event, write_field};
use crate::calendar::ops::Extra;
use crate::calendar::{Calendar, EventData, ical, rsvp, tz};
use crate::{Error, Result};

const PRODID: &str = concat!("-//Jolta//Jolta ", env!("CARGO_PKG_VERSION"), "//EN");

impl CalDav {
    pub async fn write(&self, calendar: &Calendar, write: &OpWrite<'_>) -> Result<Written> {
        let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        match *write {
            OpWrite::Create { events, key, extra } => {
                let url = join(&collection, &path_escape(key))?;
                let text = created_text(events, extra)?;
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
            OpWrite::Update { held, base, event, extra } => {
                let (Some(etag), Some(raw)) = (held.etag.as_deref(), held.raw.as_deref()) else { return Ok(Written::Stale) };
                let url = join(&collection, &path_escape(&held.remote_id))?;
                let text = edited_text(&String::from_utf8_lossy(raw), base, event, extra, tz::here())?;
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
                let reply = self.exchange("PUT", &url, &headers, &text).await?;
                match reply.status {
                    200..300 => Ok(Written::Saved(Some(self.version(&held.remote_id, reply.etag, text).await?))),
                    412 => Ok(Written::Stale),
                    404 | 410 => Ok(Written::Gone),
                    status => Err(Error::Other(format!("CalDAV PUT {url}: {status} {}", reply.body.trim()))),
                }
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
        Ok(Version { remote_id: remote_id.to_string(), etag, events, raw: Some(text.into_bytes()) })
    }
}

/// Where a new event of `calendar` goes: the path of a resource named from
/// `key`, as a sync names it, so the stored event and the synced one match.
pub(crate) fn resource_path(calendar: &Calendar, key: &str) -> Result<String> {
    let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
    Ok(href_key(join(&collection, &resource_name(key))?.path()))
}

pub(crate) fn writable(write: &OpWrite<'_>) -> Result<()> {
    match write {
        OpWrite::Create { events, extra, .. } => created_text(events, *extra).map(drop),
        OpWrite::Update { base, event, extra, .. } => {
            lines(*extra)?;
            changed_fields(base, event).into_iter().try_for_each(|f| editable(f))
        }
        OpWrite::Delete { .. } => Ok(()),
    }
}

/// The fields Jolta writes into a resource. Guests and video calls wait
/// for the units that send invitations and make meetings.
fn editable(field: Field) -> Result<()> {
    match field {
        Field::People => Err(cannot("Jolta", "change guests on this calendar yet")),
        Field::MeetingLink => Err(cannot("Jolta", "change the video call on this calendar yet")),
        _ => Ok(()),
    }
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
pub(crate) fn edited_text(text: &str, base: &EventData, event: &EventData, extra: Option<&Extra>, here: Tz) -> Result<String> {
    let mut obj = Object::parse(text);
    let rid = match &event.recurrence_id {
        None => None,
        Some(rid) => Some(
            obj.calendar()
                .into_iter()
                .flat_map(Component::components)
                .filter(|c| c.name() == "VEVENT" && c.prop("UID").is_some_and(|u| u.text() == event.uid))
                .find_map(|c| c.prop("RECURRENCE-ID").filter(|p| rsvp::prop_moment(p).is_some_and(|m| m.names(rid, here))))
                .map(|p| p.value().to_string())
                .ok_or_else(|| Error::Other("The calendar's copy no longer has this occurrence.".into()))?,
        ),
    };
    let component = obj.event_mut(&event.uid, rid.as_deref()).ok_or_else(|| Error::Other("The calendar's copy no longer has this event.".into()))?;
    let fields = changed_fields(base, event);
    for &field in &fields {
        editable(field)?;
        write_field(component, field, event);
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

    fn read(text: &str) -> Vec<EventData> {
        ical::read(text, &[]).unwrap()
    }

    #[test]
    fn an_edit_changes_only_its_own_lines() {
        let events = read(APPLE);
        let base = events.iter().find(|e| e.recurrence_id.is_none()).unwrap().clone();
        let mine = EventData { title: "Daily".into(), ..base.clone() };
        let text = edited_text(APPLE, &base, &mine, None, Brussels).unwrap();
        let untouched = |t: &str| t.lines().filter(|l| !l.starts_with("SUMMARY") && !l.starts_with("DTSTAMP")).map(str::to_string).collect::<Vec<_>>();
        assert_eq!(untouched(&text), untouched(APPLE), "every other line byte for byte, folds and the override included");
        assert!(text.contains("SUMMARY:Daily\r\n") && text.contains("SEQUENCE:2\r\n"), "a new title raises no SEQUENCE: {text}");

        let moved = EventData { span: Span::Timed { start: at("2026-11-24 10:00"), end: at("2026-11-24 10:30") }, ..base.clone() };
        let text = edited_text(APPLE, &base, &moved, None, Brussels).unwrap();
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
        let text = edited_text(APPLE, &base, &mine, None, Brussels).unwrap();
        let after = read(&text);
        assert_eq!(after.iter().find(|e| e.recurrence_id.is_none()).unwrap().title, "Standup", "the series is the same");
        assert_eq!(after.iter().find(|e| e.recurrence_id.is_some()).unwrap().title, "Standup, later");
        assert!(text.contains("X-CUSTOM;A=1:keep me\r\n"));
        let elsewhere = EventData { recurrence_id: Some(Moment::Time(at("2026-12-08 09:00"))), ..mine };
        assert!(edited_text(APPLE, &base, &elsewhere, None, Brussels).is_err(), "an occurrence the copy doesn't hold");
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
        let update = OpWrite::Update { held: &stored, base: &base, event: &mine, extra: None };
        for (status, expected) in [("412 Precondition Failed", Written::Stale), ("404 Not Found", Written::Gone)] {
            let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"1\""), Some(APPLE.into())), (status, None, None)]).await;
            assert_eq!(dav.write(&calendar, &update).await.unwrap(), expected);
            assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics", "PUT /cal/standup.ics If-Match \"1\""]);
        }
        let (dav, calendar, server) = caldav_server(vec![("404 Not Found", None, None)]).await;
        assert_eq!(dav.write(&calendar, &update).await.unwrap(), Written::Gone);
        assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics"], "deleted since: no PUT, which iCloud would take and make it again");
        let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"5\""), Some(APPLE.into()))]).await;
        assert_eq!(dav.write(&calendar, &update).await.unwrap(), Written::Stale);
        assert_eq!(server.await.unwrap(), ["GET /cal/standup.ics"], "changed since: no PUT");
        let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"1\""), Some(APPLE.into())), ("204 No Content", Some("\"2\""), None)]).await;
        let Written::Saved(Some(v)) = dav.write(&calendar, &update).await.unwrap() else { panic!("saved") };
        assert_eq!((v.etag.as_deref(), v.events.len()), (Some("\"2\""), 2), "the series and its changed occurrence");
        server.await.unwrap();

        let (unread, _) = held(None);
        let (dav, calendar, server) = caldav_server(vec![]).await;
        assert_eq!(dav.write(&calendar, &OpWrite::Update { held: &unread, base: &base, event: &mine, extra: None }).await.unwrap(), Written::Stale, "nothing without an ETag");
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
        let Written::Saved(Some(v)) = dav.write(&calendar, &create).await.unwrap() else { panic!("saved") };
        assert_eq!((v.remote_id.as_str(), v.etag.as_deref(), v.events[0].title.as_str()), ("/cal/jolta-l.ics", Some("\"7\""), "Lunch"));
        assert_eq!(server.await.unwrap(), ["PUT /cal/jolta-l.ics If-None-Match *", "GET /cal/jolta-l.ics"]);

        let other = first.replace("l@jolta.app", "someone-else");
        let (dav, calendar, _) = caldav_server(vec![("412 Precondition Failed", None, None), ("200 OK", Some("\"7\""), Some(other))]).await;
        assert!(dav.write(&calendar, &create).await.is_err(), "another event at that name is left alone");
    }

    #[tokio::test]
    async fn a_delete_carries_the_held_etag() {
        let (stored, _) = held(Some("\"1\""));
        let (dav, calendar, server) = caldav_server(vec![("204 No Content", None, None)]).await;
        assert_eq!(dav.write(&calendar, &OpWrite::Delete { held: &stored }).await.unwrap(), Written::Saved(None));
        assert_eq!(server.await.unwrap(), ["DELETE /cal/standup.ics If-Match \"1\""]);
    }
}
