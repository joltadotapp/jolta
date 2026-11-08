//! Invite from Find a time (C4 spec, U6): the editor's prefill for a
//! meeting at the picked slot with everyone Find a time showed, the room
//! apart from the people, the conversation's subject as the title and a
//! link back to it, in the asking account's calendar with its own video
//! call. Nothing is saved or sent here; the editor saves through the
//! normal Send an update? flow.

use rusqlite::{Connection, OptionalExtension};

use super::draft::{EventPrefill, default_calendar};
use super::{Calendar, Person, Span, caps};
use crate::model::ThreadId;
use crate::{AccountId, Result, store};

/// What Invite takes from Find a time: its account, the conversation it was
/// opened on, the people and rooms on its rows and the picked slot.
pub struct Picked<'a> {
    pub account: AccountId,
    pub thread: Option<ThreadId>,
    pub people: &'a [Person],
    pub rooms: &'a [Person],
    pub span: Span,
}

pub fn prefill(conn: &Connection, picked: &Picked) -> Result<EventPrefill> {
    let mine: Vec<String> = store::accounts(conn)?.into_iter().map(|a| a.address).collect();
    let mut taken = mine.clone();
    let mut keep = |p: &Person| {
        let address = p.address.trim();
        if !address.contains('@') || taken.iter().any(|t| t.eq_ignore_ascii_case(address)) {
            return None;
        }
        taken.push(address.to_string());
        Some(Person { address: address.to_ascii_lowercase(), name: if p.name.trim().eq_ignore_ascii_case(address) { String::new() } else { p.name.trim().to_string() } })
    };
    // Rooms first, so a room someone copied on the mail stays a room.
    let rooms: Vec<Person> = picked.rooms.iter().filter_map(&mut keep).collect();
    let required: Vec<Person> = picked.people.iter().filter_map(&mut keep).collect();
    let (title, description) = match picked.thread {
        Some(thread) => conversation(conn, thread)?,
        None => (String::new(), String::new()),
    };
    let calendar = match account_calendar(conn, picked.account)? {
        Some(c) => Some(c),
        None => default_calendar(conn)?,
    };
    Ok(EventPrefill {
        calendar: calendar.as_ref().map(|c| c.id),
        title,
        span: Some(picked.span),
        required,
        optional: Vec::new(),
        room: rooms.into_iter().next(),
        description,
        video: calendar.as_ref().and_then(|c| caps::of(c).video.first().copied()),
    })
}

/// The newest subject without reply prefixes, and a line linking to the
/// newest message that has a Message-ID, as a `message:` URL (RFC 2392's
/// id, the form Apple Mail and other Mac apps open).
fn conversation(conn: &Connection, thread: ThreadId) -> Result<(String, String)> {
    let subject: Option<String> = conn
        .query_row("SELECT subject FROM message WHERE thread_id = ?1 AND trim(ifnull(subject, '')) != '' ORDER BY id DESC LIMIT 1", [thread], |r| r.get(0))
        .optional()?;
    let id: Option<String> = conn
        .query_row("SELECT message_id FROM message WHERE thread_id = ?1 AND ifnull(message_id, '') != '' ORDER BY id DESC LIMIT 1", [thread], |r| r.get(0))
        .optional()?;
    let title = subject.as_deref().map(store::without_reply_prefixes).unwrap_or_default().to_string();
    let link = id.map(|id| format!("Conversation: message:%3C{}%3E", encode(id.trim_matches(|c| c == '<' || c == '>'))));
    Ok((title, link.unwrap_or_default()))
}

