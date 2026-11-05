//! Add to calendar and Update in your calendar on Google: `events.import`,
//! which keeps the invitation's UID as `iCalUID` and emails nobody, again
//! with the event's ETag for a newer version, and patches with
//! `sendUpdates=none` for an answer. The import is a
//! private copy (`privateCopy`), so nothing done to it later reaches the
//! organizer either. Google imports an event only into the calendar of its
//! organizer or a guest (its 400 on the Microsoft 365 test account's
//! invitation, 2026-11-04), so the calendar's own account joins the guests
//! with the user's answer.

use chrono_tz::Tz;
use serde_json::{Value, json};

use super::super::{NewCopy, Replaced, encode, occurrence_etag};
use super::{BASE, Google, failure, parse};
use crate::calendar::ical_edit::Prop;
use crate::calendar::rsvp::{Answer, Stored, Target};
use crate::calendar::{Attendee, Calendar, EventData, EventStatus, Local, Moment, Response, Role, ShowAs, Span, Zone, tz};
use crate::{Error, Result};

impl Google {
    /// Imports the invitation's series or single event, then its changed
    /// occurrences, which Google files under the series by their UID and
    /// original start. A calendar that already holds the UID, as after a
    /// retry whose answer was lost, keeps that event.
    pub async fn create(&self, calendar: &Calendar, copy: &NewCopy<'_>) -> Result<String> {
        let cal = encode(&calendar.remote_id);
        let found: Value = parse(&self.get_ok(&format!("{BASE}/calendars/{cal}/events?iCalUID={}", encode(copy.uid))).await?)?;
        if let Some(id) = found["items"].as_array().into_iter().flatten().find(|e| e["status"] != "cancelled").and_then(|e| e["id"].as_str()) {
            return Ok(id.to_string());
        }
        let first = copy.master().or(copy.events.first()).ok_or_else(|| Error::Other("The invitation has no event to add.".into()))?;
        let created = self.import(calendar, first, copy).await?;
        if copy.master().is_some() {
            for o in copy.overrides() {
                self.import(calendar, o, copy).await?;
            }
        }
        created["id"].as_str().map(str::to_string).ok_or_else(|| Error::Other("Google returned no id for the imported event.".into()))
    }

    /// Imports the newer version over the copy, which Google matches by
    /// its UID. A guest can't patch a private copy's time or title (Google's
    /// 403, 2026-11-04), so each import carries the ETag of the event it
    /// writes over, which `events.import` honours with a 412 for another one
    /// (2026-11-05). A changed occurrence stored without its ETag stops
    /// the update before anything is written.
    pub async fn replace(&self, calendar: &Calendar, target: &Target, occurrences: &[Stored], copy: &NewCopy<'_>) -> Result<Replaced> {
        let Some(imports) = imports(target, occurrences, copy, tz::here())? else { return Ok(Replaced::Changed) };
        for (e, etag) in imports {
            let (status, reply) = self.send_import(calendar, e, copy, etag).await?;
            match status {
                200..300 => {}
                412 => return Ok(Replaced::Changed),
                404 | 410 => return Err(Error::Other("The meeting is no longer in your calendar.".into())),
                _ => return Err(failure(status, &reply)),
            }
        }
        Ok(Replaced::Written(None))
    }

    async fn import(&self, calendar: &Calendar, e: &EventData, copy: &NewCopy<'_>) -> Result<Value> {
        let (status, reply) = self.send_import(calendar, e, copy, None).await?;
        if !(200..300).contains(&status) {
            return Err(failure(status, &reply));
        }
        parse(&reply)
    }

    async fn send_import(&self, calendar: &Calendar, e: &EventData, copy: &NewCopy<'_>, if_match: Option<&str>) -> Result<(u16, Vec<u8>)> {
        let body = event_body(e, copy, &self.account.address, tz::here());
        let url = format!("{BASE}/calendars/{}/events/import", encode(&calendar.remote_id));
        self.request(reqwest::Method::POST, &url, Some(&body), if_match).await
    }
}

