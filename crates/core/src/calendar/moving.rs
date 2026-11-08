//! Moving an event to another calendar (C3 spec, section 8): how each move
//! goes, the sentence for each move Jolta refuses, and the event the other
//! calendar gets.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::caps::{self, MoveRule};
use super::{Calendar, CalendarSource, EventData, ShowAs, change, description, draft};

/// How an event reaches the other calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum How {
    /// One request that keeps the server's object: Google `events.move`,
    /// CalDAV MOVE, or the store alone between calendars on this Mac.
    Native,
    /// A create in the other calendar, then a delete of the event once the
    /// create is done: Microsoft 365 has no move, and two accounts share no
    /// objects.
    Copy,
}

/// A move Jolta makes.
#[derive(Clone, Debug, PartialEq)]
pub struct Moving {
    pub how: How,
    /// The series and its changed occurrences as the other calendar gets
    /// them: as they are for a native move, without what belongs to the
    /// old calendar for a copy.
    pub events: Vec<EventData>,
    /// What the move costs, said before it: "Standup's Teams meeting stays
    /// behind: Google calendars can't have one."
    pub note: Option<String>,
}

/// How `events`, a series with its changed occurrences in `from`, moves to
/// `to`, or the sentence that says why it can't. `address` is the user in
/// `from`; `to_holds_uid` when `to` has an event with the UID already.
pub fn plan(from: &Calendar, address: &str, to: &Calendar, events: &[EventData], to_holds_uid: bool) -> Result<Moving, String> {
    let title = events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()).map_or("The event", |e| e.title.as_str());
    if from.id == to.id {
        return Err(format!("{title} is in {} already.", to.name));
    }
    if !caps::of(from).writable {
        return Err(format!("{} is read-only.", from.name));
    }
    if !caps::of(to).writable {
        return Err(format!("{} is read-only.", to.name));
    }
    // Check 1: a meeting someone else organizes is their copy in the
    // calendar their invitation reached.
    if let Some(o) = events.iter().find_map(|e| draft::other_organizer(from, address, e)) {
        return Err(format!("{} organizes {title}, so it stays in the calendar the invitation reached.", change::name(o)));
    }
    if to_holds_uid {
        return Err(format!("{} has {title} already.", to.name));
    }
    let within = from.source == to.source
        && match from.source {
            CalendarSource::Local => true,
            _ => from.account_id.is_some() && from.account_id == to.account_id,
        };
    let caps = caps::of(from);
    let how = match (within, caps.move_within) {
        (true, MoveRule::Native) => How::Native,
        (true, MoveRule::CopyDelete) | (false, _) => How::Copy,
        (true, MoveRule::None) => return Err(format!("{} can't move events between calendars.", caps::mailer(from))),
    };
    let guests: BTreeSet<String> = events.iter().flat_map(|e| draft::guests(from, address, e)).map(|a| a.person.address.to_lowercase()).collect();
    if !guests.is_empty() && !(how == How::Native && caps.move_meetings) {
        return Err(meeting_refusal(from, to, within, guests.len()));
    }
    // Google moves only events of the default type (Events: move, read
    // 2026-11-07); Jolta reads Google's out of office as its show as.
    if how == How::Native && from.source == CalendarSource::Google && events.iter().any(|e| e.show_as == ShowAs::OutOfOffice) {
        return Err("Google Calendar can't move an out of office event.".into());
    }
    Ok(match how {
        How::Native => Moving { how, events: events.to_vec(), note: None },
        How::Copy => {
            let (events, note) = copied(from, address, to, events);
            Moving { how, events, note }
        }
    })
}

/// Why a meeting with guests doesn't move: each way of moving it would
/// tell the guests, or lose what they answered.
fn meeting_refusal(from: &Calendar, to: &Calendar, within: bool, guests: usize) -> String {
    let your = if guests == 1 { "your guest".to_string() } else { format!("your {guests} guests") };
    let local = from.source == CalendarSource::Local || to.source == CalendarSource::Local;
    match from.source {
        _ if local => "Jolta can't move a meeting with guests into or out of a calendar on this Mac.".into(),
        // A copy with a new UID invites again and loses the answers; one
        // that keeps it makes Google cancel the guest's copy (Microsoft 365
        // test account, 2026-11-07).
        CalendarSource::Graph if within => format!("Moving this meeting would send {your} a cancellation and a new invitation, and lose their answers."),
        // Google gives the moved meeting the other calendar as organizer,
        // and what that sends the guests is unchecked.
        CalendarSource::Google if within => "Jolta doesn't move meetings with guests between Google calendars yet.".into(),
        _ if within => "Jolta doesn't move meetings with guests on this calendar server yet.".into(),
        _ => format!("Moving this meeting to another account would send {your} a cancellation and a new invitation, and lose their answers."),
    }
}

