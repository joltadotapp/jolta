//! Looks up what Find a time shows: the user's busy times, other people's
//! through the provider of the account that asks, and the working hours.
//! Reads only; nothing here writes to a calendar or sends mail.
//!
//! The user's busy times are the occurrences in every shown calendar that
//! counts as busy, joined with the provider's own answer for the asking
//! account's address on Google and Microsoft 365. A data folder holding
//! `free-busy.json` (the demo and UI tests) answers from that file in
//! place of the provider.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use serde::Deserialize;

use super::availability::{Availability, Busy, BusyKind, Schedule, Unknown, Window, WorkingHours};
use super::{Bounds, Person, Response, ShowAs, invitation, sync, tz};
use crate::model::{MessageId, ThreadId};
use crate::store::{self, OccurrenceRow};
use crate::{Account, AccountId, AccountKind, Error, Jolta, Result, compose, imap};

/// What Find a time draws and suggests from.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub account: AccountId,
    /// The asking account's address, the user's row.
    pub me: String,
    pub own: Vec<Busy>,
    pub others: Vec<Schedule>,
    pub hours: WorkingHours,
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Lookup {
    Found(Found),
    /// Other people can be looked up only through an account with calendar
    /// on. `needs_consent`: turning it on opens the browser first.
    CalendarOff { account: AccountId, address: String, needs_consent: bool },
}

/// The answers `free-busy.json` holds, by address: busy times, each
/// person's working hours, and the user's.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recorded {
    #[serde(default)]
    people: BTreeMap<String, Availability>,
    #[serde(default)]
    people_hours: BTreeMap<String, WorkingHours>,
    #[serde(default)]
    working_hours: Option<WorkingHours>,
}

/// Looks up `people` through `account`, the account's own address aside,
/// for the next `days` working days after `from`. With nobody to look up,
/// as for Insert my availability, an account with calendar off still gives
/// the user's own busy times.
pub async fn look_up(jolta: &Jolta, account: AccountId, people: &[String], from: DateTime<Utc>, days: u32) -> Result<Lookup> {
    let (a, chosen): (Account, Option<WorkingHours>) =
        jolta.store.read_async(move |c| Ok((store::account(c, account)?, store::working_hours(c)?))).await?;
    let people: Vec<String> = people.iter().filter(|p| !p.eq_ignore_ascii_case(&a.address)).cloned().collect();
    if !a.calendar && !people.is_empty() {
        return Ok(Lookup::CalendarOff { account, address: a.address.clone(), needs_consent: sync::needs_consent(&a) });
    }
    let recorded = recorded(&jolta.data_dir)?;
    // CalDAV's scheduling outbox (RFC 6638) isn't asked yet.
    let asks_provider = a.calendar && matches!(a.kind, AccountKind::Gmail | AccountKind::Graph);
    let source = match (&recorded, asks_provider) {
        (None, true) => Some(sync::account_source(jolta, &a).await?),
        _ => None,
    };
    let provider_hours = match (&recorded, &source) {
        (Some(r), _) => r.working_hours,
        (None, Some(s)) => s.working_hours().await.unwrap_or(None),
        (None, None) => None,
    };
    let hours = chosen.or(provider_hours).unwrap_or_else(|| WorkingHours::standard(tz::here()));
    let windows = hours.windows(from, days);
    let (Some(first), Some(last)) = (windows.first(), windows.last()) else {
        return Ok(Lookup::Found(Found { account, me: a.address, own: Vec::new(), others: Vec::new(), hours, windows }));
    };
    let (start, end) = (first.start, last.end);

    let mut asked = people.clone();
    asked.push(a.address.clone());
    let mut answers = match (&recorded, &source) {
        (Some(r), _) => asked.iter().map(|p| recorded_answer(r, p)).collect(),
        (None, Some(s)) => s.free_busy(&asked, start, end).await?,
        (None, None) => people.iter().map(|p| Schedule::new(p.clone(), Availability::Unknown(Unknown::NotSupported))).collect(),
    };
    let mine = answers.iter().position(|s| s.person == a.address).map(|i| answers.remove(i));

    let here = tz::here();
    let (mut own, of_account): (Vec<Busy>, Vec<Busy>) = jolta
        .store
        .read_async(move |c| {
            let (first_day, last_day) = (start.with_timezone(&here).date_naive(), end.with_timezone(&here).date_naive() + Duration::days(1));
            let calendars: Vec<_> = store::calendars(c)?.into_iter().filter(|cal| cal.account_id == Some(account)).map(|cal| cal.id).collect();
            let mut own = Vec::new();
            let mut of_account = Vec::new();
            for o in store::occurrences(c, first_day, last_day, here)? {
                if let Some(b) = stored_busy(&o, here) {
                    if calendars.contains(&o.calendar_id) {
                        of_account.push(b);
                    }
                    own.push(b);
                }
            }
            Ok((own, of_account))
        })
        .await?;
    if let Some(Schedule { availability: Availability::Known(busy), .. }) = mine {
        own.extend(uncovered(busy, of_account));
    }
    own.retain(|b| b.end > start && b.start < end);
    own.sort_by_key(|b| (b.start, b.end));
    Ok(Lookup::Found(Found { account, me: a.address, own, others: answers, hours, windows }))
}

