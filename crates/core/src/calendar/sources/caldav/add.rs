//! Add to calendar and Update in your calendar on CalDAV: the invitation's
//! own VEVENTs, kept byte for byte, with SCHEDULE-AGENT=CLIENT on the
//! ORGANIZER so a scheduling server sends nobody anything.

use chrono_tz::Tz;
use reqwest::Url;
use sha2::{Digest, Sha256};

use super::super::{Change, NewCopy, Replaced};
use super::{CalDav, cached, href_key, join, path_escape, same_version};
use crate::calendar::ical_edit::{Component, Object};
use crate::calendar::rsvp::{self, Rsvp, Scope, Target};
use crate::calendar::{Calendar, EventId, Moment, ical, remove, tz};
use crate::{Error, Result};

impl CalDav {
    /// PUTs the invitation as a new resource with `If-None-Match: *`, so it
    /// never overwrites anything. Its name comes from the UID, so a retry
    /// after a lost answer finds the copy the first try made and keeps it.
    pub async fn create(&self, calendar: &Calendar, copy: &NewCopy<'_>) -> Result<Change> {
        let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        let url = join(&collection, &resource_name(copy.uid))?;
        let text = resource(copy, tz::here())?;
        let headers = [("Content-Type", "text/calendar; charset=utf-8"), ("If-None-Match", "*")];
        let reply = self.exchange("PUT", &url, &headers, &text).await?;
        let remote_id = href_key(url.path());
        let me = self.me().await;
        match reply.status {
            200..300 => Ok(Change::Upsert { remote_id, etag: reply.etag, events: ical::read(&text, &me)?, raw: Some(text.into_bytes()) }),
            412 => match self.versioned(&url, "Add it").await? {
                Some((found, etag)) if ical::read(&found, &[]).is_ok_and(|events| events.iter().any(|e| e.uid == copy.uid)) => {
                    Ok(Change::Upsert { remote_id, etag: Some(etag), events: ical::read(&found, &me)?, raw: Some(found.into_bytes()) })
                }
                _ => Err(Error::Other("Your calendar already has another event where Jolta would put this one.".into())),
            },
            status => Err(Error::Other(format!("CalDAV PUT {url}: {status} {}", reply.body.trim()))),
        }
    }

    /// Writes the newer version over the copy with `If-Match`: a whole new
    /// resource when the message has the series or single event, else its
    /// changed occurrences in place of the copy's. A copy fetched again,
    /// because the stored one has no ETag, is written over only while it
    /// is the version Jolta read.
    pub async fn replace(&self, calendar: &Calendar, target: &Target, copy: &NewCopy<'_>) -> Result<Replaced> {
        let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        let url = join(&collection, &path_escape(&target.event.remote_id))?;
        let here = tz::here();
        let (stored, etag) = match cached(&target.event) {
            Some(found) => found,
            None => match self.versioned(&url, "Update it").await? {
                None => return Err(Error::Other("The meeting is no longer in your calendar.".into())),
                Some((text, _)) if !same_version(&target.event.data, &text, here) => return Ok(Replaced::Changed),
                Some(found) => found,
            },
        };
        let text = match copy.master() {
            Some(_) => resource(copy, here)?,
            None => with_occurrences(&stored, copy, here)?,
        };
        let headers = [("Content-Type", "text/calendar; charset=utf-8"), ("If-Match", etag.as_str())];
        let reply = self.exchange("PUT", &url, &headers, &text).await?;
        match reply.status {
            200..300 => {
                let events = ical::read(&text, &self.me().await)?;
                Ok(Replaced::Written(Some(Change::Upsert { remote_id: target.event.remote_id.clone(), etag: reply.etag, events, raw: Some(text.into_bytes()) })))
            }
            412 => Ok(Replaced::Changed),
            404 | 410 => Err(Error::Other("The meeting is no longer in your calendar.".into())),
            status => Err(Error::Other(format!("CalDAV PUT {url}: {status} {}", reply.body.trim()))),
        }
    }
}

/// The resource's name in the collection: the same for the same UID.
pub(super) fn resource_name(uid: &str) -> String {
    let hash = Sha256::digest(uid.as_bytes());
    format!("jolta-{}.ics", hex::encode(&hash[..16]))
}

