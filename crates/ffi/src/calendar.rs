//! Calendars for the app: which calendars show, the occurrences a view
//! draws, one event's detail, reminders to schedule and the next meeting.
//! All-day events cross as "YYYY-MM-DD" days, so Swift never converts them
//! between zones.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate};
use jolta_core::calendar::scheduler::Scheduler;
use jolta_core::calendar::{self as cal, Bounds, CalendarId, EventId, detail, remind, respond, rsvp, sync, tz};
use jolta_core::{AccountId, store};

use crate::{AccountInfo, Core, CoreError, CoreEvent, Result};

/// A Swift-facing copy of a core enum, kept in step by the exhaustive match.
macro_rules! mirror {
    ($(#[$doc:meta])* $name:ident <- $core:path { $($variant:ident),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl From<$core> for $name {
            fn from(v: $core) -> Self {
                use $core as C;
                match v { $(C::$variant => $name::$variant),+ }
            }
        }
    };
}

mirror!(CalendarKind <- cal::CalendarSource { Graph, Google, CalDav, Webcal, Local, EventKit });
mirror!(CalendarAccess <- cal::Access { Owner, Write, Read, FreeBusy });
mirror!(EventStatus <- cal::EventStatus { Confirmed, Tentative, Cancelled });
mirror!(ShowAs <- cal::ShowAs { Free, Tentative, Busy, OutOfOffice, Elsewhere });
mirror!(EventResponse <- cal::Response { NeedsAction, Accepted, Tentative, Declined, Delegated });
mirror!(AttendeeRole <- cal::Role { Required, Optional, Chair, Room, Resource, Fyi });
mirror!(EventVisibility <- cal::Visibility { Default, Public, Private, Confidential });
mirror!(MeetingProvider <- cal::MeetingProvider {
    Zoom, Teams, Meet, Webex, GoTo, Whereby, Jitsi, Chime, Slack, FaceTime, RingCentral, Zoho, Other
});

#[derive(uniffi::Record)]
pub struct CalendarInfo {
    pub id: i64,
    pub account_id: Option<i64>,
    pub name: String,
    pub color: Option<String>,
    pub kind: CalendarKind,
    pub access: CalendarAccess,
    /// Hidden calendars aren't synced.
    pub shown: bool,
    pub counts_as_busy: bool,
    pub notify: bool,
    pub is_default: bool,
    pub events: u32,
    /// Unix seconds of the last sync that reached the server.
    pub synced: Option<i64>,
    /// Why the last sync failed, until one succeeds.
    pub sync_error: Option<String>,
}

#[derive(uniffi::Record)]
pub struct CalendarAccount {
    pub account: AccountInfo,
    pub calendar_on: bool,
    /// Turning calendar on needs the browser sign-in from
    /// `begin_calendar_consent` first (Google and Microsoft).
    pub needs_consent: bool,
    pub calendars: Vec<CalendarInfo>,
}