/// The imports that write `copy` over `target`, each with the ETag Google
/// must still hold for the event it replaces: the series or single event
/// Jolta read, and each changed occurrence stored with it in `occurrences`.
/// Google checks a changed occurrence against its own ETag, so one the
/// copy holds unchanged has none to give. None when a changed occurrence is
/// stored without its ETag.
fn imports<'a>(target: &'a Target, occurrences: &'a [Stored], copy: &'a NewCopy<'a>, here: Tz) -> Result<Option<Vec<(&'a EventData, Option<&'a str>)>>> {
    let etag = target.event.etag.as_deref().ok_or_else(|| Error::Other("Jolta hasn't read this copy back from Google yet. Try again in a minute.".into()))?;
    let repeats = target.event.data.recurrence.is_some();
    let mut out = Vec::new();
    if let Some(e) = copy.master().or_else(|| copy.events.first().filter(|_| !repeats)) {
        out.push((e, Some(etag)));
    }
    if repeats {
        for o in copy.overrides() {
            let Ok(etag) = occurrence_etag(occurrences, o.recurrence_id.as_ref().expect("an override"), here) else { return Ok(None) };
            out.push((o, etag));
        }
    }
    Ok(Some(out))
}

/// The event as `events.import` takes it: the invitation's UID, times in
/// their own zone, repeat rules, organizer and guests, the user's entry
/// with the answer given, and `owner`, the calendar's account, as a guest
/// with the same answer.
pub(crate) fn event_body(e: &EventData, copy: &NewCopy, owner: &str, here: Tz) -> Value {
    let mut v = json!({
        "iCalUID": copy.uid,
        "privateCopy": true,
        "summary": e.title,
        "location": e.location,
        "description": super::super::with_join_link(e),
        "start": when(&e.span, true, here),
        "end": when(&e.span, false, here),
        "status": match e.status {
            EventStatus::Confirmed => "confirmed",
            EventStatus::Tentative => "tentative",
            EventStatus::Cancelled => "cancelled",
        },
        "sequence": e.sequence,
        "transparency": if e.show_as == ShowAs::Free { "transparent" } else { "opaque" },
    });
    if let Some(o) = &e.organizer {
        v["organizer"] = json!({ "email": o.address, "displayName": o.name });
    }
    let answered = |a: &Attendee| match copy.answer {
        Some(answer) if a.person.address.eq_ignore_ascii_case(copy.me) => answer.response(),
        _ => a.response,
    };
    let mut attendees: Vec<Value> = e
        .attendees
        .iter()
        .map(|a| {
            json!({
                "email": a.person.address,
                "displayName": a.person.name,
                "optional": a.role == Role::Optional,
                "resource": matches!(a.role, Role::Room | Role::Resource),
                "responseStatus": status_word(answered(a)),
            })
        })
        .collect();
    if !e.attendees.iter().any(|a| a.person.address.eq_ignore_ascii_case(owner)) {
        let response = copy.answer.map_or(Response::NeedsAction, Answer::response);
        attendees.push(json!({ "email": owner, "responseStatus": status_word(response) }));
    }
    v["attendees"] = json!(attendees);
    if let Some(rid) = &e.recurrence_id {
        v["originalStartTime"] = original_start(rid, here);
    }
    if let Some(r) = &e.recurrence {
        let lines: Vec<String> = r
            .rules
            .iter()
            .map(|rule| format!("RRULE:{rule}"))
            .chain(r.exdates.iter().map(|m| Prop::moment("EXDATE", m).to_string()))
            .chain(r.rdates.iter().map(|m| Prop::moment("RDATE", m).to_string()))
            .collect();
        v["recurrence"] = json!(lines);
    }
    v
}

fn status_word(r: Response) -> &'static str {
    match r {
        Response::Accepted => "accepted",
        Response::Tentative => "tentative",
        Response::Declined => "declined",
        Response::NeedsAction | Response::Delegated => "needsAction",
    }
}

/// The original start of a changed occurrence, on its series' clock, as
/// Google names the instance it replaces.
fn original_start(rid: &Moment, here: Tz) -> Value {
    match rid {
        Moment::Date(d) => json!({ "date": d.format("%Y-%m-%d").to_string() }),
        Moment::Time(at) => when(&Span::Timed { start: *at, end: *at }, true, here),
    }
}

