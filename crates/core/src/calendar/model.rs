use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Tz;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};

use crate::model::{id_type, text_enum};
use crate::AccountId;

id_type!(CalendarId);
id_type!(EventId);

text_enum!(CalendarSource {
    Graph => "graph",
    Google => "google",
    CalDav => "caldav",
    Webcal => "webcal",
    Local => "local",
    EventKit => "eventkit",
});

text_enum!(Access {
    Owner => "owner",
    Write => "write",
    Read => "read",
    FreeBusy => "freebusy",
});

text_enum!(EventStatus {
    Confirmed => "confirmed",
    Tentative => "tentative",
    Cancelled => "cancelled",
});

text_enum!(ShowAs {
    Free => "free",
    Tentative => "tentative",
    Busy => "busy",
    OutOfOffice => "oof",
    Elsewhere => "elsewhere",
});

text_enum!(Visibility {
    Default => "default",
    Public => "public",
    Private => "private",
    Confidential => "confidential",
});

text_enum!(Role {
    Required => "required",
    Optional => "optional",
    Chair => "chair",
    Room => "room",
    Resource => "resource",
    Fyi => "fyi",
});

text_enum!(Response {
    NeedsAction => "needs-action",
    Accepted => "accepted",
    Tentative => "tentative",
    Declined => "declined",
    Delegated => "delegated",
});

text_enum!(MeetingProvider {
    Zoom => "zoom",
    Teams => "teams",
    Meet => "meet",
    Webex => "webex",
    GoTo => "goto",
    Whereby => "whereby",
    Jitsi => "jitsi",
    Chime => "chime",
    Slack => "slack",
    FaceTime => "facetime",
    RingCentral => "ringcentral",
    Zoho => "zoho",
    Other => "other",
});

/// The zone a wall-clock time belongs to. A floating time ("9:00 wherever I
/// am") has none and is read in the zone the Mac is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Zone {
    Tz(Tz),
    Floating,
}

/// A wall-clock time and its zone: the truth for a timed event. The UTC
/// instant is derived from it, so a time zone rule change moves the instant
/// and not the meeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Local {
    pub wall: NaiveDateTime,
    pub zone: Zone,
}

/// A DTSTART, RECURRENCE-ID, RDATE or EXDATE value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Moment {
    Date(NaiveDate),
    Time(Local),
}

/// When an event happens. All-day events are dates and never pass through a
/// time zone; their end is exclusive, as in iCalendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Span {
    AllDay { start: NaiveDate, end: NaiveDate },
    Timed { start: Local, end: Local },
}