#[derive(uniffi::Record)]
pub struct CalendarList {
    pub accounts: Vec<CalendarAccount>,
    /// Calendars of no account: feed subscriptions and local calendars.
    pub other_calendars: Vec<CalendarInfo>,
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum OccurrenceTime {
    /// Unix seconds; the end is exclusive.
    Timed { start: i64, end: i64 },
    /// "YYYY-MM-DD", both days included.
    AllDay { first_day: String, last_day: String },
}

/// One occurrence as a day or week view draws it. A repeating event's
/// occurrences share `event_id`; with `time` they name one occurrence.
#[derive(uniffi::Record)]
pub struct OccurrenceRecord {
    /// The changed occurrence when one was changed, else the series.
    pub event_id: i64,
    pub calendar_id: i64,
    /// With the calendar and the time, names the occurrence across syncs.
    pub uid: String,
    pub color: Option<String>,
    pub title: String,
    pub location: String,
    pub time: OccurrenceTime,
    pub status: EventStatus,
    pub show_as: ShowAs,
    pub my_response: Option<EventResponse>,
    pub meeting_provider: Option<MeetingProvider>,
    pub join_url: Option<String>,
    pub has_attendees: bool,
    /// Blocks time: not free, cancelled or declined, in a calendar that counts as busy.
    pub busy: bool,
    /// How many shown calendars hold this meeting; it is listed once.
    pub copies: u32,
}

#[derive(uniffi::Enum, Debug)]
pub enum EventTime {
    AllDay { first_day: String, last_day: String },
    Timed {
        start: i64,
        end: i64,
        /// The event's own IANA zone, set only when its clock differs from here.
        zone: Option<String>,
        /// "10:00 to 11:00 New York (16:00 to 17:00 here)", or "16:00 to 17:00".
        text: String,
    },
}

#[derive(uniffi::Record)]
pub struct PersonRecord {
    pub name: String,
    pub address: String,
}

#[derive(uniffi::Record)]
pub struct AttendeeRecord {
    pub name: String,
    pub address: String,
    pub role: AttendeeRole,
    pub comment: String,
    pub is_me: bool,
}

#[derive(uniffi::Record)]
pub struct AttendeeGroup {
    pub response: EventResponse,
    pub people: Vec<AttendeeRecord>,
}

#[derive(uniffi::Record)]
pub struct MeetingInfo {
    pub provider: MeetingProvider,
    pub join_url: String,
    /// Opens the provider's app; check that it is installed before using it.
    pub app_url: Option<String>,
    pub meeting_id: Option<String>,
    pub passcode: Option<String>,
    /// `tel:` URIs with pauses and passcode filled in.
    pub phones: Vec<String>,
}

#[derive(uniffi::Enum)]
pub enum AlarmInfo {
    /// Negative minutes go off after the start.
    Before { minutes: i64, email: bool },
    At { unix: i64, email: bool },
}

#[derive(uniffi::Record)]
pub struct EventDetail {
    pub event_id: i64,
    pub calendar_id: i64,
    pub calendar_name: String,
    pub calendar_color: Option<String>,
    pub calendar_access: CalendarAccess,
    pub title: String,
    pub location: String,
    pub url: Option<String>,
    pub time: EventTime,
    /// "Every 2 weeks on Tuesday and Thursday", or "Custom repeat"; None for a single event.
    pub repeat: Option<String>,
    /// A full HTML document for the reader's web view, or empty.
    pub description_html: String,
    pub status: EventStatus,
    pub show_as: ShowAs,
    pub visibility: EventVisibility,
    pub my_response: Option<EventResponse>,
    /// Accept, Maybe and Decline apply: an invitation from someone else in a
    /// calendar Jolta answers through.
    pub can_answer: bool,
    pub organizer: Option<PersonRecord>,
    /// Accepted, maybe, no answer, declined, delegated; empty groups left out.
    pub attendees: Vec<AttendeeGroup>,
    pub rooms: Vec<PersonRecord>,
    /// "4 accepted, 1 maybe, 2 no answer", or empty.
    pub responses: String,
    pub meeting: Option<MeetingInfo>,
    pub alarms: Vec<AlarmInfo>,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationAnswer {
    Accept,
    Maybe,
    Decline,
}

impl From<InvitationAnswer> for rsvp::Answer {
    fn from(a: InvitationAnswer) -> Self {
        match a {
            InvitationAnswer::Accept => rsvp::Answer::Accept,
            InvitationAnswer::Maybe => rsvp::Answer::Maybe,
            InvitationAnswer::Decline => rsvp::Answer::Decline,
        }
    }
}

/// An invitation in the open conversation the user can answer.
#[derive(uniffi::Record)]
pub struct InvitationInfo {
    pub message_id: i64,
    /// Whether its message starts expanded; the reader reports changes.
    pub expanded: bool,
    pub answer: Option<EventResponse>,
    /// For a series, the occurrence the card shows: "Thu 5 Nov". None for
    /// an invitation to one occurrence, which has no scope to choose.
    pub occurrence_day: Option<String>,
    /// "Ann".
    pub organizer: String,
}

impl From<jolta_core::reader::OpenInvitation> for InvitationInfo {
    fn from(i: jolta_core::reader::OpenInvitation) -> Self {
        InvitationInfo {
            message_id: i.answering.message.0,
            expanded: i.expanded,
            answer: i.answer.map(Into::into),
            occurrence_day: i.answering.occurrence.filter(|o| !o.alone).map(|o| o.day),
            organizer: i.answering.organizer,
        }
    }
}

/// How the next answer goes out.
#[derive(uniffi::Record)]
pub struct AnswerOptions {
    /// Goes with the answer; empty for none.
    pub note: String,
    /// False for "Don't send a reply".
    pub notify: bool,
    /// Only the occurrence on screen; a single event ignores it.
    pub occurrence_only: bool,
}

/// One notification to schedule. Replace pending notifications by `id`, and
/// remove pending ones of the same range that are no longer listed.
#[derive(uniffi::Record)]
pub struct ReminderRecord {
    pub id: String,
    pub fire_at: i64,
    pub event_id: i64,
    pub calendar_id: i64,
    pub title: String,
    pub location: String,
    pub time: OccurrenceTime,
    pub meeting_provider: Option<MeetingProvider>,
    pub join_url: Option<String>,
}

#[derive(uniffi::Record)]
pub struct NextMeetingRecord {
    pub event_id: i64,
    pub calendar_id: i64,
    pub color: Option<String>,
    pub title: String,
    pub start: i64,
    pub end: i64,
    pub meeting: MeetingInfo,
}

impl Core {
    async fn respond(&self, subject: respond::Subject, answer: InvitationAnswer, options: AnswerOptions) -> Result<String> {
        let note = Some(options.note.trim().to_string()).filter(|n| !n.is_empty());
        let request = respond::Request { subject, answer: answer.into(), note, notify: options.notify, occurrence_only: options.occurrence_only };
        let jolta = self.jolta.clone();
        let outcome = self
            .runtime
            .spawn(async move { respond::answer(&jolta, &request).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        if let Some(account) = outcome.mailed_from {
            self.running_engine()?.wake(account);
        }
        self.calendar_changed(outcome.calendar);
        Ok(outcome.text)
    }
}

#[uniffi::export]
impl Core {
    /// Answers the invitation on a message's card. Returns the words for the
    /// toast: "Accepted. Jolta emailed Ann."
    pub async fn answer_invitation(&self, message_id: i64, answer: InvitationAnswer, options: AnswerOptions) -> Result<String> {
        self.respond(respond::Subject::Message(jolta_core::MessageId(message_id)), answer, options).await
    }

    /// Answers an event from its detail; `time` names the occurrence, as
    /// for `calendar_event`.
    pub async fn answer_event(&self, event_id: i64, time: Option<OccurrenceTime>, answer: InvitationAnswer, options: AnswerOptions) -> Result<String> {
        let at = time.map(Bounds::try_from).transpose()?;
        self.respond(respond::Subject::Event { id: EventId(event_id), at }, answer, options).await
    }

    /// Every account with its calendar switch and calendars, then the
    /// calendars of no account.
    pub fn calendars(&self) -> Result<CalendarList> {
        let (accounts, calendars, counts, on) = self.jolta.store.read(|c| {
            let accounts = store::accounts(c)?;
            let on = accounts.iter().map(|a| Ok((a.id, store::account_calendar_on(c, a.id)?))).collect::<jolta_core::Result<BTreeMap<_, _>>>()?;
            Ok((accounts, store::calendars(c)?, store::event_counts(c)?, on))
        })?;
        let scheduler = self.scheduler();
        let mut by_account: BTreeMap<Option<AccountId>, Vec<CalendarInfo>> = BTreeMap::new();
        for c in calendars {
            let info = CalendarInfo {
                id: c.id.0,
                account_id: c.account_id.map(|a| a.0),
                color: c.color,
                kind: c.source.into(),
                access: c.access.into(),
                shown: c.shown,
                counts_as_busy: c.counts_as_busy,
                notify: c.notify,
                is_default: c.is_default,
                events: counts.get(&c.id).copied().unwrap_or(0) as u32,
                synced: c.synced,
                sync_error: scheduler.as_ref().and_then(|s| s.error(c.id)),
                name: c.name,
            };
            by_account.entry(c.account_id).or_default().push(info);
        }
        Ok(CalendarList {
            other_calendars: by_account.remove(&None).unwrap_or_default(),
            accounts: accounts
                .into_iter()
                .map(|a| CalendarAccount {
                    calendar_on: on.get(&a.id).copied().unwrap_or(false),
                    needs_consent: sync::needs_consent(&a),
                    calendars: by_account.remove(&Some(a.id)).unwrap_or_default(),
                    account: a.into(),
                })
                .collect(),
        })
    }

    /// Turns calendar on or off for an account. On lists its calendars,
    /// shows the primary one and syncs it; off deletes the account's
    /// calendar data here, never on the server. Google and Microsoft
    /// accounts need calendar consent first (`sync::enable` says so until
    /// the account has it).
    pub async fn set_calendar_enabled(&self, account_id: i64, on: bool) -> Result<Vec<CalendarInfo>> {
        let account = AccountId(account_id);
        if !on {
            let ids = self.jolta.store.read(|c| store::account_calendars(c, account))?;
            sync::disable(&self.jolta, account)?;
            if let Some(engine) = self.engine.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                engine.collect_blobs_later();
            }
            self.calendar_changed(ids.iter().map(|c| c.id));
            return Ok(Vec::new());
        }
        let jolta = self.jolta.clone();
        let list = self
            .runtime
            .spawn(async move { sync::enable(&jolta, account).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        if let Some(scheduler) = self.scheduler() {
            list.iter().filter(|c| c.shown).for_each(|c| scheduler.sync(c.id));
        }
        Ok(self.calendars()?.accounts.into_iter().find(|a| a.account.id == account_id).map(|a| a.calendars).unwrap_or_default())
    }

    /// Showing a calendar syncs it; hiding one stops its syncs and takes it
    /// out of every view.
    pub fn set_calendar_shown(&self, calendar_id: i64, shown: bool) -> Result<()> {
        let id = CalendarId(calendar_id);
        self.jolta.store.write(|tx| store::set_calendar_shown(tx, id, shown))?;
        if let (true, Some(scheduler)) = (shown, self.scheduler()) {
            scheduler.sync(id);
        }
        self.calendar_changed([id]);
        Ok(())
    }

    pub fn set_calendar_busy(&self, calendar_id: i64, busy: bool) -> Result<()> {
        let id = CalendarId(calendar_id);
        self.jolta.store.write(|tx| store::set_calendar_busy(tx, id, busy))?;
        self.calendar_changed([id]);
        Ok(())
    }

    /// Whether the calendar's events remind. Reschedule reminders after.
    pub fn set_calendar_notify(&self, calendar_id: i64, notify: bool) -> Result<()> {
        let id = CalendarId(calendar_id);
        self.jolta.store.write(|tx| store::set_calendar_notify(tx, id, notify))?;
        self.calendar_changed([id]);
        Ok(())
    }

    /// A "#rrggbb" color, or None for the server's.
    pub fn set_calendar_color(&self, calendar_id: i64, color: Option<String>) -> Result<()> {
        let id = CalendarId(calendar_id);
        self.jolta.store.write(|tx| store::set_calendar_color(tx, id, color.as_deref()))?;
        self.calendar_changed([id]);
        Ok(())
    }

    /// Subscribes to a `webcal://` or `https://` feed and downloads it. A
    /// failed first download leaves the subscription, with `sync_error` set,
    /// for the next round to retry.
    pub async fn subscribe_calendar(&self, url: String) -> Result<CalendarInfo> {
        let jolta = self.jolta.clone();
        let (calendar, synced) = self
            .runtime
            .spawn(async move {
                let calendar = sync::subscribe(&jolta, &url).await?;
                let synced = sync::sync_calendar(&jolta, calendar.id).await;
                Ok::<_, jolta_core::Error>((calendar, synced))
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        let error = synced.as_ref().err().map(ToString::to_string);
        if synced.is_ok_and(|r| r.upserted + r.removed > 0) {
            self.calendar_changed([calendar.id]);
        }
        self.listener.on_event(CoreEvent::CalendarSynced { calendar_id: calendar.id.0, error: error.clone() });
        let mut info = self
            .calendars()?
            .other_calendars
            .into_iter()
            .find(|c| c.id == calendar.id.0)
            .ok_or_else(|| CoreError::NotFound { message: format!("calendar {}", calendar.id) })?;
        info.sync_error = info.sync_error.or(error);
        Ok(info)
    }

    /// Removes a feed or local calendar with its events. An account's
    /// calendars are hidden or turned off with the account instead.
    pub fn remove_calendar(&self, calendar_id: i64) -> Result<()> {
        let id = CalendarId(calendar_id);
        let calendar = self.jolta.store.read(|c| store::calendar(c, id))?;
        if calendar.account_id.is_some() {
            return Err(CoreError::Failed {
                message: format!("{} belongs to an account; hide it or turn calendar off for the account", calendar.name),
            });
        }
        self.jolta.store.write(|tx| store::delete_calendar(tx, id))?;
        self.calendar_changed([id]);
        Ok(())
    }

    /// The occurrences in shown calendars on the days from the one holding
    /// `from` through the one holding `to - 1` (unix seconds, days in this
    /// Mac's zone), ordered by start with all-day events first. Pass the
    /// midnights the Mac's calendar gives: a week is not always 7 × 86,400
    /// seconds, and an end an hour past midnight adds a day.
    pub fn calendar_occurrences(&self, from: i64, to: i64) -> Result<Vec<OccurrenceRecord>> {
        if to <= from {
            return Ok(Vec::new());
        }
        let here = tz::here();
        let day = |t: i64| DateTime::from_timestamp(t, 0).unwrap_or_default().with_timezone(&here).date_naive();
        let (first, end) = (day(from), day(to - 1) + Duration::days(1));
        let rows = self.jolta.store.read(|c| store::occurrences(c, first, end, here))?;
        Ok(rows
            .into_iter()
            .map(|o| OccurrenceRecord {
                event_id: o.event_id.0,
                calendar_id: o.calendar_id.0,
                uid: o.uid,
                color: o.color,
                title: o.title,
                location: o.location,
                time: o.bounds.into(),
                status: o.status.into(),
                show_as: o.show_as.into(),
                my_response: o.my_response.map(Into::into),
                meeting_provider: o.meeting.as_ref().map(|m| m.provider.into()),
                join_url: o.meeting.map(|m| m.url),
                has_attendees: o.has_attendees,
                busy: o.busy,
                copies: o.copies,
            })
            .collect())
    }

    /// Everything the detail shows for one occurrence (`time` from its
    /// row), or for the event's own time without one. `app_css` styles the
    /// description document, as for `open_thread`.
    pub fn calendar_event(&self, event_id: i64, time: Option<OccurrenceTime>, app_css: String) -> Result<EventDetail> {
        let at = time.map(Bounds::try_from).transpose()?;
        let (d, account) = self.jolta.store.read(|c| {
            let d = detail::open(c, EventId(event_id), at, tz::here(), &app_css)?;
            let account = d.calendar.account_id.map(|a| store::account(c, a)).transpose()?.map(|a| a.address).unwrap_or_default();
            Ok((d, account))
        })?;
        let can_answer = respond::can_answer(&account, &d.calendar, &d.event);
        let person = |p: cal::Person| PersonRecord { name: p.name, address: p.address };
        let e = d.event;
        Ok(EventDetail {
            event_id,
            calendar_id: d.calendar.id.0,
            calendar_name: d.calendar.name,
            calendar_color: d.calendar.color,
            calendar_access: d.calendar.access.into(),
            title: e.title,
            location: e.location,
            url: e.url,
            time: match d.when {
                detail::When::AllDay { first, last } => EventTime::AllDay { first_day: day_text(first), last_day: day_text(last) },
                detail::When::Timed { start, end, zone, text } => EventTime::Timed {
                    start: start.timestamp(),
                    end: end.timestamp(),
                    zone: zone.map(|z| z.name().to_string()),
                    text,
                },
            },
            repeat: d.repeat,
            description_html: d.description_html,
            status: e.status.into(),
            show_as: e.show_as.into(),
            visibility: e.visibility.into(),
            my_response: e.attendees.iter().find(|a| a.is_me).map(|a| a.response.into()),
            can_answer,
            organizer: e.organizer.map(person),
            attendees: d
                .attendees
                .into_iter()
                .map(|(response, people)| AttendeeGroup {
                    response: response.into(),
                    people: people
                        .into_iter()
                        .map(|a| AttendeeRecord {
                            name: a.person.name,
                            address: a.person.address,
                            role: a.role.into(),
                            comment: a.comment,
                            is_me: a.is_me,
                        })
                        .collect(),
                })
                .collect(),
            rooms: d.rooms.into_iter().map(person).collect(),
            responses: d.responses,
            meeting: e.meeting.map(|m| meeting_info(m, d.app_url)),
            alarms: e
                .alarms
                .iter()
                .map(|a| match a.trigger {
                    cal::AlarmTrigger::MinutesBefore(minutes) => AlarmInfo::Before { minutes, email: a.email },
                    cal::AlarmTrigger::At(at) => AlarmInfo::At { unix: at.timestamp(), email: a.email },
                })
                .collect(),
        })
    }

    /// The reminders that go off from `from` up to `to` (unix seconds), in
    /// order. Alarms more than 4 weeks ahead of their event aren't found.
    pub fn calendar_reminders(&self, from: i64, to: i64) -> Result<Vec<ReminderRecord>> {
        let (from, to) = (instant(from), instant(to));
        let list = self.jolta.store.read(|c| remind::between(c, from, to, tz::here()))?;
        Ok(list
            .into_iter()
            .map(|r| ReminderRecord {
                id: r.id,
                fire_at: r.fire_at.timestamp(),
                event_id: r.event_id.0,
                calendar_id: r.calendar_id.0,
                title: r.title,
                location: r.location,
                time: r.bounds.into(),
                meeting_provider: r.meeting.as_ref().map(|m| m.provider.into()),
                join_url: r.meeting.map(|m| m.url),
            })
            .collect())
    }

    /// The reminder `id` that fires at `fire_at`, as it stands now: a sync
    /// may have replaced its event's row since it was scheduled. None when
    /// the event moved or went, or its calendar is hidden or silent.
    pub fn calendar_reminder(&self, id: String, fire_at: i64) -> Result<Option<ReminderRecord>> {
        Ok(self.calendar_reminders(fire_at, fire_at + 1)?.into_iter().find(|r| r.id == id))
    }

    /// The online meeting to join at `now`: one starting within 10 minutes,
    /// else the one running, else the next within a week.
    pub fn next_meeting(&self, now: i64) -> Result<Option<NextMeetingRecord>> {
        let next = self.jolta.store.read(|c| remind::next_meeting(c, instant(now)))?;
        Ok(next.map(|m| NextMeetingRecord {
            event_id: m.event_id.0,
            calendar_id: m.calendar_id.0,
            color: m.color,
            title: m.title,
            start: m.start.timestamp(),
            end: m.end.timestamp(),
            meeting: meeting_info(m.meeting, m.app_url),
        }))
    }
}

impl Core {
    fn scheduler(&self) -> Option<Arc<Scheduler>> {
        self.engine.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|e| e.calendars().clone())
    }

    pub(crate) fn calendar_changed(&self, ids: impl IntoIterator<Item = CalendarId>) {
        self.listener.on_event(CoreEvent::CalendarChanged { calendar_ids: ids.into_iter().map(|c| c.0).collect() });
    }
}

fn meeting_info(m: cal::Meeting, app_url: Option<String>) -> MeetingInfo {
    MeetingInfo {
        provider: m.provider.into(),
        join_url: m.join_url,
        app_url,
        meeting_id: m.meeting_id,
        passcode: m.passcode,
        phones: m.phones,
    }
}

fn instant(unix: i64) -> DateTime<chrono::Utc> {
    DateTime::from_timestamp(unix, 0).unwrap_or_default()
}

fn day_text(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn day_of(n: i32) -> NaiveDate {
    NaiveDate::default() + Duration::days(n.into())
}

impl From<Bounds> for OccurrenceTime {
    fn from(b: Bounds) -> Self {
        match b {
            Bounds::Utc { start, end } => OccurrenceTime::Timed { start: start.timestamp(), end: end.timestamp() },
            Bounds::Days { start, end } => {
                OccurrenceTime::AllDay { first_day: day_text(day_of(start)), last_day: day_text(day_of((end - 1).max(start))) }
            }
        }
    }
}

impl TryFrom<OccurrenceTime> for Bounds {
    type Error = CoreError;

    fn try_from(t: OccurrenceTime) -> Result<Bounds> {
        match t {
            OccurrenceTime::Timed { start, end } => Ok(Bounds::Utc { start: instant(start), end: instant(end) }),
            OccurrenceTime::AllDay { first_day, last_day } => {
                let parse = |s: &str| {
                    NaiveDate::parse_from_str(s, "%Y-%m-%d")
                        .map(tz::day_number)
                        .map_err(|_| CoreError::Query { message: format!("{s} is not a day like 2026-11-09") })
                };
                Ok(Bounds::Days { start: parse(&first_day)?, end: parse(&last_day)? + 1 })
            }
        }
    }
}
