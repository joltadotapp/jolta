//! Add to calendar and Update in your calendar on Google: `events.import`,
//! which keeps the invitation's UID as `iCalUID` and emails nobody, then
//! patches with `sendUpdates=none` and the event's ETag. The import is a
//! private copy (`privateCopy`), so nothing done to it later reaches the
//! organizer either. Google imports an event only into the calendar of its
//! organizer or a guest (its 400 on the Microsoft 365 test account's
//! invitation, 2026-11-04), so the calendar's own account joins the guests
//! with the user's answer.

use chrono_tz::Tz;
use serde_json::{Value, json};

use super::super::{NewCopy, Replaced, encode};
use super::{BASE, Google, failure, parse};
use crate::calendar::ical_edit::Prop;
use crate::calendar::rsvp::{Answer, Target};
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
    /// 403, 2026-11-04), and the import takes no ETag, so the copy is read
    /// first and left alone unless it is still the version Jolta read.
    pub async fn replace(&self, calendar: &Calendar, target: &Target, copy: &NewCopy<'_>) -> Result<Replaced> {
        let url = format!("{BASE}/calendars/{}/events/{}", encode(&calendar.remote_id), encode(&target.event.remote_id));
        let (status, body) = self.get(&url).await?;
        if matches!(status, 404 | 410) {
            return Err(Error::Other("The meeting is no longer in your calendar.".into()));
        }
        if !(200..300).contains(&status) {
            return Err(failure(status, &body));
        }
        let now: Value = parse(&body)?;
        let same = match &target.event.etag {
            Some(etag) => now["etag"].as_str() == Some(etag.as_str()),
            None => now["sequence"].as_i64().unwrap_or(0) == target.event.data.sequence,
        };
        if !same || now["status"] == "cancelled" {
            return Ok(Replaced::Changed);
        }
        let whole = copy.master().or_else(|| copy.events.first().filter(|_| target.event.data.recurrence.is_none()));
        if let Some(e) = whole {
            self.import(calendar, e, copy).await?;
        }
        if target.event.data.recurrence.is_some() {
            for o in copy.overrides() {
                self.import(calendar, o, copy).await?;
            }
        }
        Ok(Replaced::Written(None))
    }

    async fn import(&self, calendar: &Calendar, e: &EventData, copy: &NewCopy<'_>) -> Result<Value> {
        let body = event_body(e, copy, &self.account.address, tz::here());
        let url = format!("{BASE}/calendars/{}/events/import", encode(&calendar.remote_id));
        let (status, reply) = self.request(reqwest::Method::POST, &url, Some(&body), None).await?;
        if !(200..300).contains(&status) {
            return Err(failure(status, &reply));
        }
        parse(&reply)
    }
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
