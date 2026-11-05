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

use super::super::{NewCopy, Replaced, encode};
use super::{BASE, Google, GoogleEvent, failure, moment, parse};
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
    /// 403, 2026-11-04), so the copy is imported again, guarded by its
    /// ETags. `held` are the changed occurrences Jolta holds of it.
    pub async fn replace(&self, calendar: &Calendar, target: &Target, held: &[Stored], copy: &NewCopy<'_>) -> Result<Replaced> {
        let fallback = Zone::Tz(calendar.zone.as_deref().and_then(tz::zone).unwrap_or(chrono_tz::UTC));
        update(&Importer { google: self, calendar, copy }, target, held, copy, fallback, tz::here()).await
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

/// What an update asks of Google: `Importer` asks it, a test answers it.
trait Import {
    /// Every event in the calendar with the copy's UID, cancelled ones too.
    async fn listed(&self) -> Result<Vec<GoogleEvent>>;
    async fn import(&self, e: &EventData, etag: &str) -> Result<(u16, Vec<u8>)>;
}

/// The imports into `calendar` of `copy`.
struct Importer<'a> {
    google: &'a Google,
    calendar: &'a Calendar,
    copy: &'a NewCopy<'a>,
}

impl Import for Importer<'_> {
    async fn listed(&self) -> Result<Vec<GoogleEvent>> {
        let url = format!("{BASE}/calendars/{}/events?iCalUID={}&showDeleted=true", encode(&self.calendar.remote_id), encode(self.copy.uid));
        let page: Value = parse(&self.google.get_ok(&url).await?)?;
        Ok(page["items"].as_array().into_iter().flatten().filter_map(|item| serde_json::from_value(item.clone()).ok()).collect())
    }

    async fn import(&self, e: &EventData, etag: &str) -> Result<(u16, Vec<u8>)> {
        self.google.send_import(self.calendar, e, self.copy, Some(etag)).await
    }
}

/// What an import over an event of the copy names as the ETag Google must
/// still hold.
#[derive(Clone, Debug, PartialEq)]
enum Guard {
    /// The event's own: the series or single event, or a changed occurrence.
    Own(String),
    /// The series' as it stands, which Google gives each occurrence that has
    /// no event of its own.
    Series,
}

/// Writes `copy` over `target` while Google holds what Jolta read of it.
/// Google gives the series and each changed occurrence of a private copy an
/// ETag of its own, and an occurrence without an event of its own the
/// series' ETag; an import of the series changes only the series' ETag
/// (2026-11-05). So one listing first finds a change Jolta hasn't read to
/// any event the update writes, also an occurrence changed that Jolta holds
/// unchanged, and nothing is written. Each import still carries its ETag,
/// which `events.import` honours with a 412.
async fn update(google: &impl Import, target: &Target, held: &[Stored], copy: &NewCopy<'_>, fallback: Zone, here: Tz) -> Result<Replaced> {
    let read = target.event.etag.clone().ok_or_else(|| Error::Other("Jolta hasn't read this copy back from Google yet. Try again in a minute.".into()))?;
    let listed = google.listed().await?;
    let Some(imports) = imports(target, held, &listed, copy, fallback, here) else { return Ok(Replaced::Changed) };
    let mut series = read;
    for (e, guard) in imports {
        let etag = match guard {
            Guard::Own(etag) => etag,
            Guard::Series => series.clone(),
        };
        let (status, reply) = google.import(e, &etag).await?;
        match status {
            200..300 if e.recurrence_id.is_none() => {
                let written: Value = parse(&reply)?;
                series = written["etag"].as_str().ok_or_else(|| Error::Other("Google returned no ETag for the imported event.".into()))?.to_string();
            }
            200..300 => {}
            412 => return Ok(Replaced::Changed),
            404 | 410 => return Err(Error::Other("The meeting is no longer in your calendar.".into())),
            _ => return Err(failure(status, &reply)),
        }
    }
    Ok(Replaced::Written(None))
}