fn encode(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'@' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The account's calendar for new meetings: the default calendar when it is
/// the account's, else its own shown calendar that takes new events.
fn account_calendar(conn: &Connection, account: AccountId) -> Result<Option<Calendar>> {
    let address = store::account(conn, account)?.address;
    let writable: Vec<Calendar> = store::account_calendars(conn, account)?.into_iter().filter(|c| caps::of(c).writable && c.is_own(&address)).collect();
    Ok(writable.iter().find(|c| c.is_default).or_else(|| writable.iter().find(|c| c.shown)).cloned())
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::invitation::tests::{ME, import, jolta};
    use crate::calendar::{Access, CalendarSource, MeetingProvider, draft};

    fn thread_of(j: &crate::Jolta, message: crate::MessageId) -> ThreadId {
        j.store.read(|c| Ok(c.query_row("SELECT thread_id FROM message WHERE id = ?1", [message], |r| r.get(0))?)).unwrap()
    }

    fn person(address: &str, name: &str) -> Person {
        Person { address: address.into(), name: name.into() }
    }

    fn slot() -> Span {
        draft::timed_span(
            Utc.with_ymd_and_hms(2026, 11, 10, 13, 0, 0).unwrap().timestamp(),
            Utc.with_ymd_and_hms(2026, 11, 10, 13, 30, 0).unwrap().timestamp(),
            crate::calendar::Zone::Tz(Brussels),
            Brussels,
        )
    }

    #[test]
    fn a_conversation_gives_the_title_people_room_and_a_link_back() {
        let (j, work) = jolta("invite-prefill");
        let first = import(&j, format!("From: Ann Berg <ann@example.com>\r\nTo: {ME}\r\nSubject: Partnership call\r\nMessage-ID: <a@example.com>\r\n\r\nHi\r\n").as_bytes());
        import(&j, format!("From: Me <{ME}>\r\nTo: ann@example.com\r\nSubject: AW: Re: Partnership call\r\nIn-Reply-To: <a@example.com>\r\nMessage-ID: <b+1/x@example.com>\r\n\r\nYes\r\n").as_bytes());
        let people = [person("ann@example.com", "Ann Berg"), person("Me@Example.com", "Me"), person("bruegel@rooms.example", "bruegel@rooms.example"), person("ANN@example.com", "")];
        let rooms = [person("bruegel@rooms.example", "Bruegel"), person("ensor@rooms.example", "ensor@rooms.example")];
        let picked = Picked { account: AccountId(1), thread: Some(thread_of(&j, first)), people: &people, rooms: &rooms, span: slot() };
        let p = j.store.read(|c| prefill(c, &picked)).unwrap();

        assert_eq!(p.title, "Partnership call", "the newest subject without AW: and Re:");
        assert_eq!(p.required, [person("ann@example.com", "Ann Berg")], "without the user, the room and a second Ann");
        assert!(p.optional.is_empty());
        assert_eq!(p.room, Some(person("bruegel@rooms.example", "Bruegel")), "the first room, kept apart from the people");
        assert_eq!(p.description, "Conversation: message:%3Cb%2B1%2Fx@example.com%3E", "the newest message, its id encoded");
        assert_eq!((p.calendar, p.span), (Some(work), Some(slot())), "the account's calendar at the picked slot");
        assert_eq!(p.video, None, "a calendar that makes no video calls");

        let d = j.store.read(|c| draft::draft_from_prefill(c, &p, Utc::now(), Brussels)).unwrap();
        assert_eq!((d.room.as_str(), d.room_name.as_str(), d.people.len()), ("bruegel@rooms.example", "Bruegel", 1), "the editor opens with them");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn every_reply_prefix_goes_and_the_rest_of_the_subject_stays() {
        for (subject, title) in [("Re: Plan", "Plan"), ("AW: Plan", "Plan"), ("Antw: Plan", "Plan"), ("RE: Fwd: Re[2]: Plan: Q4", "Plan: Q4"), ("Plan", "Plan")] {
            assert_eq!(store::without_reply_prefixes(subject), title, "{subject}");
        }
    }

    #[test]
    fn the_accounts_own_calendar_takes_its_video_call_and_another_falls_back_to_the_default() {
        let (j, work) = jolta("invite-calendar");
        let team = j
            .store
            .write(|tx| {
                tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (2, 'gmail', 'me@work.example', 'me', 'a2', 0, 1);")?;
                let primary = crate::store::RemoteCalendar {
                    remote_id: "me@work.example".into(),
                    name: "Me".into(),
                    color: None,
                    zone: None,
                    access: Access::Owner,
                    primary: true,
                    refresh: None,
                    video: Some(vec![MeetingProvider::Meet]),
                };
                let shared = crate::store::RemoteCalendar { remote_id: "boss@work.example".into(), name: "Boss".into(), primary: false, ..primary.clone() };
                crate::store::sync_calendar_list(tx, AccountId(2), CalendarSource::Google, &[shared, primary])?;
                Ok(crate::store::account_calendars(tx, AccountId(2))?.into_iter().find(|c| c.name == "Me").unwrap().id)
            })
            .unwrap();
        let picked = |account| Picked { account, thread: None, people: &[], rooms: &[], span: slot() };
        let google = j.store.read(|c| prefill(c, &picked(AccountId(2)))).unwrap();
        assert_eq!((google.calendar, google.video), (Some(team), Some(MeetingProvider::Meet)), "its own primary calendar, not the one shared with it, and Meet");
        assert_eq!((google.title.as_str(), google.description.as_str()), ("", ""), "no conversation, no title or link");

        j.store.write(|tx| tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (3, 'imap', 'me@home.example', 'me', 'a3', 0, 0);").map_err(Into::into)).unwrap();
        j.store.write(|tx| crate::store::set_default_calendar(tx, Some(work))).unwrap();
        let other = j.store.read(|c| prefill(c, &picked(AccountId(3)))).unwrap();
        assert_eq!(other.calendar, Some(work), "an account without a calendar of its own uses the default one");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