/// The events a copy makes in `to`: without the user's own entries among
/// the attendees, which another account would invite, without the organizer
/// and series ids the old server gave, and without the call the old
/// server made, which belongs to the event it leaves. A calendar that makes
/// the same call gets a new one.
fn copied(from: &Calendar, address: &str, to: &Calendar, events: &[EventData]) -> (Vec<EventData>, Option<String>) {
    let made_by_from = caps::own_video(from.source);
    let mut note = None;
    let out = events
        .iter()
        .map(|e| {
            let me = draft::mine(from, address, e);
            let mut c = e.clone();
            c.attendees.retain(|a| !a.is_me && !me.iter().any(|m| m.eq_ignore_ascii_case(&a.person.address)));
            c.organizer = None;
            c.organizer_is_me = false;
            c.master_id = None;
            if let Some(m) = e.meeting.as_ref().filter(|m| Some(m.provider) == made_by_from) {
                let parts = description::split(&e.description, e.description_html);
                c.description = draft::without_meeting(&parts.editable, m);
                c.description_html = false;
                c.location = e.location.replace(&m.join_url, "").trim_matches(|ch: char| ch.is_whitespace() || ",;·|-".contains(ch)).to_owned();
                c.meeting = None;
                let again = caps::of(to).video.contains(&m.provider);
                c.video = again.then_some(m.provider);
                let call = match m.provider {
                    super::MeetingProvider::Teams => "Teams meeting",
                    super::MeetingProvider::Meet => "Google Meet",
                    _ => "video call",
                };
                note.get_or_insert_with(|| match again {
                    true => format!("{} gets a new {call} link in {}.", e.title, to.name),
                    false => format!("{}'s {call} stays behind: {} calendars can't have one.", e.title, draft::provider(to)),
                });
            }
            c
        })
        .collect();
    (out, note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AccountId;
    use crate::calendar::{Access, Attendee, CalendarId, Local, Meeting, MeetingProvider, Person, Response, Role, Span, Zone};

    const ME: &str = "me@example.com";

    fn calendar(id: i64, account: Option<i64>, source: CalendarSource) -> Calendar {
        Calendar {
            id: CalendarId(id),
            account_id: account.map(AccountId),
            source,
            remote_id: format!("cal{id}"),
            name: format!("Cal {id}"),
            color: None,
            zone: None,
            access: Access::Owner,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: false,
            cursor: None,
            refresh: None,
            synced: None,
            video: None,
        }
    }

    fn at(s: &str) -> Local {
        Local { wall: chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(chrono_tz::Europe::Brussels) }
    }

    fn standup() -> EventData {
        EventData { title: "Standup".into(), ..EventData::new("u1", Span::Timed { start: at("2026-11-24 09:00"), end: at("2026-11-24 09:30") }) }
    }

    fn person(address: &str, is_me: bool) -> Attendee {
        Attendee { person: Person { address: address.into(), name: String::new() }, role: Role::Required, response: Response::NeedsAction, comment: String::new(), is_me }
    }

    fn meeting(guests: &[&str]) -> EventData {
        let mut e = standup();
        e.organizer = Some(Person { address: ME.into(), name: String::new() });
        e.organizer_is_me = true;
        e.attendees = std::iter::once(person(ME, true)).chain(guests.iter().map(|g| person(g, false))).collect();
        e
    }

    fn how(from: &Calendar, to: &Calendar, e: &EventData) -> Result<How, String> {
        plan(from, ME, to, std::slice::from_ref(e), false).map(|m| m.how)
    }

    const GOOGLE: CalendarSource = CalendarSource::Google;
    const GRAPH: CalendarSource = CalendarSource::Graph;
    const CALDAV: CalendarSource = CalendarSource::CalDav;
    const LOCAL: CalendarSource = CalendarSource::Local;

    /// The table in section 8 of the C3 spec, for an event without guests.
    #[test]
    fn an_event_without_guests_moves_natively_within_an_account_and_by_copy_across() {
        let e = standup();
        assert_eq!(how(&calendar(1, Some(1), GOOGLE), &calendar(2, Some(1), GOOGLE), &e), Ok(How::Native), "Google events.move");
        assert_eq!(how(&calendar(1, Some(1), GRAPH), &calendar(2, Some(1), GRAPH), &e), Ok(How::Copy), "Graph has no move");
        assert_eq!(how(&calendar(1, Some(1), CALDAV), &calendar(2, Some(1), CALDAV), &e), Ok(How::Native), "CalDAV MOVE");
        assert_eq!(how(&calendar(1, None, LOCAL), &calendar(2, None, LOCAL), &e), Ok(How::Native), "the store alone");
        assert_eq!(how(&calendar(1, Some(1), GOOGLE), &calendar(2, Some(2), GOOGLE), &e), Ok(How::Copy), "two Google accounts");
        assert_eq!(how(&calendar(1, Some(1), GOOGLE), &calendar(2, Some(1), CALDAV), &e), Ok(How::Copy), "another source");
        assert_eq!(how(&calendar(1, None, LOCAL), &calendar(2, Some(1), GRAPH), &e), Ok(How::Copy), "out of this Mac");
        assert_eq!(how(&calendar(1, Some(1), GRAPH), &calendar(2, None, LOCAL), &e), Ok(How::Copy), "onto this Mac");
    }

    /// Every refused cell of the table, with its sentence.
    #[test]
    fn a_meeting_with_guests_moves_only_where_nobody_hears_of_it() {
        let icloud = |id| Calendar { remote_id: format!("https://p01-caldav.icloud.com/1/calendars/c{id}/"), ..calendar(id, Some(1), CALDAV) };
        let m = meeting(&["ann@example.com", "bob@example.com"]);
        assert_eq!(how(&icloud(1), &icloud(2), &m), Ok(How::Native), "iCloud's MOVE mails nobody");
        let refused = |from: Calendar, to: Calendar| how(&from, &to, &m).unwrap_err();
        assert_eq!(
            refused(calendar(1, Some(1), GRAPH), calendar(2, Some(1), GRAPH)),
            "Moving this meeting would send your 2 guests a cancellation and a new invitation, and lose their answers."
        );
        assert_eq!(refused(calendar(1, Some(1), GOOGLE), calendar(2, Some(1), GOOGLE)), "Jolta doesn't move meetings with guests between Google calendars yet.");
        assert_eq!(refused(calendar(1, Some(1), CALDAV), calendar(2, Some(1), CALDAV)), "Jolta doesn't move meetings with guests on this calendar server yet.");
        assert_eq!(refused(calendar(1, None, LOCAL), calendar(2, None, LOCAL)), "Jolta can't move a meeting with guests into or out of a calendar on this Mac.");
        assert_eq!(refused(calendar(1, Some(1), GOOGLE), calendar(2, None, LOCAL)), "Jolta can't move a meeting with guests into or out of a calendar on this Mac.");
        assert_eq!(
            refused(icloud(1), calendar(2, Some(2), GOOGLE)),
            "Moving this meeting to another account would send your 2 guests a cancellation and a new invitation, and lose their answers."
        );
        let one = meeting(&["ann@example.com"]);
        assert_eq!(
            how(&calendar(1, Some(1), GRAPH), &calendar(2, Some(1), GRAPH), &one).unwrap_err(),
            "Moving this meeting would send your guest a cancellation and a new invitation, and lose their answers."
        );
        let mut room = standup();
        room.attendees.push(Attendee { role: Role::Room, ..person("room@example.com", false) });
        assert!(how(&calendar(1, Some(1), GRAPH), &calendar(2, Some(1), GRAPH), &room).is_err(), "a room hears of it too");
        let mut alone = standup();
        alone.attendees.push(person(ME, true));
        assert_eq!(how(&calendar(1, Some(1), GRAPH), &calendar(2, Some(1), GRAPH), &alone), Ok(How::Copy), "the user alone is no guest");
    }

    /// Check 1: someone else's meeting stays where their invitation came.
    #[test]
    fn someone_elses_meeting_stays_in_the_calendar_it_reached() {
        let mut theirs = meeting(&[ME]);
        theirs.organizer = Some(Person { address: "jan@example.com".into(), name: "Jan Peeters".into() });
        theirs.organizer_is_me = false;
        theirs.attendees = vec![person(ME, true), person("els@example.com", false)];
        for (from, to) in [(calendar(1, None, LOCAL), calendar(2, None, LOCAL)), (calendar(1, Some(1), GOOGLE), calendar(2, Some(1), GOOGLE)), (calendar(1, Some(1), GRAPH), calendar(2, Some(2), CALDAV))] {
            assert_eq!(how(&from, &to, &theirs).unwrap_err(), "Jan Peeters organizes Standup, so it stays in the calendar the invitation reached.");
        }
    }

    #[test]
    fn read_only_calendars_and_one_holding_the_event_take_no_move() {
        let e = standup();
        let feed = Calendar { access: Access::Read, ..calendar(2, Some(1), GOOGLE) };
        assert_eq!(how(&calendar(1, Some(1), GOOGLE), &feed, &e).unwrap_err(), "Cal 2 is read-only.");
        assert_eq!(how(&feed, &calendar(1, Some(1), GOOGLE), &e).unwrap_err(), "Cal 2 is read-only.");
        assert_eq!(how(&calendar(1, Some(1), GOOGLE), &calendar(1, Some(1), GOOGLE), &e).unwrap_err(), "Standup is in Cal 1 already.");
        let to = calendar(2, Some(1), GOOGLE);
        assert_eq!(plan(&calendar(1, Some(1), GOOGLE), ME, &to, &[e.clone()], true).unwrap_err(), "Cal 2 has Standup already.");
        let away = EventData { show_as: ShowAs::OutOfOffice, ..e };
        assert_eq!(how(&calendar(1, Some(1), GOOGLE), &to, &away).unwrap_err(), "Google Calendar can't move an out of office event.");
    }

    const TEAMS_BLOCK: &str = "________________\nMicrosoft Teams meeting\nJoin: https://teams.microsoft.com/l/meetup-join/abc\nMeeting ID: 123 456 789\n________________";

    fn teams(e: EventData) -> EventData {
        EventData {
            description: format!("Agenda first.\n\n{TEAMS_BLOCK}"),
            meeting: Some(Meeting {
                provider: MeetingProvider::Teams,
                join_url: "https://teams.microsoft.com/l/meetup-join/abc".into(),
                meeting_id: Some("123 456 789".into()),
                passcode: None,
                phones: Vec::new(),
                other_links: Vec::new(),
            }),
            ..e
        }
    }

    /// A Teams meeting moved to a Google calendar loses its Teams block,
    /// and says so before the move.
    #[test]
    fn a_copy_leaves_the_old_servers_call_behind_and_says_so() {
        let from = calendar(1, Some(1), GRAPH);
        let mut e = teams(standup());
        e.attendees.push(person(ME, true));
        e.master_id = Some("AAMk".into());
        let moved = plan(&from, ME, &calendar(2, Some(2), GOOGLE), &[e.clone()], false).unwrap();
        let c = &moved.events[0];
        assert_eq!((c.description.as_str(), c.meeting.as_ref(), c.video), ("Agenda first.", None, None), "Google can't make a Teams meeting");
        assert!(c.attendees.is_empty() && c.organizer.is_none() && c.master_id.is_none(), "nothing of the old account's goes along");
        assert_eq!(moved.note.as_deref(), Some("Standup's Teams meeting stays behind: Google calendars can't have one."));

        let again = plan(&from, ME, &calendar(2, Some(1), GRAPH), &[e.clone()], false).unwrap();
        assert_eq!((again.events[0].description.as_str(), again.events[0].video), ("Agenda first.", Some(MeetingProvider::Teams)));
        assert_eq!(again.note.as_deref(), Some("Standup gets a new Teams meeting link in Cal 2."));

        let mut zoom = standup();
        zoom.description = "https://zoom.us/j/1".into();
        zoom.meeting = Some(Meeting { provider: MeetingProvider::Zoom, join_url: "https://zoom.us/j/1".into(), meeting_id: None, passcode: None, phones: Vec::new(), other_links: Vec::new() });
        let kept = plan(&from, ME, &calendar(2, Some(2), GOOGLE), &[zoom.clone()], false).unwrap();
        assert_eq!((kept.events[0].description.as_str(), kept.note), ("https://zoom.us/j/1", None), "a link the user typed works anywhere");
        let native = plan(&calendar(1, Some(1), CALDAV), ME, &calendar(2, Some(1), CALDAV), &[teams(standup())], false).unwrap();
        assert_eq!(native.events, [teams(standup())], "a native move keeps the event as it is");
    }
}