/// A start or end as Google takes it: a date for an all-day event, else
/// the wall-clock time and its zone; a floating time in the Mac's zone.
fn when(span: &Span, start: bool, here: Tz) -> Value {
    match span {
        Span::AllDay { start: s, end: e } => json!({ "date": (if start { s } else { e }).format("%Y-%m-%d").to_string() }),
        Span::Timed { start: s, end: e } => {
            let Local { wall, zone } = if start { s } else { e };
            let zone = match zone {
                Zone::Tz(tz) => *tz,
                Zone::Floating => here,
            };
            json!({ "dateTime": wall.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": zone.name() })
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::ical;

    const SERIES: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:standup@example.com\r\nSEQUENCE:2\r\n\
        DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY;BYDAY=TU\r\n\
        EXDATE;TZID=Europe/Brussels:20261124T093000\r\nSUMMARY:Standup\r\nLOCATION:Room 4\r\nORGANIZER;CN=Ann:mailto:ann@example.com\r\n\
        ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.example\r\nATTENDEE;ROLE=OPT-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:bob@example.com\r\n\
        END:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn the_import_keeps_the_uid_the_zone_the_rules_and_the_answer() {
        let events = ical::read(SERIES, &[]).unwrap();
        let copy = NewCopy { uid: "standup@example.com", text: SERIES, events: &events, me: "me@icloud.example", answer: Some(Answer::Maybe) };
        let v = event_body(&events[0], &copy, "me@gmail.example", Brussels);
        assert_eq!((v["iCalUID"].clone(), v["privateCopy"].clone()), (json!("standup@example.com"), json!(true)), "nothing done to it reaches the organizer");
        assert_eq!(v["start"], json!({ "dateTime": "2026-11-10T09:30:00", "timeZone": "Europe/Brussels" }));
        assert_eq!(v["recurrence"], json!(["RRULE:FREQ=WEEKLY;BYDAY=TU", "EXDATE;TZID=Europe/Brussels:20261124T093000"]));
        assert_eq!(v["sequence"], 2);
        assert_eq!(v["organizer"]["email"], "ann@example.com");
        let who = |email: &str| v["attendees"].as_array().unwrap().iter().find(|a| a["email"] == email).unwrap().clone();
        assert_eq!(who("me@icloud.example")["responseStatus"], "tentative", "the answer given");
        assert_eq!(who("bob@example.com")["responseStatus"], "accepted", "the others as sent");
        assert_eq!(who("bob@example.com")["optional"], true);
        assert_eq!(who("me@gmail.example")["responseStatus"], "tentative", "the calendar's account, which Google's import needs as a guest");

        let unanswered = NewCopy { answer: None, ..copy };
        let v = event_body(&events[0], &unanswered, "me@gmail.example", Brussels);
        assert_eq!((v["attendees"][0]["responseStatus"].clone(), v["attendees"][2]["responseStatus"].clone()), (json!("needsAction"), json!("needsAction")));
    }

    #[test]
    fn a_changed_occurrence_imports_under_its_original_start() {
        let moved = SERIES
            .replace("RRULE:FREQ=WEEKLY;BYDAY=TU\r\nEXDATE;TZID=Europe/Brussels:20261124T093000\r\n", "RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\nSTATUS:CANCELLED\r\n")
            .replace("DTSTART;TZID=Europe/Brussels:20261110T093000", "DTSTART;TZID=Europe/Brussels:20261117T093000")
            .replace("DTEND;TZID=Europe/Brussels:20261110T094500", "DTEND;TZID=Europe/Brussels:20261117T094500");
        let events = ical::read(&moved, &[]).unwrap();
        let copy = NewCopy { uid: "standup@example.com", text: &moved, events: &events, me: "me@icloud.example", answer: None };
        let v = event_body(&events[0], &copy, "me@gmail.example", Brussels);
        assert_eq!(v["originalStartTime"], json!({ "dateTime": "2026-11-17T09:30:00", "timeZone": "Europe/Brussels" }), "Google files it under the series");
        assert_eq!(v["status"], "cancelled");
        assert!(v.get("recurrence").is_none());
    }

    /// Update writes only over what Jolta read: each import names the ETag
    /// Google must still hold, the series' or the changed occurrence's own.
    #[test]
    fn every_import_of_an_update_is_conditional_where_jolta_holds_an_etag() {
        use crate::calendar::EventId;
        use crate::calendar::rsvp::{Occurrence, Stored};
        let stored = |data: &EventData, etag: Option<&str>| Stored { id: EventId(1), remote_id: "r".into(), etag: etag.map(str::to_string), raw: None, data: data.clone() };
        let series = ical::read(SERIES, &[]).unwrap();
        let moved = SERIES
            .replace("RRULE:FREQ=WEEKLY;BYDAY=TU\r\nEXDATE;TZID=Europe/Brussels:20261124T093000\r\n", "RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\n")
            .replace("20261110T09", "20261117T10");
        let occurrence = ical::read(&moved, &[]).unwrap();
        let copy = |events| NewCopy { uid: "standup@example.com", text: SERIES, events, me: "me@icloud.example", answer: None };
        let conditions = |target: &Target, held: &[Stored], events| {
            imports(target, held, &copy(events), Brussels).map(|list| list.map(|list| list.into_iter().map(|(e, etag)| (e.recurrence_id.is_some(), etag.map(str::to_string))).collect::<Vec<_>>()))
        };

        let whole = Target { event: stored(&series[0], Some("\"1\"")), occurrence: None, me: "me@gmail.example".into() };
        assert_eq!(conditions(&whole, &[], &series).unwrap(), Some(vec![(false, Some("\"1\"".into()))]), "the series Jolta read");
        let rid = occurrence[0].recurrence_id.unwrap();
        let held = [stored(&occurrence[0], Some("\"7\""))];
        let changed = Target { occurrence: Some(Occurrence { recurrence_id: rid, stored: Some(held[0].clone()) }), ..whole.clone() };
        assert_eq!(conditions(&changed, &held, &occurrence).unwrap(), Some(vec![(true, Some("\"7\"".into()))]), "Google checks a changed occurrence against its own ETag");
        let unchanged = Target { occurrence: Some(Occurrence { recurrence_id: rid, stored: None }), ..whole.clone() };
        assert_eq!(conditions(&unchanged, &[], &occurrence).unwrap(), Some(vec![(true, None)]), "an occurrence without an event of its own has no ETag");
        let alone = Target { event: stored(&occurrence[0], Some("\"3\"")), occurrence: None, me: "me@gmail.example".into() };
        assert_eq!(conditions(&alone, &[], &occurrence).unwrap(), Some(vec![(true, Some("\"3\"".into()))]), "an occurrence the copy holds without its series");
        let unread = Target { event: stored(&series[0], None), ..whole.clone() };
        assert!(conditions(&unread, &[], &series).unwrap_err().to_string().contains("hasn't read this copy"), "no write without an ETag to hold it to");

        // A newer series with its changed occurrence: each import over an
        // event Jolta holds carries that event's own ETag.
        let both = [series[0].clone(), occurrence[0].clone()];
        assert_eq!(conditions(&whole, &held, &both).unwrap(), Some(vec![(false, Some("\"1\"".into())), (true, Some("\"7\"".into()))]), "the whole series names no occurrence, and still guards it");
        let unread = [stored(&occurrence[0], None)];
        assert_eq!(conditions(&whole, &unread, &both).unwrap(), None, "a changed occurrence stored without its ETag stops the update before any import");
    }

    #[test]
    fn an_all_day_event_is_dates_and_a_floating_time_takes_the_macs_zone() {
        let all_day = SERIES.replace("DTSTART;TZID=Europe/Brussels:20261110T093000", "DTSTART;VALUE=DATE:20261110").replace("DTEND;TZID=Europe/Brussels:20261110T094500", "DTEND;VALUE=DATE:20261111");
        let events = ical::read(&all_day, &[]).unwrap();
        let copy = NewCopy { uid: "standup@example.com", text: &all_day, events: &events, me: "me@icloud.example", answer: None };
        let v = event_body(&events[0], &copy, "me@gmail.example", Brussels);
        assert_eq!((v["start"].clone(), v["end"].clone()), (json!({ "date": "2026-11-10" }), json!({ "date": "2026-11-11" })));

        let floating = SERIES.replace("DTSTART;TZID=Europe/Brussels:", "DTSTART:").replace("DTEND;TZID=Europe/Brussels:", "DTEND:");
        let events = ical::read(&floating, &[]).unwrap();
        let v = event_body(&events[0], &NewCopy { text: &floating, events: &events, ..copy }, "me@gmail.example", chrono_tz::Asia::Tokyo);
        assert_eq!(v["start"]["timeZone"], "Asia/Tokyo");
    }
}
