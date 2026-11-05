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

/// The answers `free-busy.json` holds, by address.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recorded {
    #[serde(default)]
    people: BTreeMap<String, Availability>,
    #[serde(default)]
    working_hours: Option<WorkingHours>,
}

/// Looks up `people` through `account` for the next `days` working days
/// after `from`. With nobody to look up, as for Insert my availability, an
/// account with calendar off still gives the user's own busy times.
pub async fn look_up(jolta: &Jolta, account: AccountId, people: &[String], from: DateTime<Utc>, days: u32) -> Result<Lookup> {
    let (a, every, chosen): (Account, Vec<String>, Option<WorkingHours>) = jolta
        .store
        .read_async(move |c| Ok((store::account(c, account)?, store::accounts(c)?.into_iter().map(|a| a.address).collect(), store::working_hours(c)?)))
        .await?;
    let people: Vec<String> = people.iter().filter(|p| !every.iter().any(|m| m.eq_ignore_ascii_case(p))).cloned().collect();
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
        (None, None) => people.iter().map(|p| Schedule { person: p.clone(), availability: Availability::Unknown(Unknown::NotSupported) }).collect(),
    };
    let mine = answers.iter().position(|s| s.person == a.address).map(|i| answers.remove(i));

    let here = tz::here();
    let mut own: Vec<Busy> = jolta
        .store
        .read_async(move |c| {
            let (first_day, last_day) = (start.with_timezone(&here).date_naive(), end.with_timezone(&here).date_naive() + Duration::days(1));
            Ok(store::occurrences(c, first_day, last_day, here)?.iter().filter_map(|o| stored_busy(o, here)).collect())
        })
        .await?;
    if let Some(Schedule { availability: Availability::Known(busy), .. }) = mine {
        own.extend(busy);
    }
    own.retain(|b| b.end > start && b.start < end);
    own.sort_by_key(|b| (b.start, b.end));
    Ok(Lookup::Found(Found { account, me: a.address, own, others: answers, hours, windows }))
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
    Schedule { person: person.to_string(), availability: found.unwrap_or(Availability::Unknown(Unknown::NotFound)) }
}

/// An occurrence that blocks the user's time, as a busy block. An all-day
/// one blocks its days from midnight in `here`.
fn stored_busy(o: &OccurrenceRow, here: Tz) -> Option<Busy> {
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
        let slots = availability::suggest(&found.own, &found.others, &found.hours, Duration::minutes(60), from, 1);
        assert_eq!(slots[0].start, Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(), "after Ann's morning, 13:00 in Brussels");
        assert_eq!(slots[0].missing, ["bo@example.com", "cy@example.com"]);

        std::fs::remove_file(j.data_dir.join("free-busy.json")).unwrap();
        let Lookup::Found(found) = rt.block_on(look_up(&j, account, &people, from, 1)).unwrap() else { panic!("calendar is on") };
        assert!(found.others.iter().all(|s| s.availability == Availability::Unknown(Unknown::NotSupported)), "CalDAV asks nobody yet");
        assert_eq!(found.own.len(), 1, "the stored standup");

        j.store.write(|tx| Ok(tx.execute("UPDATE account SET calendar = 0 WHERE id = ?1", [account])?)).unwrap();
        assert!(matches!(rt.block_on(look_up(&j, account, &people, from, 1)).unwrap(), Lookup::CalendarOff { .. }));
        assert!(matches!(rt.block_on(look_up(&j, account, &[], from, 1)).unwrap(), Lookup::Found(_)), "the user's own times need no calendar sign-in");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
