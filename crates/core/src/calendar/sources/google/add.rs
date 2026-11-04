//! Add to calendar and Update in your calendar on Google: `events.import`,
//! which keeps the invitation's UID as `iCalUID` and emails nobody, then
//! patches with `sendUpdates=none` and the event's ETag.

use chrono_tz::Tz;
use serde_json::{Value, json};

use super::super::{NewCopy, Replaced, encode};
use super::{BASE, Google, failure, parse};
use crate::calendar::ical_edit::Prop;
use crate::calendar::rsvp::Target;
use crate::calendar::{Calendar, EventData, EventStatus, Local, Response, Role, ShowAs, Span, Zone, tz};
use crate::{Error, Result};

impl Google {
    /// Imports the invitation's series or single event, then gives its
    /// changed occurrences their own times. A calendar that already holds
    /// the UID, as after a retry whose answer was lost, keeps that event.
    pub async fn create(&self, calendar: &Calendar, copy: &NewCopy<'_>) -> Result<String> {
        let here = tz::here();
        let cal = encode(&calendar.remote_id);
        let found: Value = parse(&self.get_ok(&format!("{BASE}/calendars/{cal}/events?iCalUID={}", encode(copy.uid))).await?)?;
        if let Some(id) = found["items"].as_array().into_iter().flatten().find(|e| e["status"] != "cancelled").and_then(|e| e["id"].as_str()) {
            return Ok(id.to_string());
        }
        let first = copy.master().or(copy.events.first()).ok_or_else(|| Error::Other("The invitation has no event to add.".into()))?;
        let (status, body) = self.request(reqwest::Method::POST, &format!("{BASE}/calendars/{cal}/events/import"), Some(&event_body(first, copy, here)), None).await?;
        if !(200..300).contains(&status) {
            return Err(failure(status, &body));
        }
        let created: Value = parse(&body)?;
        let id = created["id"].as_str().ok_or_else(|| Error::Other("Google returned no id for the imported event.".into()))?.to_string();
        if copy.master().is_some() {
            for o in copy.overrides() {
                self.write_occurrence(calendar, &id, o, here).await?;
            }
        }
        Ok(id)
    }

    /// Patches the copy with the newer version, guarded by its ETag, and
    /// its changed occurrences by theirs.
    pub async fn replace(&self, calendar: &Calendar, target: &Target, copy: &NewCopy<'_>) -> Result<Replaced> {
        let here = tz::here();
        let url = format!("{BASE}/calendars/{}/events/{}", encode(&calendar.remote_id), encode(&target.event.remote_id));
        if let Some(master) = copy.master() {
            let etag = match &target.event.etag {
                Some(etag) => etag.clone(),
                None => {
                    let now: Value = parse(&self.get_ok(&url).await?)?;
                    if now["sequence"].as_i64().unwrap_or(0) != target.event.data.sequence || now["status"] == "cancelled" {
                        return Ok(Replaced::Changed);
                    }
                    now["etag"].as_str().map(str::to_string).ok_or_else(|| Error::Other("Google gave the event no ETag.".into()))?
                }
            };
            let mut body = event_body(master, copy, here);
            body.as_object_mut().expect("an object").remove("iCalUID");
            let (status, reply) = self.request(reqwest::Method::PATCH, &format!("{url}?sendUpdates=none"), Some(&body), Some(&etag)).await?;
            match status {
                200..300 => {}
                412 => return Ok(Replaced::Changed),
                _ => return Err(failure(status, &reply)),
            }
        }
        let series = if target.event.data.recurrence.is_some() { Some(target.event.remote_id.as_str()) } else { None };
        for o in copy.overrides() {
            match series {
                Some(id) => self.write_occurrence(calendar, id, o, here).await?,
                // Invited to one occurrence only: the copy is that event.
                None => {
                    let etag = target.event.etag.as_deref();
                    let (status, reply) = self.request(reqwest::Method::PATCH, &format!("{url}?sendUpdates=none"), Some(&occurrence_body(o, here)), etag).await?;
                    match status {
                        200..300 => {}
                        412 => return Ok(Replaced::Changed),
                        _ => return Err(failure(status, &reply)),
                    }
                }
            }
        }
        Ok(Replaced::Written(None))
    }

