//! Email guests: a mail to the people on an event, for what isn't a
//! calendar update ("running 5 minutes late", the agenda). It opens
//! compose from the event's account with every other person on it, the
//! subject "Re: <title>" and a first line naming the event, with its
//! meeting's join link when it has one. Nothing is sent here.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, params};

use super::detail::{self, When};
use super::{Bounds, Calendar, EventData, EventId, Person, Role, draft, organizer};
use crate::compose::{self, Compose};
use crate::{Error, Result, store};

/// Who Email guests writes to: everyone on `event` but the user and rooms,
/// the organizer first. On a meeting someone else organizes whose guest
/// list is hidden from guests, only the organizer, whom the user can see.
/// `me` holds the addresses of every account of the user's.
pub fn recipients(calendar: &Calendar, account: &str, event: &EventData, me: &[String]) -> Vec<Person> {
    let mut mine = draft::mine(calendar, account, event);
    mine.extend(me.iter().cloned());
    let is_me = |address: &str| mine.iter().any(|m| m.trim().eq_ignore_ascii_case(address.trim()));
    let guest_of_hidden = event.options.hide_guests && draft::other_organizer(calendar, account, event).is_some();
    let people = event.attendees.iter().filter(|a| !guest_of_hidden && !a.is_me && !matches!(a.role, Role::Room | Role::Resource)).map(|a| &a.person);
    let mut out: Vec<Person> = Vec::new();
    for p in event.organizer.iter().filter(|_| !event.organizer_is_me).chain(people) {
        if p.address.trim().is_empty() || is_me(&p.address) || out.iter().any(|o| o.address.eq_ignore_ascii_case(p.address.trim())) {
            continue;
        }
        out.push(Person { address: p.address.trim().to_string(), name: p.name.trim().to_string() });
    }
    out
}

/// The account Email guests writes from: the calendar's, or for a
/// calendar on this Mac the one its meetings are sent from.
fn sender(conn: &Connection, calendar: &Calendar, event: &EventData) -> Result<Option<crate::model::Account>> {
    match calendar.account_id {
        Some(id) => store::account(conn, id).map(Some),
        None => organizer::sender(conn, calendar, event),
    }
}

/// Whether Email guests applies to `event`: an account writes for its
/// calendar and someone else is on it.
pub fn possible(conn: &Connection, calendar: &Calendar, event: &EventData) -> Result<bool> {
    let Some(account) = sender(conn, calendar, event)? else { return Ok(false) };
    let every: Vec<String> = store::accounts(conn)?.into_iter().map(|a| a.address).collect();
    Ok(!recipients(calendar, &account.address, event, &every).is_empty())
}

/// The compose Email guests opens for the occurrence `at` of `event`, to
/// everyone `recipients` names, or with `only` to that one person on it.
pub fn prefill(conn: &Connection, event: EventId, at: Option<Bounds>, only: Option<&str>, here: Tz) -> Result<Compose> {
    let d = detail::open(conn, event, at, here, "")?;
    let account = sender(conn, &d.calendar, &d.event)?
        .ok_or_else(|| Error::Other(format!("Pick the account {} sends from in Settings › Calendar first.", d.calendar.name)))?;
    let every: Vec<String> = store::accounts(conn)?.into_iter().map(|a| a.address).collect();
    let mut people = recipients(&d.calendar, &account.address, &d.event, &every);
    if let Some(only) = only {
        people.retain(|p| p.address.eq_ignore_ascii_case(only.trim()));
    }
    if people.is_empty() {
        return Err(Error::Other(match only {
            Some(a) => format!("{a} isn't on {} for you to write to.", title(&d.event)),
            None => format!("Nobody else is on {}.", title(&d.event)),
        }));
    }
    let to = compose::format_addresses(&people.iter().map(|p| (p.name.clone(), p.address.clone())).collect::<Vec<_>>());
    let subject = match d.event.title.trim() {
        t if t.to_ascii_lowercase().starts_with("re:") => t.to_string(),
        t => format!("Re: {}", if t.is_empty() { "(no title)" } else { t }),
    };
    let body = format!("{}{}", about(&d.event, &d.when, here), compose::opening(&account.signature, ""));
    Ok(Compose { account: Some(account.id), to, subject, body, ..Compose::default() })
}

fn title(e: &EventData) -> &str {
    if e.title.trim().is_empty() { "this event" } else { e.title.trim() }
}