/// A span placed on the timeline, as the occurrence table stores it. Days
/// count from 1970-01-01.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bounds {
    Days { start: i32, end: i32 },
    Utc { start: DateTime<Utc>, end: DateTime<Utc> },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recurrence {
    /// RRULE values without the `RRULE:` prefix. Almost always one.
    pub rules: Vec<String>,
    pub rdates: Vec<Moment>,
    pub exdates: Vec<Moment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub address: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
    pub person: Person,
    pub role: Role,
    pub response: Response,
    pub comment: String,
    /// One of the user's own addresses on this account.
    pub is_me: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlarmTrigger {
    MinutesBefore(i64),
    At(DateTime<Utc>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alarm {
    pub trigger: AlarmTrigger,
    pub email: bool,
}

/// What a meeting's organizer lets its guests do (C3 spec, section 8). A
/// source holds only some of these; the others keep their default, which
/// is what every provider does when nothing is said.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeetingOptions {
    /// Graph `responseRequested`, iCalendar `RSVP`.
    pub request_responses: bool,
    /// Graph `allowNewTimeProposals`, Outlook's `X-MICROSOFT-DISALLOW-COUNTER`.
    pub allow_new_times: bool,
    /// Graph `hideAttendees`, Google `guestsCanSeeOtherGuests: false`.
    pub hide_guests: bool,
    /// Google `guestsCanInviteOthers`.
    pub guests_invite: bool,
    /// Google `guestsCanModify`.
    pub guests_modify: bool,
}

impl Default for MeetingOptions {
    fn default() -> Self {
        MeetingOptions { request_responses: true, allow_new_times: true, hide_guests: false, guests_invite: true, guests_modify: false }
    }
}

impl MeetingOptions {
    pub fn is_default(&self) -> bool {
        *self == MeetingOptions::default()
    }
}

/// How to join an online meeting, found by `meeting::detect`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meeting {
    pub provider: MeetingProvider,
    pub join_url: String,
    pub meeting_id: Option<String>,
    pub passcode: Option<String>,
    /// Dial-in numbers as `tel:` URIs with pauses and passcode filled in.
    pub phones: Vec<String>,
    /// Other meeting-like links found in the description.
    pub other_links: Vec<String>,
}

/// What a provider's status event says beyond busy or free: Google's
/// `eventType` with its properties. Other sources say only "out of office"
/// in `show_as`, which `shown` turns into a kind where the views read it,
/// so an edit of Show as never leaves a stale kind behind.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    #[default]
    Default,
    OutOfOffice {
        declines: AutoDecline,
        message: String,
    },
    Focus {
        declines: AutoDecline,
        do_not_disturb: bool,
        message: String,
    },
    WorkingLocation(Workplace),
    Birthday,
}

/// Which invitations over a status event the provider declines for the
/// user. New invitations only is Google Calendar's own default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoDecline {
    None,
    #[default]
    New,
    All,
}

/// Where a working location event says the user works, with the label the
/// provider shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Workplace {
    Home,
    Office(String),
    Other(String),
}

impl EventKind {
    pub fn is_default(&self) -> bool {
        *self == EventKind::Default
    }

    /// The kind an event shows as: its own; else out of office, declining
    /// nothing, for an event shown as out of office; else focus time for a
    /// busy event with the Focus time category, which Jolta gives focus
    /// time where the calendar has no kind for it (`focus::CATEGORY`).
    pub fn shown(&self, show_as: ShowAs, categories: &[String]) -> EventKind {
        match self {
            EventKind::Default if show_as == ShowAs::OutOfOffice => EventKind::OutOfOffice { declines: AutoDecline::None, message: String::new() },
            EventKind::Default if show_as == ShowAs::Busy && categories.iter().any(|c| c.eq_ignore_ascii_case(super::focus::CATEGORY)) => {
                EventKind::Focus { declines: AutoDecline::None, do_not_disturb: false, message: String::new() }
            }
            kind => kind.clone(),
        }
    }

    /// The event detail's line: "Out of office · declines new invitations".
    pub fn line(&self) -> Option<String> {
        let declines = |d: &AutoDecline| match d {
            AutoDecline::None => None,
            AutoDecline::New => Some("declines new invitations"),
            AutoDecline::All => Some("declines new and existing meetings"),
        };
        let parts: Vec<&str> = match self {
            EventKind::Default | EventKind::Birthday => return None,
            EventKind::OutOfOffice { declines: d, .. } => ["Out of office"].into_iter().chain(declines(d)).collect(),
            EventKind::Focus { declines: d, do_not_disturb, .. } => {
                ["Focus time"].into_iter().chain(declines(d)).chain(do_not_disturb.then_some("Do not disturb in Chat")).collect()
            }
            EventKind::WorkingLocation(place) => {
                return Some(match place {
                    Workplace::Home => "Working from home".into(),
                    Workplace::Office(label) if label.is_empty() => "Working from the office".into(),
                    Workplace::Office(label) => format!("Working from the office · {label}"),
                    Workplace::Other(label) if label.is_empty() => "Working elsewhere".into(),
                    Workplace::Other(label) => format!("Working from {label}"),
                });
            }
        };
        Some(parts.join(" · "))
    }
}

