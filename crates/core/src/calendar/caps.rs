//! What Jolta can do with a calendar's events, per source and access, from
//! the facts in the C3 spec (section 2). The editor reads this once and
//! never guesses.

use super::{Access, Calendar, CalendarSource, MeetingProvider, ShowAs};

/// Whether "Don't send" exists for a change to a meeting's own fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hold {
    /// Google's `sendUpdates=none`, and Jolta's own iMIP.
    Always,
    /// The server mails guests on every change to time, title, place,
    /// description or people: Exchange, and iCloud's scheduling (RFC 6638).
    NeverForMeetingFields,
}

/// How an event moves to another calendar of the same account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveRule {
    /// Google `events.move`, CalDAV MOVE, or the store for local calendars.
    Native,
    /// A create in the target, then a delete: Graph has no move.
    CopyDelete,
    None,
}

/// Which repeat rules the calendar holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepeatLimits {
    /// Rules more often than daily, and BYHOUR, BYMINUTE or BYSECOND.
    /// Google refuses FREQ=HOURLY and FREQ=MINUTELY with "Invalid recurrence
    /// rule" and keeps a weekly rule's BYHOUR=9,15 at 09:00 alone (Google
    /// test account, 2026-11-07).
    pub hourly: bool,
    /// BYSETPOS ("the last weekday of the month").
    pub set_pos: bool,
    /// More than one BYMONTH.
    pub several_months: bool,
}

impl RepeatLimits {
    const ALL: RepeatLimits = RepeatLimits { hourly: true, set_pos: true, several_months: true };
}

/// The organizer options the calendar offers (C3 spec, section 8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OrganizerOptions {
    pub request_responses: bool,
    pub allow_new_time_proposals: bool,
    pub hide_guest_list: bool,
    pub guests_can_invite: bool,
    pub guests_can_modify: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// False for feeds, read-only and free/busy calendars.
    pub writable: bool,
    /// Can add people.
    pub invite: bool,
    pub hold_update: Hold,
    /// Google's `sendUpdates=externalOnly`.
    pub outside_only: bool,
    /// A change to the people alone mails only those added or removed.
    pub new_people_only: bool,
    /// A cancellation can carry the user's message.
    pub cancel_message: bool,
    pub repeat: RepeatLimits,
    /// "This and following events".
    pub split_series: bool,
    pub move_within: MoveRule,
    pub video: Vec<MeetingProvider>,
    pub remove_video: bool,
    pub options: OrganizerOptions,
    pub show_as: Vec<ShowAs>,
    pub max_people: Option<usize>,
    /// The server mails the guests; otherwise Jolta sends iMIP from `send_from`.
    pub sends_itself: bool,
}