    /// Gives one occurrence of the series `master` the time, place and
    /// status of the changed occurrence `o`, with that instance's ETag.
    async fn write_occurrence(&self, calendar: &Calendar, master: &str, o: &EventData, here: Tz) -> Result<()> {
        let rid = o.recurrence_id.as_ref().expect("an override");
        let id = match self.instance(calendar, master, rid).await {
            Ok(id) => id,
            // Not an occurrence the series makes: nothing to change.
            Err(Error::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let url = format!("{BASE}/calendars/{}/events/{}", encode(&calendar.remote_id), encode(&id));
        let now: Value = parse(&self.get_ok(&url).await?)?;
        let (status, reply) = self.request(reqwest::Method::PATCH, &format!("{url}?sendUpdates=none"), Some(&occurrence_body(o, here)), now["etag"].as_str()).await?;
        if !(200..300).contains(&status) {
            return Err(failure(status, &reply));
        }
        Ok(())
    }
}

/// The event as `events.import` takes it: the invitation's UID, times in
/// their own zone, repeat rules, organizer and guests, the user's entry
/// with the answer given.
pub(crate) fn event_body(e: &EventData, copy: &NewCopy, here: Tz) -> Value {
    let mut v = json!({
        "iCalUID": copy.uid,
        "summary": e.title,
        "location": e.location,
        "description": super::super::with_join_link(e),
        "start": when(&e.span, true, here),
        "end": when(&e.span, false, here),
        "status": if e.status == EventStatus::Tentative { "tentative" } else { "confirmed" },
        "sequence": e.sequence,
        "transparency": if e.show_as == ShowAs::Free { "transparent" } else { "opaque" },
    });
    if let Some(o) = &e.organizer {
        v["organizer"] = json!({ "email": o.address, "displayName": o.name });
    }
    let attendees: Vec<Value> = e
        .attendees
        .iter()
        .map(|a| {
            let me = a.person.address.eq_ignore_ascii_case(copy.me);
            let response = match (me, copy.answer) {
                (true, Some(answer)) => answer.response(),
                _ => a.response,
            };
            json!({
                "email": a.person.address,
                "displayName": a.person.name,
                "optional": a.role == Role::Optional,
                "resource": matches!(a.role, Role::Room | Role::Resource),
                "responseStatus": status_word(response),
            })
        })
        .collect();
    if !attendees.is_empty() {
        v["attendees"] = json!(attendees);
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

/// What a changed occurrence changes on its instance.
fn occurrence_body(o: &EventData, here: Tz) -> Value {
    json!({
        "summary": o.title,
        "location": o.location,
        "description": super::super::with_join_link(o),
        "start": when(&o.span, true, here),
        "end": when(&o.span, false, here),
        "status": if o.status == EventStatus::Cancelled { "cancelled" } else { "confirmed" },
    })
}

fn status_word(r: Response) -> &'static str {
    match r {
        Response::Accepted => "accepted",
        Response::Tentative => "tentative",
        Response::Declined => "declined",
        Response::NeedsAction | Response::Delegated => "needsAction",
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
    use crate::calendar::rsvp::Answer;

    const SERIES: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:standup@example.com\r\nSEQUENCE:2\r\n\
        DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY;BYDAY=TU\r\n\
        EXDATE;TZID=Europe/Brussels:20261124T093000\r\nSUMMARY:Standup\r\nLOCATION:Room 4\r\nORGANIZER;CN=Ann:mailto:ann@example.com\r\n\
        ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.example\r\nATTENDEE;ROLE=OPT-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:bob@example.com\r\n\
        END:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn the_import_keeps_the_uid_the_zone_the_rules_and_the_answer() {
        let events = ical::read(SERIES, &[]).unwrap();
        let copy = NewCopy { uid: "standup@example.com", text: SERIES, events: &events, me: "me@icloud.example", answer: Some(Answer::Maybe) };
        let v = event_body(&events[0], &copy, Brussels);
        assert_eq!(v["iCalUID"], "standup@example.com");
        assert_eq!(v["start"], json!({ "dateTime": "2026-11-10T09:30:00", "timeZone": "Europe/Brussels" }));
        assert_eq!(v["recurrence"], json!(["RRULE:FREQ=WEEKLY;BYDAY=TU", "EXDATE;TZID=Europe/Brussels:20261124T093000"]));
        assert_eq!(v["sequence"], 2);
        assert_eq!(v["organizer"]["email"], "ann@example.com");
        let who = |email: &str| v["attendees"].as_array().unwrap().iter().find(|a| a["email"] == email).unwrap().clone();
        assert_eq!(who("me@icloud.example")["responseStatus"], "tentative", "the answer given");
        assert_eq!(who("bob@example.com")["responseStatus"], "accepted", "the others as sent");
        assert_eq!(who("bob@example.com")["optional"], true);

        let unanswered = NewCopy { answer: None, ..copy };
        assert_eq!(event_body(&events[0], &unanswered, Brussels)["attendees"][0]["responseStatus"], "needsAction");
    }

    #[test]
    fn an_all_day_event_is_dates_and_a_floating_time_takes_the_macs_zone() {
        let all_day = SERIES.replace("DTSTART;TZID=Europe/Brussels:20261110T093000", "DTSTART;VALUE=DATE:20261110").replace("DTEND;TZID=Europe/Brussels:20261110T094500", "DTEND;VALUE=DATE:20261111");
        let events = ical::read(&all_day, &[]).unwrap();
        let copy = NewCopy { uid: "standup@example.com", text: &all_day, events: &events, me: "me@icloud.example", answer: None };
        let v = event_body(&events[0], &copy, Brussels);
        assert_eq!((v["start"].clone(), v["end"].clone()), (json!({ "date": "2026-11-10" }), json!({ "date": "2026-11-11" })));

        let floating = SERIES.replace("DTSTART;TZID=Europe/Brussels:", "DTSTART:").replace("DTEND;TZID=Europe/Brussels:", "DTEND:");
        let events = ical::read(&floating, &[]).unwrap();
        let v = event_body(&events[0], &NewCopy { text: &floating, events: &events, ..copy }, chrono_tz::Asia::Tokyo);
        assert_eq!(v["start"]["timeZone"], "Asia/Tokyo");
    }
}