impl Workplace {
    /// The label the week view's all-day strip shows: "Home", "Office · Building 2".
    pub fn label(&self) -> String {
        match self {
            Workplace::Home => "Home".into(),
            Workplace::Office(label) if label.is_empty() => "Office".into(),
            Workplace::Office(label) => format!("Office · {label}"),
            Workplace::Other(label) if label.is_empty() => "Elsewhere".into(),
            Workplace::Other(label) => label.clone(),
        }
    }
}

/// One event as every source delivers it: a single event, the master of a
/// series, or an override of one occurrence (`recurrence_id` set). Masters
/// and overrides share a `uid`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventData {
    pub uid: String,
    pub recurrence_id: Option<Moment>,
    /// `RANGE=THISANDFUTURE` on the recurrence id. Read, never written.
    pub this_and_future: bool,
    pub span: Span,
    pub recurrence: Option<Recurrence>,
    pub title: String,
    pub location: String,
    /// As the source gave it. Shown through the same sanitiser as mail.
    pub description: String,
    pub description_html: bool,
    pub url: Option<String>,
    pub status: EventStatus,
    pub show_as: ShowAs,
    pub visibility: Visibility,
    pub organizer: Option<Person>,
    pub attendees: Vec<Attendee>,
    pub sequence: i64,
    pub alarms: Vec<Alarm>,
    pub meeting: Option<Meeting>,
    pub categories: Vec<String>,
    pub color: Option<String>,
    /// For an occurrence a provider keeps as an object of its own, the
    /// remote id of its series: Google's `recurringEventId`, Graph's
    /// `seriesMasterId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_id: Option<String>,
    #[serde(default, skip_serializing_if = "MeetingOptions::is_default")]
    pub options: MeetingOptions,
    /// The source says the organizer is the user: Google's `organizer.self`,
    /// Graph's `isOrganizer`, a CalDAV ORGANIZER among the principal's
    /// addresses, which iCloud rewrites to the Apple ID's preferred one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub organizer_is_me: bool,
    /// A video call the next write asks the server to make: Add video call
    /// sets it, and the server's answer, which carries the meeting in
    /// `meeting`, replaces the event. Google's reader sets it for a Meet
    /// Google is still making.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<MeetingProvider>,
    #[serde(default, skip_serializing_if = "EventKind::is_default")]
    pub kind: EventKind,
    /// Jolta's hidden marker, on an event Jolta made as a hold or for out
    /// of office.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark: Option<super::holds::Mark>,
    /// When the provider says the event was made: Google's `created`,
    /// Graph's `createdDateTime`, CREATED.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
}

impl EventData {
    /// A confirmed, busy event with nothing else set.
    pub fn new(uid: impl Into<String>, span: Span) -> EventData {
        EventData {
            uid: uid.into(),
            recurrence_id: None,
            this_and_future: false,
            span,
            recurrence: None,
            title: String::new(),
            location: String::new(),
            description: String::new(),
            description_html: false,
            url: None,
            status: EventStatus::Confirmed,
            show_as: ShowAs::Busy,
            visibility: Visibility::Default,
            organizer: None,
            attendees: Vec::new(),
            sequence: 0,
            alarms: Vec::new(),
            meeting: None,
            categories: Vec::new(),
            color: None,
            master_id: None,
            options: MeetingOptions::default(),
            organizer_is_me: false,
            video: None,
            kind: EventKind::Default,
            mark: None,
            created: None,
        }
    }

