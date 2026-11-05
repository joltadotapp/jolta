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
}