/// "Planning, Thu 12 Nov 16:00 to 17:00", and the join link on a line of
/// its own.
fn about(e: &EventData, when: &When, here: Tz) -> String {
    let day = |t: DateTime<Utc>| t.with_timezone(&here).format("%a %-d %b").to_string();
    let time = match when {
        When::Timed { start, end, .. } if start.with_timezone(&here).date_naive() == end.with_timezone(&here).date_naive() => {
            format!("{} {} to {}", day(*start), start.with_timezone(&here).format("%H:%M"), end.with_timezone(&here).format("%H:%M"))
        }
        When::Timed { start, end, .. } => format!("{} {} to {} {}", day(*start), start.with_timezone(&here).format("%H:%M"), day(*end), end.with_timezone(&here).format("%H:%M")),
        When::AllDay { first, last } if first == last => first.format("%a %-d %b").to_string(),
        When::AllDay { first, last } => format!("{} to {}", first.format("%a %-d %b"), last.format("%a %-d %b")),
    };
    let join = e.meeting.as_ref().map(|m| format!("\nJoin: {}", m.join_url)).unwrap_or_default();
    format!("{}, {time}{join}\n", title(e))
}

/// The meeting the palette's Email guests writes about when no event is
/// selected: the one running now that started last, else the next one in
/// the coming week, with someone to write to. Declined and cancelled
/// events and hidden calendars don't count.
pub fn next_meeting(conn: &Connection, now: DateTime<Utc>) -> Result<Option<(EventId, Bounds, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT o.event_id, o.start_utc, o.end_utc, e.title FROM occurrence o JOIN calendar c ON c.id = o.calendar_id JOIN event e ON e.id = o.event_id
         WHERE o.end_utc > ?1 AND o.start_utc < ?1 + 7 * 86400 AND c.shown AND e.has_attendees
           AND e.status <> 'cancelled' AND ifnull(e.my_response, '') <> 'declined'
         ORDER BY o.start_utc > ?1, CASE WHEN o.start_utc <= ?1 THEN -o.start_utc ELSE o.start_utc END",
    )?;
    let rows = stmt
        .query_map(params![now.timestamp()], |r| Ok((r.get::<_, EventId>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (event, start, end, title) in rows {
        let (calendar, data): (super::CalendarId, String) = conn.query_row("SELECT calendar_id, data FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let e: EventData = serde_json::from_str(&data).map_err(|err| Error::Other(format!("stored event {event}: {err}")))?;
        if possible(conn, &store::calendar(conn, calendar)?, &e)? {
            let at = Bounds::Utc { start: DateTime::from_timestamp(start, 0).unwrap_or_default(), end: DateTime::from_timestamp(end, 0).unwrap_or_default() };
            return Ok(Some((event, at, title)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::invitation::tests::{ME, jolta};
    use crate::calendar::series::Horizon;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    /// Stores one meeting from `lines` in the account's calendar and
    /// returns its event id.
    fn meeting(j: &crate::Jolta, calendar: super::super::CalendarId, uid: &str, lines: &str) -> EventId {
        let ics = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Planning\r\n{lines}END:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let events = crate::calendar::ical::read(&ics, &[ME.to_string()]).unwrap();
        j.store
            .write(|tx| {
                store::apply_item(tx, calendar, &format!("{uid}.ics"), None, &events, None, &Horizon::around(now(), Brussels))?;
                Ok(tx.query_row("SELECT id FROM event WHERE uid = ?1", [uid], |r| r.get(0))?)
            })
            .unwrap()
    }

    fn to(c: &Compose) -> Vec<String> {
        compose::parse_addresses(&c.to).into_iter().map(|(_, a)| a).collect()
    }

    /// Everyone on the meeting but the user and the room, organizer first,
    /// from the meeting's account, with "Re: Planning" and the event named
    /// in the first line.
    #[test]
    fn email_guests_writes_to_everyone_but_the_user_and_rooms() {
        let (j, calendar) = jolta("emailing-guests");
        j.store.write(|tx| Ok(tx.execute_batch("UPDATE account SET signature = '-- Me' WHERE id = 1; INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (2, 'imap', 'me@work.example', 'me2', 'a2', 0);")?)).unwrap();
        let mine = meeting(
            &j,
            calendar,
            "mine",
            "ORGANIZER;CN=Me:mailto:me@example.com\r\nATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nATTENDEE;ROLE=OPT-PARTICIPANT:mailto:ann@example.com\r\n\
             ATTENDEE;CUTYPE=ROOM;CN=Board room:mailto:board@rooms.example\r\nATTENDEE:mailto:me@example.com\r\nATTENDEE:mailto:me@work.example\r\n\
             ATTENDEE:mailto:BO@example.com\r\nURL:https://example.com\r\n",
        );
        let c = j.store.read(|c| prefill(c, mine, None, None, Brussels)).unwrap();
        assert_eq!(to(&c), ["bo@example.com", "ann@example.com"], "not the user, their other account, the room or Bo twice");
        assert_eq!((c.account, c.subject.as_str()), (Some(crate::AccountId(1)), "Re: Planning"));
        assert_eq!(c.body, "Planning, Thu 12 Nov 16:00 to 17:00\n\n\n-- Me", "the event named first, then room to write, then the signature");

        let theirs = meeting(&j, calendar, "theirs", "ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nATTENDEE:mailto:bo@example.com\r\n");
        let c = j.store.read(|c| prefill(c, theirs, None, None, Brussels)).unwrap();
        assert_eq!(to(&c), ["ann@example.com", "bo@example.com"], "the organizer first");
        let c = j.store.read(|c| prefill(c, theirs, None, Some("BO@example.com"), Brussels)).unwrap();
        assert_eq!(to(&c), ["bo@example.com"], "an address in the detail: that person only");
        assert!(j.store.read(|c| prefill(c, theirs, None, Some("eve@example.com"), Brussels)).unwrap_err().to_string().contains("isn't on Planning"));

        let alone = meeting(&j, calendar, "alone", "ORGANIZER:mailto:me@example.com\r\nATTENDEE:mailto:me@example.com\r\n");
        assert!(j.store.read(|c| prefill(c, alone, None, None, Brussels)).unwrap_err().to_string().contains("Nobody else"));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A guest of a meeting whose guest list is hidden writes to its
    /// organizer only; the organizer of one writes to every guest.
    #[test]
    fn a_hidden_guest_list_addresses_only_the_organizer() {
        let (j, calendar) = jolta("emailing-hidden");
        let hidden = |e: &mut EventData| e.options.hide_guests = true;
        let theirs = meeting(&j, calendar, "theirs", "ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nATTENDEE:mailto:bo@example.com\r\n");
        let mine = meeting(&j, calendar, "mine", "ORGANIZER:mailto:me@example.com\r\nATTENDEE:mailto:bo@example.com\r\nATTENDEE:mailto:cy@example.com\r\n");
        j.store
            .write(|tx| {
                for id in [theirs, mine] {
                    let data: String = tx.query_row("SELECT data FROM event WHERE id = ?1", [id], |r| r.get(0))?;
                    let mut e: EventData = serde_json::from_str(&data).unwrap();
                    hidden(&mut e);
                    store::set_event_data(tx, id, &e)?;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(to(&j.store.read(|c| prefill(c, theirs, None, None, Brussels)).unwrap()), ["ann@example.com"]);
        assert_eq!(to(&j.store.read(|c| prefill(c, mine, None, None, Brussels)).unwrap()), ["bo@example.com", "cy@example.com"]);
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The meeting's join link goes in the first lines; the palette's
    /// meeting is the one running, else the next with someone on it.
    #[test]
    fn the_join_link_and_the_next_meeting() {
        let (j, calendar) = jolta("emailing-next");
        let call = meeting(&j, calendar, "call", "ORGANIZER:mailto:me@example.com\r\nATTENDEE:mailto:bo@example.com\r\nLOCATION:https://meet.google.com/abc-defg-hij\r\n");
        let c = j.store.read(|c| prefill(c, call, None, None, Brussels)).unwrap();
        assert!(c.body.starts_with("Planning, Thu 12 Nov 16:00 to 17:00\nJoin: https://meet.google.com/abc-defg-hij\n"), "{}", c.body);
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:solo\r\nDTSTART:20261112T120000Z\r\nDTEND:20261112T130000Z\r\nSUMMARY:Focus\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = crate::calendar::ical::read(ics, &[ME.to_string()]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "solo.ics", None, &events, None, &Horizon::around(now(), Brussels)).map(|_| ())).unwrap();
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 11, 12, h, 0, 0).unwrap();
        let next = |t| j.store.read(|c| next_meeting(c, t)).unwrap().map(|(id, _, title)| (id, title));
        assert_eq!(next(at(11)), Some((call, "Planning".into())), "the event without guests before it doesn't count");
        assert_eq!(next(at(15)), Some((call, "Planning".into())), "running now");
        assert_eq!(next(at(17)), None, "over");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