/// The invitation as a calendar object resource: without METHOD, which a
/// stored object must not have (RFC 4791 section 4.1), with only the
/// events of its UID and their time zones, every ORGANIZER quiet, and the
/// user's PARTSTAT set to the answer given. Every other line stays as sent.
pub(crate) fn resource(copy: &NewCopy, here: Tz) -> Result<String> {
    let mut obj = Object::parse(copy.text);
    let calendar = obj.calendar_mut().ok_or_else(|| Error::Other("The invitation has no calendar to add.".into()))?;
    calendar.remove("METHOD");
    let others: Vec<(String, Option<String>)> = calendar
        .components()
        .filter(|c| c.name() == "VEVENT")
        .filter_map(|c| {
            let uid = c.prop("UID").map(|p| p.text())?;
            (uid != copy.uid).then(|| (uid, c.prop("RECURRENCE-ID").map(|p| p.value().to_string())))
        })
        .collect();
    for (uid, rid) in others {
        obj.remove_event(&uid, rid.as_deref());
    }
    if obj.event(copy.uid, None).is_none() && copy.master().is_some() {
        return Err(Error::Other(format!("the invitation has no event {}", copy.uid)));
    }
    let text = obj.to_string();
    answered(&text, copy, None, here)
}

/// `text` with every ORGANIZER of the copy's UID quiet and the user's
/// PARTSTAT set, for the whole event or with `rid` one occurrence. A copy
/// without an answer, or whose guest list doesn't name the user, only goes
/// quiet.
fn answered(text: &str, copy: &NewCopy, rid: Option<&Moment>, here: Tz) -> Result<String> {
    if let Some(answer) = copy.answer {
        let rsvp = Rsvp { event: EventId(0), answer, note: None, notify: false, scope: rid.map_or(Scope::Series, |r| Scope::Occurrence(*r)), proposal: None };
        if let Ok(done) = rsvp::answer_in_ical(text, copy.uid, rid, &[copy.me], &rsvp, here) {
            return Ok(done);
        }
    }
    Ok(remove::quiet(text, copy.uid).unwrap_or_else(|| text.to_string()))
}

/// The stored resource with each changed occurrence of `copy` in place of
/// the one it had for that moment.
fn with_occurrences(stored: &str, copy: &NewCopy, here: Tz) -> Result<String> {
    let sent = Object::parse(copy.text);
    let mut obj = Object::parse(stored);
    if obj.event(copy.uid, None).is_none() {
        return Err(Error::Other("Your calendar's copy has no series to update.".into()));
    }
    let mut rids = Vec::new();
    for e in copy.overrides() {
        let rid = e.recurrence_id.expect("an override");
        let new = override_named(&sent, copy.uid, &rid, here).ok_or_else(|| Error::Other("The update has no occurrence to write.".into()))?;
        if let Some(old) = override_named(&obj, copy.uid, &rid, here) {
            let value = old.prop("RECURRENCE-ID").map(|p| p.value().to_string());
            obj.remove_event(copy.uid, value.as_deref());
        }
        obj.add_after_master(new.clone());
        rids.push(rid);
    }
    let mut text = obj.to_string();
    for rid in &rids {
        text = answered(&text, copy, Some(rid), here)?;
    }
    Ok(text)
}