/// The capabilities of `calendar`. CalDAV counts as a server with
/// scheduling, as iCloud and Fastmail are, until Jolta reads the
/// `calendar-auto-schedule` token per calendar. Video lists the provider's
/// own meeting until Jolta reads the calendar's allowed providers.
pub fn of(calendar: &Calendar) -> Capabilities {
    let writable = matches!(calendar.access, Access::Owner | Access::Write)
        && !matches!(calendar.source, CalendarSource::Webcal | CalendarSource::EventKit);
    let all_show_as = vec![ShowAs::Free, ShowAs::Tentative, ShowAs::Busy, ShowAs::OutOfOffice, ShowAs::Elsewhere];
    let none = Capabilities {
        writable: false,
        invite: false,
        hold_update: Hold::Always,
        outside_only: false,
        new_people_only: false,
        cancel_message: false,
        repeat: RepeatLimits::ALL,
        split_series: false,
        move_within: MoveRule::None,
        video: Vec::new(),
        remove_video: false,
        options: OrganizerOptions::default(),
        show_as: Vec::new(),
        max_people: None,
        sends_itself: false,
    };
    if !writable {
        return none;
    }
    let base = Capabilities { writable: true, invite: true, split_series: true, remove_video: true, ..none.clone() };
    match calendar.source {
        CalendarSource::Google => Capabilities {
            outside_only: true,
            repeat: RepeatLimits { hourly: false, ..RepeatLimits::ALL },
            move_within: MoveRule::Native,
            video: vec![MeetingProvider::Meet],
            options: OrganizerOptions { hide_guest_list: true, guests_can_invite: true, guests_can_modify: true, ..Default::default() },
            show_as: vec![ShowAs::Free, ShowAs::Busy],
            sends_itself: true,
            ..base
        },
        CalendarSource::Graph => Capabilities {
            hold_update: Hold::NeverForMeetingFields,
            new_people_only: true,
            cancel_message: true,
            repeat: RepeatLimits { hourly: false, set_pos: false, several_months: false },
            move_within: MoveRule::CopyDelete,
            video: vec![MeetingProvider::Teams],
            remove_video: false,
            options: OrganizerOptions { request_responses: true, allow_new_time_proposals: true, hide_guest_list: true, ..Default::default() },
            show_as: all_show_as,
            max_people: Some(500),
            sends_itself: true,
            ..base
        },
        CalendarSource::CalDav => Capabilities {
            hold_update: Hold::NeverForMeetingFields,
            move_within: MoveRule::Native,
            options: OrganizerOptions { request_responses: true, allow_new_time_proposals: true, ..Default::default() },
            show_as: vec![ShowAs::Free, ShowAs::Busy],
            sends_itself: true,
            ..base
        },
        CalendarSource::Local => Capabilities {
            new_people_only: true,
            cancel_message: true,
            move_within: MoveRule::Native,
            options: OrganizerOptions { request_responses: true, allow_new_time_proposals: true, ..Default::default() },
            show_as: all_show_as,
            ..base
        },
        CalendarSource::Webcal | CalendarSource::EventKit => none,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::CalendarId;

    fn calendar(source: CalendarSource, access: Access) -> Calendar {
        Calendar {
            id: CalendarId(1),
            account_id: None,
            source,
            remote_id: "x".into(),
            name: "x".into(),
            color: None,
            zone: None,
            access,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: false,
            cursor: None,
            refresh: None,
            synced: None,
        }
    }

    #[test]
    fn read_only_calendars_take_no_writes() {
        for (source, access) in [
            (CalendarSource::Webcal, Access::Owner),
            (CalendarSource::Google, Access::Read),
            (CalendarSource::Graph, Access::FreeBusy),
            (CalendarSource::EventKit, Access::Owner),
        ] {
            let caps = of(&calendar(source, access));
            assert!(!caps.writable && !caps.invite && caps.video.is_empty(), "{source:?} {access:?}");
        }
        assert!(of(&calendar(CalendarSource::CalDav, Access::Write)).writable, "a shared calendar with write access");
    }

    /// The table in section 4 of the C3 spec.
    #[test]
    fn each_source_matches_the_spec_table() {
        let g = of(&calendar(CalendarSource::Google, Access::Owner));
        assert_eq!((g.hold_update, g.outside_only, g.cancel_message, g.move_within, g.sends_itself), (Hold::Always, true, false, MoveRule::Native, true));
        assert_eq!(g.video, [MeetingProvider::Meet]);
        assert_eq!(g.repeat, RepeatLimits { hourly: false, set_pos: true, several_months: true }, "the Google probe of 2026-11-07");

        let m = of(&calendar(CalendarSource::Graph, Access::Owner));
        assert_eq!((m.hold_update, m.new_people_only, m.cancel_message, m.move_within, m.remove_video), (Hold::NeverForMeetingFields, true, true, MoveRule::CopyDelete, false));
        assert_eq!((m.video.as_slice(), m.max_people, m.repeat.hourly), ([MeetingProvider::Teams].as_slice(), Some(500), false));

        let i = of(&calendar(CalendarSource::CalDav, Access::Owner));
        assert_eq!((i.hold_update, i.new_people_only, i.sends_itself), (Hold::NeverForMeetingFields, false, true));
        assert!(i.video.is_empty());

        let l = of(&calendar(CalendarSource::Local, Access::Owner));
        assert_eq!((l.hold_update, l.new_people_only, l.cancel_message, l.sends_itself), (Hold::Always, true, true, false));
    }
}