    pub fn my_response(&self) -> Option<Response> {
        self.attendees.iter().find(|a| a.is_me).map(|a| a.response)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Calendar {
    pub id: CalendarId,
    /// None for local, webcal and EventKit calendars.
    pub account_id: Option<AccountId>,
    pub source: CalendarSource,
    /// The calendar id (Graph, Google), collection URL (CalDAV) or feed URL.
    pub remote_id: String,
    pub name: String,
    /// The user's color if they picked one, else the server's.
    pub color: Option<String>,
    pub zone: Option<String>,
    pub access: Access,
    pub shown: bool,
    pub counts_as_busy: bool,
    pub notify: bool,
    pub default_reminder: Option<i64>,
    pub is_default: bool,
    /// Delta link, sync token or feed ETag, in the source's own format.
    pub cursor: Option<String>,
    /// Seconds between polls, when the source says (a feed's REFRESH-INTERVAL).
    pub refresh: Option<i64>,
    /// Unix seconds of the last sync that reached the server.
    pub synced: Option<i64>,
    /// The video calls the server lets this calendar make, as its calendar
    /// list says; None until it says, and for sources that never do.
    pub video: Option<Vec<MeetingProvider>>,
}

impl Calendar {
    /// Whether this calendar of the account with address `account` is the
    /// account's own, not one shared with it or delegated to it. Only then
    /// is the attendee its copies mark as the user one of the user's
    /// addresses, and only then is a copy there the user's to remove.
    /// CalDAV and Microsoft 365 sync give `Access::Owner` only to a calendar
    /// whose DAV:owner is the account's principal, or whose Graph `owner` is
    /// the account. Google gives its owner role to a calendar shared with
    /// "Make changes and manage sharing" too, so there only the primary
    /// calendar counts, whose id is the account's address.
    pub fn is_own(&self, account: &str) -> bool {
        self.access == Access::Owner && (self.source != CalendarSource::Google || self.remote_id.eq_ignore_ascii_case(account))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_events_from_before_kinds_read_as_default() {
        let e = EventData::new("u", Span::AllDay { start: NaiveDate::from_ymd_opt(2026, 11, 9).unwrap(), end: NaiveDate::from_ymd_opt(2026, 11, 10).unwrap() });
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("\"kind\""), "a default kind isn't written: {json}");
        let read: EventData = serde_json::from_str(&json).unwrap();
        assert_eq!(read.kind, EventKind::Default);
        let away = EventData { kind: EventKind::OutOfOffice { declines: AutoDecline::New, message: "Back Monday".into() }, ..e };
        assert_eq!(serde_json::from_str::<EventData>(&serde_json::to_string(&away).unwrap()).unwrap(), away);
    }

    #[test]
    fn the_detail_line_and_the_strip_label() {
        let shown = EventKind::Default.shown(ShowAs::OutOfOffice, &[]);
        assert_eq!(shown.line().as_deref(), Some("Out of office"));
        assert_eq!(EventKind::Default.shown(ShowAs::Busy, &[]), EventKind::Default);
        let google = EventKind::OutOfOffice { declines: AutoDecline::New, message: String::new() };
        assert_eq!(google.shown(ShowAs::Busy, &[]), google, "a provider's own kind wins");
        let focus = ["Focus time".to_string()];
        assert_eq!(EventKind::Default.shown(ShowAs::Busy, &focus).line().as_deref(), Some("Focus time"), "Microsoft 365's busy event with the category");
        assert_eq!(EventKind::Default.shown(ShowAs::Free, &focus), EventKind::Default, "shown as free, it isn't focus time");
        assert_eq!(google.line().as_deref(), Some("Out of office · declines new invitations"));
        assert_eq!(EventKind::Focus { declines: AutoDecline::None, do_not_disturb: true, message: String::new() }.line().as_deref(), Some("Focus time · Do not disturb in Chat"));
        assert_eq!(EventKind::WorkingLocation(Workplace::Home).line().as_deref(), Some("Working from home"));
        assert_eq!(EventKind::Default.line(), None);
        assert_eq!(Workplace::Office("Building 2".into()).label(), "Office · Building 2");
        assert_eq!(Workplace::Other(String::new()).label(), "Elsewhere");
        assert_eq!(AutoDecline::default(), AutoDecline::New, "Google Calendar's default, and Koen's");
    }
}