/// The VEVENT of `uid` whose RECURRENCE-ID names the moment `rid`.
fn override_named<'a>(obj: &'a Object, uid: &str, rid: &Moment, here: Tz) -> Option<&'a Component> {
    obj.calendar()
        .into_iter()
        .flat_map(Component::components)
        .filter(|c| c.name() == "VEVENT" && c.prop("UID").is_some_and(|u| u.text() == uid))
        .find(|c| c.prop("RECURRENCE-ID").and_then(rsvp::prop_moment).is_some_and(|m| m.names(rid, here)))
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::rsvp::Answer;

    const REQUEST: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Google Inc//Google Calendar 70.9054//EN\r\nMETHOD:REQUEST\r\n\
        BEGIN:VTIMEZONE\r\nTZID:Europe/Brussels\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:standup@google.com\r\nSEQUENCE:0\r\n\
        DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY\r\n\
        SUMMARY:Standup\r\nORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@icloud.example\r\n\
        ATTENDEE;PARTSTAT=ACCEPTED:mailto:ann@example.com\r\nX-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn copy<'a>(text: &'a str, events: &'a [crate::calendar::EventData], answer: Option<Answer>) -> NewCopy<'a> {
        NewCopy { uid: "standup@google.com", text, events, me: "me@icloud.example", answer }
    }

    #[test]
    fn the_resource_is_the_invitation_without_method_quiet_and_answered() {
        let events = ical::read(REQUEST, &[]).unwrap();
        let text = resource(&copy(REQUEST, &events, None), Brussels).unwrap();
        assert!(!text.contains("METHOD:"), "a stored object has no METHOD: {text}");
        assert!(text.contains("ORGANIZER;CN=Ann;SCHEDULE-AGENT=CLIENT:mailto:ann@example.com\r\n"), "the server sends nothing: {text}");
        assert!(text.contains("ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@icloud.example\r\n"), "no answer yet: as sent");
        assert!(text.contains("X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij\r\n") && text.contains("BEGIN:VTIMEZONE"), "the rest as sent");

        let accepted = resource(&copy(REQUEST, &events, Some(Answer::Accept)), Brussels).unwrap();
        assert!(accepted.contains("ATTENDEE;PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:me@icloud.example\r\n"), "the answer given: {accepted}");
        assert!(accepted.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:ann@example.com\r\n"), "only the user's entry changes");
        assert_eq!(accepted.matches("SCHEDULE-AGENT=CLIENT").count(), 1);
    }

    #[test]
    fn another_address_of_the_user_is_never_answered_for() {
        let events = ical::read(REQUEST, &[]).unwrap();
        let other = NewCopy { me: "me@work.example", ..copy(REQUEST, &events, Some(Answer::Decline)) };
        let text = resource(&other, Brussels).unwrap();
        assert!(!text.contains("DECLINED"), "the invitation went to the iCloud address only: {text}");
        assert!(text.contains("SCHEDULE-AGENT=CLIENT"), "still quiet");
    }

    #[test]
    fn the_same_uid_gives_the_same_resource_so_a_retry_finds_the_first_copy() {
        assert_eq!(resource_name("standup@google.com"), resource_name("standup@google.com"));
        assert_ne!(resource_name("standup@google.com"), resource_name("review@google.com"));
        assert!(resource_name("a/b c@d").starts_with("jolta-") && resource_name("a/b c@d").ends_with(".ics"), "no character a path must escape");
    }

    /// The request as Jolta stored its copy at SEQUENCE 0, under `etag`.
    fn added(etag: Option<&str>) -> Target {
        let text = resource(&copy(REQUEST, &ical::read(REQUEST, &[]).unwrap(), None), Brussels).unwrap();
        let data = ical::read(&text, &[]).unwrap().remove(0);
        let event = rsvp::Stored { id: EventId(1), remote_id: "/cal/jolta-1.ics".into(), etag: etag.map(str::to_string), raw: Some(text.into_bytes()), data };
        Target { event, occurrence: None, me: "me@icloud.example".into() }
    }

    #[tokio::test]
    async fn a_new_copy_never_overwrites_and_a_retry_keeps_the_first() {
        let events = ical::read(REQUEST, &[]).unwrap();
        let new = copy(REQUEST, &events, Some(Answer::Accept));
        let name = resource_name("standup@google.com");

        let (dav, calendar, server) = super::super::tests::caldav_server(vec![("201 Created", Some("\"1\""), None)]).await;
        let Change::Upsert { remote_id, etag, raw, .. } = dav.create(&calendar, &new).await.unwrap() else { panic!("an upsert") };
        assert_eq!((remote_id, etag.as_deref()), (format!("/cal/{name}"), Some("\"1\"")));
        assert!(String::from_utf8(raw.unwrap()).unwrap().contains("PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:me@icloud.example"));
        assert_eq!(server.await.unwrap(), [format!("PUT /cal/{name} If-None-Match *")]);

        let first = resource(&new, Brussels).unwrap();
        let (dav, calendar, server) = super::super::tests::caldav_server(vec![("412 Precondition Failed", None, None), ("200 OK", Some("\"7\""), Some(first))]).await;
        let Change::Upsert { etag, .. } = dav.create(&calendar, &new).await.unwrap() else { panic!("an upsert") };
        assert_eq!(etag.as_deref(), Some("\"7\""), "the copy the lost answer made");
        assert_eq!(server.await.unwrap(), [format!("PUT /cal/{name} If-None-Match *"), format!("GET /cal/{name}")]);

        let other = REQUEST.replace("UID:standup@google.com", "UID:someone-else");
        let (dav, calendar, server) = super::super::tests::caldav_server(vec![("412 Precondition Failed", None, None), ("200 OK", Some("\"7\""), Some(other))]).await;
        assert!(dav.create(&calendar, &new).await.is_err(), "another event lives there");
        assert_eq!(server.await.unwrap().len(), 2, "and nothing is written over it");
    }

    #[tokio::test]
    async fn an_update_writes_only_over_the_version_jolta_read() {
        let newer = REQUEST.replace("SEQUENCE:0", "SEQUENCE:1").replace("T093000", "T100000").replace("T094500", "T101500");
        let events = ical::read(&newer, &[]).unwrap();
        let update = copy(&newer, &events, None);

        let (dav, calendar, server) = super::super::tests::caldav_server(vec![("204 No Content", Some("\"2\""), None)]).await;
        let Replaced::Written(Some(Change::Upsert { events, .. })) = dav.replace(&calendar, &added(Some("\"1\"")), &update).await.unwrap() else { panic!("written") };
        assert_eq!(events[0].sequence, 1);
        assert_eq!(server.await.unwrap(), ["PUT /cal/jolta-1.ics If-Match \"1\""]);

        let (dav, calendar, server) = super::super::tests::caldav_server(vec![("412 Precondition Failed", None, None), ("204 No Content", None, None)]).await;
        assert_eq!(dav.replace(&calendar, &added(Some("\"1\"")), &update).await.unwrap(), Replaced::Changed);
        assert_eq!(server.await.unwrap(), ["PUT /cal/jolta-1.ics If-Match \"1\""], "no second try over a copy changed since");

        let changed = REQUEST.replace("SEQUENCE:0", "SEQUENCE:3");
        let (dav, calendar, server) = super::super::tests::caldav_server(vec![("200 OK", Some("\"5\""), Some(changed)), ("204 No Content", None, None)]).await;
        assert_eq!(dav.replace(&calendar, &added(None), &update).await.unwrap(), Replaced::Changed, "fetched again, it isn't the version Jolta read");
        assert_eq!(server.await.unwrap(), ["GET /cal/jolta-1.ics"]);

        let (dav, calendar, server) = super::super::tests::caldav_server(vec![("200 OK", None, Some(REQUEST.to_string())), ("204 No Content", None, None)]).await;
        assert!(dav.replace(&calendar, &added(None), &update).await.unwrap_err().to_string().contains("ETag"));
        assert_eq!(server.await.unwrap(), ["GET /cal/jolta-1.ics"], "never written without an ETag");
    }

    #[test]
    fn an_occurrence_update_replaces_only_that_occurrence() {
        let stored = resource(&copy(REQUEST, &ical::read(REQUEST, &[]).unwrap(), Some(Answer::Accept)), Brussels).unwrap();
        let moved = REQUEST
            .replace("SEQUENCE:0\r\n", "SEQUENCE:1\r\nRECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\n")
            .replace("DTSTART;TZID=Europe/Brussels:20261110T093000", "DTSTART;TZID=Europe/Brussels:20261117T110000")
            .replace("DTEND;TZID=Europe/Brussels:20261110T094500", "DTEND;TZID=Europe/Brussels:20261117T111500")
            .replace("RRULE:FREQ=WEEKLY\r\n", "");
        let events = ical::read(&moved, &[]).unwrap();
        let once = with_occurrences(&stored, &copy(&moved, &events, Some(Answer::Accept)), Brussels).unwrap();
        let read = ical::read(&once, &["me@icloud.example".into()]).unwrap();
        assert_eq!(read.len(), 2, "the series and the moved occurrence: {once}");
        assert!(once.contains("DTSTART;TZID=Europe/Brussels:20261117T110000") && once.contains("RRULE:FREQ=WEEKLY"));
        assert!(read.iter().all(|e| e.my_response() == Some(crate::calendar::Response::Accepted)), "the answer holds on both: {once}");

        let utc = moved.replace("RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000", "RECURRENCE-ID:20261117T083000Z").replace("T110000", "T120000");
        let events = ical::read(&utc, &[]).unwrap();
        let twice = with_occurrences(&once, &copy(&utc, &events, None), Brussels).unwrap();
        assert_eq!(twice.matches("RECURRENCE-ID").count(), 1, "the same moment written in UTC replaces it: {twice}");
        assert!(twice.contains("T120000") && !twice.contains("T110000"));
    }
}