/// The imports that write `copy` over `target`, each with its guard: the
/// series or single event, then a series' changed occurrences. None when
/// the events Google `listed` of the copy differ from what Jolta holds for
/// one of them: another ETag, or a changed occurrence only one side has.
fn imports<'a>(target: &Target, held: &[Stored], listed: &[GoogleEvent], copy: &'a NewCopy<'a>, fallback: Zone, here: Tz) -> Option<Vec<(&'a EventData, Guard)>> {
    let object = target.event.remote_id.as_str();
    let on_google = |rid: Option<&Moment>| {
        let found = listed.iter().find(|g| match rid {
            None => g.id == object,
            Some(rid) => {
                g.id != object
                    && g.recurring_event_id.as_deref() == Some(object)
                    && g.original_start_time.as_ref().and_then(|w| moment(w, fallback)).is_some_and(|m| m.names(rid, here))
            }
        });
        found.map(|g| g.etag.clone())
    };
    let read = target.event.etag.clone()?;
    if on_google(None) != Some(Some(read.clone())) {
        return None;
    }
    let repeats = target.event.data.recurrence.is_some();
    let mut out: Vec<(&EventData, Guard)> = copy.master().or_else(|| copy.events.first().filter(|_| !repeats)).map(|e| (e, Guard::Own(read))).into_iter().collect();
    for o in copy.overrides().filter(|_| repeats) {
        let rid = o.recurrence_id.as_ref().expect("an override");
        let ours = held.iter().find(|s| s.data.recurrence_id.is_some_and(|r| r.names(rid, here))).map(|s| s.etag.clone());
        match (ours, on_google(Some(rid))) {
            (None, None) => out.push((o, Guard::Series)),
            (Some(Some(ours)), Some(Some(theirs))) if ours == theirs => out.push((o, Guard::Own(ours))),
            _ => return None,
        }
    }
    Some(out)
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

    /// Google as a test answers it: it lists `listed`, `replies` answer the
    /// imports in turn with a new ETag, and each import is noted as whether
    /// it is an occurrence and its If-Match.
    struct Fake {
        listed: Vec<Value>,
        replies: std::sync::Mutex<std::collections::VecDeque<u16>>,
        seen: std::sync::Mutex<Vec<(bool, String)>>,
    }

    impl Import for Fake {
        async fn listed(&self) -> Result<Vec<GoogleEvent>> {
            Ok(self.listed.iter().map(|v| serde_json::from_value(v.clone()).unwrap()).collect())
        }

        async fn import(&self, e: &EventData, etag: &str) -> Result<(u16, Vec<u8>)> {
            let mut seen = self.seen.lock().unwrap();
            seen.push((e.recurrence_id.is_some(), etag.to_string()));
            Ok((self.replies.lock().unwrap().pop_front().expect("a reply"), format!(r#"{{"etag": "\"{}\""}}"#, seen.len() + 1).into_bytes()))
        }
    }

    /// The imports an update of `target` to `events` sends while Jolta holds
    /// `held` and Google lists `listed`, then answers `replies`.
    async fn sent(target: &Target, held: &[Stored], listed: Vec<Value>, events: &[EventData], replies: &[u16]) -> (Replaced, Vec<(bool, String)>) {
        let copy = NewCopy { uid: "standup@example.com", text: SERIES, events, me: "me@icloud.example", answer: None };
        let fake = Fake { listed, replies: std::sync::Mutex::new(replies.iter().copied().collect()), seen: std::sync::Mutex::new(Vec::new()) };
        let done = update(&fake, target, held, &copy, Zone::Tz(Brussels), Brussels).await.unwrap();
        (done, fake.seen.into_inner().unwrap())
    }

    /// Google gives the series and each changed occurrence of a private
    /// copy an ETag of its own, and an occurrence without one the series'.
    /// Update writes only while Google lists what Jolta holds for every
    /// event it writes, so a change made on Google to an occurrence Jolta
    /// holds unchanged stops it before anything is written.
    #[tokio::test]
    async fn an_update_writes_only_over_what_jolta_holds_of_every_event_it_writes() {
        use crate::calendar::EventId;
        let stored = |data: &EventData, remote: &str, etag: &str| Stored { id: EventId(1), remote_id: remote.into(), etag: Some(etag.into()), raw: None, data: data.clone() };
        let at = |day: u32| SERIES.replace("RRULE:FREQ=WEEKLY;BYDAY=TU\r\nEXDATE;TZID=Europe/Brussels:20261124T093000\r\n", &format!("RECURRENCE-ID;TZID=Europe/Brussels:202611{day}T093000\r\n")).replace("20261110T09", &format!("202611{day}T10"));
        let series = ical::read(SERIES, &[]).unwrap().remove(0);
        let (moved, later) = (ical::read(&at(17), &[]).unwrap().remove(0), ical::read(&at(24), &[]).unwrap().remove(0));
        let whole = Target { event: stored(&series, "s1", "\"1\""), occurrence: None, me: "me@gmail.example".into() };
        let held = [stored(&moved, "s1_20261117T083000Z", "\"7\"")];
        let google = |id: &str, etag: &str, day: Option<u32>| match day {
            None => json!({ "id": id, "etag": etag }),
            Some(day) => json!({ "id": id, "etag": etag, "recurringEventId": id.split('_').next(), "originalStartTime": { "dateTime": format!("2026-11-{day}T09:30:00+01:00") } }),
        };
        let as_held = || vec![google("s1", "\"1\"", None), google("s1_20261117T083000Z", "\"7\"", Some(17)), google("other_20261124T083000Z", "\"9\"", Some(24))];
        let newer = [series.clone(), moved.clone(), later.clone()];
        let written = |seen: &[(bool, &str)]| seen.iter().map(|(o, e)| (*o, e.to_string())).collect::<Vec<_>>();

        let (done, seen) = sent(&whole, &held, as_held(), &newer, &[200, 200, 200]).await;
        assert_eq!(done, Replaced::Written(None), "another event's changed 24th is not the copy's");
        assert_eq!(seen, written(&[(false, "\"1\""), (true, "\"7\""), (true, "\"2\"")]), "the series, its changed 17th by its own ETag, the 24th by the series' as its import left it");

        let changed_by_hand = [as_held(), vec![google("s1_20261124T083000Z", "\"8\"", Some(24))]].concat();
        assert_eq!(sent(&whole, &held, changed_by_hand, &newer, &[]).await, (Replaced::Changed, vec![]), "the 24th, which Jolta holds unchanged, was changed on Google");
        let listed = vec![google("s1", "\"1\"", None), google("s1_20261117T083000Z", "\"8\"", Some(17))];
        assert_eq!(sent(&whole, &held, listed, &newer, &[]).await, (Replaced::Changed, vec![]), "the changed 17th changed again");
        let listed = vec![google("s1", "\"5\"", None), google("s1_20261117T083000Z", "\"7\"", Some(17))];
        assert_eq!(sent(&whole, &held, listed, &newer, &[]).await, (Replaced::Changed, vec![]), "the series changed");
        assert_eq!(sent(&whole, &[], as_held(), &newer, &[]).await, (Replaced::Changed, vec![]), "a changed 17th Jolta doesn't hold");

        let (done, seen) = sent(&whole, &held, as_held(), std::slice::from_ref(&later), &[412]).await;
        assert_eq!((done, seen), (Replaced::Changed, written(&[(true, "\"1\"")])), "an occurrence alone, held unchanged, carries the series' ETag");
        let alone = Target { event: stored(&moved, "o1", "\"3\""), ..whole };
        let (done, seen) = sent(&alone, &[], vec![google("o1", "\"3\"", None)], std::slice::from_ref(&moved), &[200]).await;
        assert_eq!((done, seen), (Replaced::Written(None), written(&[(true, "\"3\"")])), "an occurrence the copy holds without its series");
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