/// When each room in `rooms` is busy from `start` to `end`, in that order,
/// through `account`'s provider: Microsoft 365's `getSchedule` and Google's
/// `freeBusy` take a room's address like a person's. A room's working hours
/// say nothing about when it can be booked, so it has none. A recording
/// answers in place of the provider, as for people.
pub async fn room_schedules(jolta: &Jolta, account: AccountId, rooms: &[String], start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<Schedule>> {
    if rooms.is_empty() || end <= start {
        return Ok(Vec::new());
    }
    let a = jolta.store.read_async(move |c| store::account(c, account)).await?;
    let unknown = |why: Unknown| rooms.iter().map(|r| Schedule::new(r.clone(), Availability::Unknown(why.clone()))).collect();
    let mut out: Vec<Schedule> = match recorded(&jolta.data_dir)? {
        Some(r) => rooms.iter().map(|room| recorded_answer(&r, room)).collect(),
        None if !a.calendar => unknown(Unknown::Failed(format!("Turn calendar on for {} to see when rooms are free", a.address))),
        None if matches!(a.kind, AccountKind::Gmail | AccountKind::Graph) => sync::account_source(jolta, &a).await?.free_busy(rooms, start, end).await?,
        None => unknown(Unknown::NotSupported),
    };
    out.iter_mut().for_each(|s| s.hours = None);
    Ok(out)
}

fn recorded(dir: &Path) -> Result<Option<Recorded>> {
    match std::fs::read(dir.join("free-busy.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| Error::Other(format!("free-busy.json: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn recorded_answer(r: &Recorded, person: &str) -> Schedule {
    let found = r.people.iter().find(|(p, _)| p.eq_ignore_ascii_case(person)).map(|(_, a)| a.clone());
    let hours = r.people_hours.iter().find(|(p, _)| p.eq_ignore_ascii_case(person)).map(|(_, h)| *h);
    Schedule { hours, ..Schedule::new(person, found.unwrap_or(Availability::Unknown(Unknown::NotFound))) }
}

/// The provider's busy blocks for the account's own address that the
/// account's stored blocking occurrences don't cover completely. Google's
/// free/busy gives an out-of-office event as a plain busy block, so the
/// stored occurrence's kind wins; a block nothing stored covers comes from a
/// calendar Jolta doesn't show, and stays.
fn uncovered(provider: Vec<Busy>, mut stored: Vec<Busy>) -> Vec<Busy> {
    stored.sort_by_key(|b| b.start);
    let covered = |b: &Busy| {
        let mut reached = b.start;
        for s in &stored {
            if s.start > reached {
                break;
            }
            reached = reached.max(s.end);
            if reached >= b.end {
                return true;
            }
        }
        false
    };
    provider.into_iter().filter(|b| !covered(b)).collect()
}

/// An occurrence that blocks the user's time, as a busy block. An all-day
/// one blocks its days from midnight in `here`.
pub(super) fn stored_busy(o: &OccurrenceRow, here: Tz) -> Option<Busy> {
    if !o.busy {
        return None;
    }
    let (start, end) = match o.bounds {
        Bounds::Utc { start, end } => (start, end),
        Bounds::Days { start, end } => {
            let midnight = |d: i32| tz::instant((chrono::NaiveDate::default() + Duration::days(d.into())).and_time(chrono::NaiveTime::MIN), here);
            (midnight(start), midnight(end))
        }
    };
    let kind = match o.show_as {
        ShowAs::OutOfOffice => BusyKind::OutOfOffice,
        ShowAs::Elsewhere => BusyKind::Elsewhere,
        ShowAs::Tentative => BusyKind::Tentative,
        _ if o.my_response == Some(Response::Tentative) => BusyKind::Tentative,
        _ => BusyKind::Busy,
    };
    Some(Busy { start, end, kind })
}

/// The people on a conversation, From, To and Cc of every message without
/// the user's own addresses, and the account of its newest message.
pub fn thread_people(conn: &rusqlite::Connection, thread: ThreadId) -> Result<(AccountId, Vec<Person>)> {
    let every: Vec<String> = store::accounts(conn)?.into_iter().map(|a| a.address).collect();
    let mut stmt = conn.prepare_cached("SELECT account_id, from_name, from_addr, to_addrs, cc_addrs FROM message WHERE thread_id = ?1 ORDER BY id")?;
    let rows = stmt.query_map([thread], |r| {
        Ok((r.get::<_, AccountId>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
    })?;
    let mut account = None;
    let mut people: Vec<Person> = Vec::new();
    for row in rows {
        let (id, from_name, from_addr, to, cc) = row?;
        account = Some(id);
        let listed = std::iter::once((from_name, from_addr)).chain(compose::parse_addresses(&to)).chain(compose::parse_addresses(&cc));
        for (name, address) in listed {
            add_person(&mut people, &every, Person { address, name });
        }
    }
    let account = account.ok_or_else(|| Error::NotFound(format!("conversation {thread}")))?;
    Ok((account, people))
}

/// Who a proposal from Find a time goes to, as To and Cc lines: the
/// people `thread_people` looks up, the newest sender among them in To.
pub fn proposal_recipients(conn: &rusqlite::Connection, thread: ThreadId) -> Result<(String, String)> {
    let (_, mut people) = thread_people(conn, thread)?;
    let mut stmt = conn.prepare_cached("SELECT from_addr FROM message WHERE thread_id = ?1 ORDER BY id DESC")?;
    let senders = stmt.query_map([thread], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let to = match senders.iter().find_map(|s| people.iter().position(|p| p.address.eq_ignore_ascii_case(s))) {
        Some(newest) => vec![people.remove(newest)],
        None => std::mem::take(&mut people),
    };
    let line = |list: &[Person]| compose::format_addresses(&list.iter().map(|p| (p.name.clone(), p.address.clone())).collect::<Vec<_>>());
    Ok((line(&to), line(&people)))
}

/// The organizer and guests of the invitation on a message, without the
/// user's own addresses, and the account it came to.
pub async fn invitation_people(jolta: &Jolta, message: MessageId) -> Result<(AccountId, Vec<Person>)> {
    let (account, every): (AccountId, Vec<String>) = jolta
        .store
        .read_async(move |c| {
            let account = c.query_row("SELECT account_id FROM message WHERE id = ?1", [message], |r| r.get(0))?;
            Ok((account, store::accounts(c)?.into_iter().map(|a| a.address).collect()))
        })
        .await?;
    let raw = imap::message_source(jolta, message).await?;
    let found = invitation::find(&raw, &every);
    let event = &found.first().ok_or_else(|| Error::Other("This message has no invitation.".into()))?.event;
    let mut people = Vec::new();
    for p in event.organizer.iter().chain(event.attendees.iter().filter(|a| !a.is_me).map(|a| &a.person)) {
        add_person(&mut people, &every, p.clone());
    }
    Ok((account, people))
}

fn add_person(people: &mut Vec<Person>, mine: &[String], p: Person) {
    let taken = |a: &str| mine.iter().chain(people.iter().map(|q| &q.address)).any(|m| m.eq_ignore_ascii_case(a));
    if !p.address.contains('@') || taken(&p.address) {
        return;
    }
    people.push(Person { address: p.address.to_ascii_lowercase(), name: p.name });
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::availability::{self, Weekdays};
    use crate::calendar::invitation::tests::{ME, import, jolta};

    #[test]
    fn the_people_on_a_conversation_without_the_user() {
        let (j, _) = jolta("find-time-people");
        let first = import(&j, format!("From: Ann Berg <ann@example.com>\r\nTo: {ME}, Bo <bo@example.com>\r\nCc: ANN@example.com\r\nSubject: Plan\r\nMessage-ID: <a@example.com>\r\n\r\nHi\r\n").as_bytes());
        import(&j, format!("From: Me <{ME}>\r\nTo: ann@example.com\r\nCc: Cy <cy@example.com>\r\nSubject: Re: Plan\r\nIn-Reply-To: <a@example.com>\r\nMessage-ID: <b@example.com>\r\n\r\nYes\r\n").as_bytes());
        let thread: ThreadId = j.store.read(|c| Ok(c.query_row("SELECT thread_id FROM message WHERE id = ?1", [first], |r| r.get(0))?)).unwrap();
        let (_, people) = j.store.read(|c| thread_people(c, thread)).unwrap();
        assert_eq!(
            people.iter().map(|p| (p.address.as_str(), p.name.as_str())).collect::<Vec<_>>(),
            [("ann@example.com", "Ann Berg"), ("bo@example.com", "Bo"), ("cy@example.com", "Cy")]
        );
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_proposal_goes_to_everyone_looked_up_with_the_newest_other_sender_in_to() {
        let (j, _) = jolta("find-time-recipients");
        let first = import(&j, format!("From: Ann Berg <ann@example.com>\r\nTo: {ME}, Bo <bo@example.com>\r\nSubject: Plan\r\nMessage-ID: <a@example.com>\r\n\r\nHi\r\n").as_bytes());
        import(&j, format!("From: Dee <dee@example.com>\r\nTo: {ME}\r\nSubject: Re: Plan\r\nIn-Reply-To: <a@example.com>\r\nMessage-ID: <b@example.com>\r\n\r\nMe too\r\n").as_bytes());
        import(&j, format!("From: Me <{ME}>\r\nTo: ann@example.com\r\nCc: \"Cy <Ops> Ray\" <cy@example.com>\r\nSubject: Re: Plan\r\nIn-Reply-To: <b@example.com>\r\nMessage-ID: <c@example.com>\r\n\r\nYes\r\n").as_bytes());
        let thread: ThreadId = j.store.read(|c| Ok(c.query_row("SELECT thread_id FROM message WHERE id = ?1", [first], |r| r.get(0))?)).unwrap();
        let (to, cc) = j.store.read(|c| proposal_recipients(c, thread)).unwrap();
        assert_eq!(to, "Dee <dee@example.com>", "the user wrote last, so the newest sender who isn't the user");
        assert_eq!(cc, "Ann Berg <ann@example.com>, Bo <bo@example.com>, \"Cy <Ops> Ray\" <cy@example.com>", "Bo was only on the first message");
        let sent: Vec<String> = crate::compose::parse_addresses(&cc).into_iter().map(|(_, a)| a).collect();
        assert_eq!(sent, ["ann@example.com", "bo@example.com", "cy@example.com"], "the addresses looked up are the ones the reply goes to");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_recording_answers_in_place_of_the_provider_and_the_store_gives_the_users_busy_times() {
        let (j, calendar) = jolta("find-time-recorded");
        let account = AccountId(1);
        let horizon = crate::calendar::series::Horizon::around(Utc.with_ymd_and_hms(2026, 11, 9, 7, 0, 0).unwrap(), Brussels);
        for (uid, start, end, transp) in [("standup", "150000", "153000", "OPAQUE"), ("lunch", "110000", "120000", "TRANSPARENT")] {
            let ics = format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:20261109T{start}Z\r\nDTEND:20261109T{end}Z\r\nSUMMARY:{uid}\r\nTRANSP:{transp}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            );
            let events = crate::calendar::ical::read(&ics, &[ME.to_string()]).unwrap();
            j.store.write(|tx| store::apply_item(tx, calendar, uid, None, &events, None, &horizon)).unwrap();
        }
        let hours = availability::WorkingHours { days: Weekdays::MONDAY_TO_FRIDAY, ..WorkingHours::standard(Brussels) };
        let file = serde_json::json!({
            "workingHours": hours,
            "peopleHours": { "Ann@example.com": WorkingHours { end: chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap(), start: chrono::NaiveTime::from_hms_opt(8, 0, 0).unwrap(), ..WorkingHours::standard(chrono_tz::America::New_York) } },
            "people": {
                "ann@example.com": {"known": [{"start": "2026-11-09T08:00:00Z", "end": "2026-11-09T12:00:00Z", "kind": "busy"}]},
                "bo@example.com": {"unknown": "notShared"},
                ME: {"known": [{"start": "2026-11-09T13:00:00Z", "end": "2026-11-09T14:00:00Z", "kind": "tentative"}]}
            }
        });
        std::fs::write(j.data_dir.join("free-busy.json"), file.to_string()).unwrap();
        let from = Utc.with_ymd_and_hms(2026, 11, 9, 7, 0, 0).unwrap();
        let people = ["ann@example.com".to_string(), "bo@example.com".into(), "cy@example.com".into(), ME.into()];
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let Lookup::Found(found) = rt.block_on(look_up(&j, account, &people, from, 1)).unwrap() else { panic!("calendar is on") };
        assert_eq!(found.others.iter().map(|s| s.person.as_str()).collect::<Vec<_>>(), ["ann@example.com", "bo@example.com", "cy@example.com"], "the user is not among the others");
        assert_eq!(found.others[1].availability, Availability::Unknown(Unknown::NotShared));
        assert_eq!(found.others[2].availability, Availability::Unknown(Unknown::NotFound), "a person the recording doesn't name");
        assert_eq!(
            found.own.iter().map(|b| (b.start.format("%H:%M").to_string(), b.kind)).collect::<Vec<_>>(),
            [("13:00".to_string(), BusyKind::Tentative), ("15:00".to_string(), BusyKind::Busy)],
            "the recording's answer for the user beside the stored standup; the free lunch blocks nothing"
        );
        assert_eq!(found.others[0].hours.map(|h| h.zone), Some(chrono_tz::America::New_York), "the recording's hours for Ann");
        assert_eq!(found.others[1].hours, None);
        let suggest = |respect_others_hours| availability::suggest(&found.own, &found.others, &found.hours, Duration::minutes(60), from, 1, availability::SuggestOptions { respect_others_hours });
        let slots = suggest(false);
        assert_eq!(slots[0].start, Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(), "after Ann's morning, 13:00 in Brussels");
        assert_eq!(slots[0].missing, ["bo@example.com", "cy@example.com"]);
        let slots = suggest(true);
        assert_eq!(slots[0].start, Utc.with_ymd_and_hms(2026, 11, 9, 14, 0, 0).unwrap(), "Ann starts at 14:00 in Brussels, and the user is busy until 15:00");

        std::fs::remove_file(j.data_dir.join("free-busy.json")).unwrap();
        let Lookup::Found(found) = rt.block_on(look_up(&j, account, &people, from, 1)).unwrap() else { panic!("calendar is on") };
        assert!(found.others.iter().all(|s| s.availability == Availability::Unknown(Unknown::NotSupported)), "CalDAV asks nobody yet");
        assert_eq!(found.own.len(), 1, "the stored standup");

        j.store.write(|tx| Ok(tx.execute("UPDATE account SET calendar = 0 WHERE id = ?1", [account])?)).unwrap();
        assert!(matches!(rt.block_on(look_up(&j, account, &people, from, 1)).unwrap(), Lookup::CalendarOff { .. }));
        assert!(matches!(rt.block_on(look_up(&j, account, &[], from, 1)).unwrap(), Lookup::Found(_)), "the user's own times need no calendar sign-in");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_room_is_looked_up_like_a_person_without_working_hours() {
        let (j, _) = jolta("find-time-rooms");
        let file = serde_json::json!({
            "people": { "bruegel@rooms.example": {"known": [{"start": "2026-11-10T13:00:00Z", "end": "2026-11-10T14:00:00Z", "kind": "busy"}]} },
            "peopleHours": { "bruegel@rooms.example": WorkingHours::standard(Brussels) }
        });
        std::fs::write(j.data_dir.join("free-busy.json"), file.to_string()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (start, end) = (Utc.with_ymd_and_hms(2026, 11, 10, 11, 0, 0).unwrap(), Utc.with_ymd_and_hms(2026, 11, 10, 16, 0, 0).unwrap());
        let rooms = ["bruegel@rooms.example".to_string(), "brel@rooms.example".to_string()];
        let found = rt.block_on(room_schedules(&j, AccountId(1), &rooms, start, end)).unwrap();
        assert_eq!(found.iter().map(|s| s.person.as_str()).collect::<Vec<_>>(), ["bruegel@rooms.example", "brel@rooms.example"]);
        assert!(matches!(&found[0].availability, Availability::Known(b) if b.len() == 1));
        assert_eq!(found[0].hours, None, "a room's hours don't limit when it is booked");
        assert_eq!(found[1].availability, Availability::Unknown(Unknown::NotFound));
        std::fs::remove_file(j.data_dir.join("free-busy.json")).unwrap();
        j.store.write(|tx| Ok(tx.execute("UPDATE account SET calendar = 0 WHERE id = 1", [])?)).unwrap();
        let off = rt.block_on(room_schedules(&j, AccountId(1), &rooms, start, end)).unwrap();
        assert!(off.iter().all(|s| matches!(&s.availability, Availability::Unknown(Unknown::Failed(m)) if m.starts_with("Turn calendar on"))));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Google's free/busy answers the user's own out of office as plain
    /// busy; the stored occurrence says what it is (C4 U1).
    #[test]
    fn the_stored_out_of_office_wins_over_the_providers_busy_block() {
        let (j, calendar) = jolta("find-time-away");
        let horizon = crate::calendar::series::Horizon::around(Utc.with_ymd_and_hms(2026, 11, 10, 7, 0, 0).unwrap(), Brussels);
        let local = j
            .store
            .write(|tx| {
                let c = crate::store::RemoteCalendar { remote_id: "here".into(), name: "Here".into(), color: None, zone: None, access: crate::calendar::Access::Owner, primary: true, refresh: None, video: None };
                store::add_calendar(tx, crate::calendar::CalendarSource::Local, &c)
            })
            .unwrap()
            .id;
        for (cal, uid, start, end, status) in [(calendar, "away", "080000", "120000", "OOF"), (calendar, "talk", "120000", "130000", "BUSY"), (local, "gym", "150000", "160000", "BUSY")] {
            let ics = format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:20261110T{start}Z\r\nDTEND:20261110T{end}Z\r\nSUMMARY:{uid}\r\nX-MICROSOFT-CDO-BUSYSTATUS:{status}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            );
            let events = crate::calendar::ical::read(&ics, &[ME.to_string()]).unwrap();
            j.store.write(|tx| store::apply_item(tx, cal, uid, None, &events, None, &horizon)).unwrap();
        }
        let block = |start: &str, end: &str| serde_json::json!({"start": format!("2026-11-10T{start}:00Z"), "end": format!("2026-11-10T{end}:00Z"), "kind": "busy"});
        let file = serde_json::json!({
            "workingHours": availability::WorkingHours { days: Weekdays::MONDAY_TO_FRIDAY, start: chrono::NaiveTime::from_hms_opt(7, 0, 0).unwrap(), end: chrono::NaiveTime::from_hms_opt(19, 0, 0).unwrap(), ..WorkingHours::standard(Brussels) },
            "people": { ME: {"known": [block("07:00", "07:30"), block("08:00", "13:00"), block("15:00", "16:00"), block("16:30", "17:00")]} }
        });
        std::fs::write(j.data_dir.join("free-busy.json"), file.to_string()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let from = Utc.with_ymd_and_hms(2026, 11, 10, 5, 0, 0).unwrap();
        let Lookup::Found(found) = rt.block_on(look_up(&j, AccountId(1), &[], from, 1)).unwrap() else { panic!("calendar is on") };
        let blocks: Vec<(String, String, BusyKind)> = found.own.iter().map(|b| (b.start.format("%H:%M").to_string(), b.end.format("%H:%M").to_string(), b.kind)).collect();
        let want = |list: &[(&str, &str, BusyKind)]| list.iter().map(|(s, e, k)| (s.to_string(), e.to_string(), *k)).collect::<Vec<_>>();
        assert_eq!(
            blocks,
            want(&[
                ("07:00", "07:30", BusyKind::Busy),
                ("08:00", "12:00", BusyKind::OutOfOffice),
                ("12:00", "13:00", BusyKind::Busy),
                ("15:00", "16:00", BusyKind::Busy),
                ("15:00", "16:00", BusyKind::Busy),
                ("16:30", "17:00", BusyKind::Busy),
            ]),
            "Google's 08:00 to 13:00 is the stored out of office and talk together, so it goes; the gym is on another calendar than the account's, and 07:00 and 16:30 come from calendars Jolta doesn't show"
        );
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
