//! The event editor for the app: drafts to open, their problems, saving
//! and deleting through the calendar op queue, and the editor's settings.
//! Times cross as unix seconds and all-day events as "YYYY-MM-DD" days, as
//! for the views; Swift keeps no editing rules.

use chrono::{Duration, NaiveDate, NaiveTime, Timelike, Utc};
use jolta_core::calendar::draft::{self, Reminder};
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{self as cal, CalendarId, EventId, Span, Zone, detail, tz};
use jolta_core::store;

use crate::calendar::{AttendeeRole, EventResponse, EventVisibility, MeetingInfo, MeetingProvider, OccurrenceTime, PersonRecord, ShowAs, meeting_info};
use crate::{Core, CoreError, Result};

/// A part of the editor, top to bottom.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DraftField {
    Title,
    Time,
    Repeat,
    People,
    Room,
    Location,
    Video,
    Description,
    Calendar,
    ShowAs,
    Reminder,
    Visibility,
}

impl From<draft::DraftField> for DraftField {
    fn from(f: draft::DraftField) -> Self {
        use draft::DraftField as F;
        match f {
            F::Title => DraftField::Title,
            F::Time => DraftField::Time,
            F::Repeat => DraftField::Repeat,
            F::People => DraftField::People,
            F::Room => DraftField::Room,
            F::Location => DraftField::Location,
            F::Video => DraftField::Video,
            F::Description => DraftField::Description,
            F::Calendar => DraftField::Calendar,
            F::ShowAs => DraftField::ShowAs,
            F::Reminder => DraftField::Reminder,
            F::Visibility => DraftField::Visibility,
        }
    }
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftReminder {
    /// A new event on its calendar's own default, which the provider applies.
    Default,
    Off,
    Before { minutes: i64 },
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct DraftPerson {
    pub name: String,
    pub address: String,
    pub role: AttendeeRole,
    pub response: EventResponse,
    pub comment: String,
    pub is_me: bool,
}

/// The event as the editor holds it. Pass it back whole: `base` and `zone`
/// are for the core.
#[derive(uniffi::Record)]
pub struct EventDraft {
    pub calendar_id: i64,
    /// The stored event the editor opened, as the core reads it; None for a new one.
    pub base: Option<String>,
    pub uid: String,
    pub title: String,
    pub time: OccurrenceTime,
    /// The zone the event's times are kept in: an IANA name, or empty for
    /// times that float with the Mac.
    pub zone: String,
    /// "10:00 to 11:00 Lisbon (11:00 to 12:00 here)" when the event's zone
    /// isn't the Mac's.
    pub zone_text: Option<String>,
    pub people: Vec<DraftPerson>,
    pub room: String,
    pub location: String,
    /// The user's own text; a meeting block stays out of it and is kept on save.
    pub description: String,
    pub video: Option<MeetingProvider>,
    pub show_as: ShowAs,
    pub reminder: DraftReminder,
    pub visibility: EventVisibility,
    /// Shown as plain text with a lock, never as controls.
    pub locks: Vec<DraftField>,
    /// Why, said once at the top of the panel.
    pub notice: Option<String>,
    pub organizer: Option<PersonRecord>,
    pub meeting: Option<MeetingInfo>,
    /// The repeat rule in words, None for a single event.
    pub repeat: Option<String>,
    pub description_note: Option<String>,
}

/// What another feature fills in when it opens the editor.
#[derive(uniffi::Record)]
pub struct EventPrefill {
    /// The default calendar when None.
    pub calendar_id: Option<i64>,
    pub title: String,
    /// The next half hour for the default length when None.
    pub time: Option<OccurrenceTime>,
    pub required: Vec<PersonRecord>,
    pub optional: Vec<PersonRecord>,
    pub room: Option<String>,
    pub description: String,
    pub video: Option<MeetingProvider>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct DraftProblem {
    /// None for one about the whole event, shown above the buttons.
    pub field: Option<DraftField>,
    pub text: String,
    /// Save waits until it is fixed; otherwise a warning.
    pub blocking: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct SavePlan {
    /// Nothing differs from the stored event: saving only closes.
    pub unchanged: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct SavedEdit {
    /// For `undo_calendar`; None when nothing changed.
    pub token: Option<i64>,
    /// "Saved Standup".
    pub message: String,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct DeletePlan {
    /// Why the event can't be deleted, if it can't.
    pub refusal: Option<String>,
    pub title: String,
}

fn failed(e: impl std::fmt::Display) -> CoreError {
    CoreError::Failed { message: e.to_string() }
}

fn day(text: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| CoreError::Query { message: format!("{text} is not a day like 2026-11-09") })
}

fn zone_of(name: &str) -> Zone {
    tz::zone(name).map_or(Zone::Floating, Zone::Tz)
}

/// The core's span for a time from Swift, kept in `zone`.
fn span_of(time: &OccurrenceTime, zone: Zone) -> Result<Span> {
    let here = tz::here();
    Ok(match time {
        OccurrenceTime::Timed { start, end } => draft::timed_span(*start, *end, zone, here),
        OccurrenceTime::AllDay { first_day, last_day } => Span::AllDay { start: day(first_day)?, end: day(last_day)? + Duration::days(1) },
    })
}

fn time_of(span: &Span) -> OccurrenceTime {
    let here = tz::here();
    match span {
        Span::Timed { start, end } => OccurrenceTime::Timed { start: start.instant(here).timestamp(), end: end.instant(here).timestamp() },
        Span::AllDay { start, end } => OccurrenceTime::AllDay {
            first_day: start.format("%Y-%m-%d").to_string(),
            last_day: (*end - Duration::days(1)).max(*start).format("%Y-%m-%d").to_string(),
        },
    }
}

fn person(p: &PersonRecord) -> cal::Person {
    cal::Person { address: p.address.trim().to_string(), name: p.name.clone() }
}

impl From<draft::EventDraft> for EventDraft {
    fn from(d: draft::EventDraft) -> Self {
        let here = tz::here();
        let zone = match d.span {
            Span::Timed { start, .. } => match start.zone {
                Zone::Tz(t) => t.name().to_string(),
                Zone::Floating => String::new(),
            },
            Span::AllDay { .. } => String::new(),
        };
        let zone_text = match detail::when(&d.span, d.span.bounds(here), here) {
            detail::When::Timed { zone: Some(_), text, .. } => Some(text),
            _ => None,
        };
        EventDraft {
            calendar_id: d.calendar.0,
            base: d.base.as_ref().and_then(|b| serde_json::to_string(b).ok()),
            uid: d.uid,
            title: d.title,
            time: time_of(&d.span),
            zone,
            zone_text,
            people: d
                .people
                .into_iter()
                .map(|a| DraftPerson {
                    name: a.person.name,
                    address: a.person.address,
                    role: a.role.into(),
                    response: a.response.into(),
                    comment: a.comment,
                    is_me: a.is_me,
                })
                .collect(),
            room: d.room,
            location: d.location,
            description: d.description,
            video: d.video.map(Into::into),
            show_as: d.show_as.into(),
            reminder: match d.reminder {
                Reminder::Default => DraftReminder::Default,
                Reminder::Off => DraftReminder::Off,
                Reminder::Before(minutes) => DraftReminder::Before { minutes },
            },
            visibility: d.visibility.into(),
            locks: d.locks.into_iter().map(Into::into).collect(),
            notice: d.notice,
            organizer: d.organizer.map(|p| PersonRecord { name: p.name, address: p.address }),
            meeting: d.meeting.map(|m| {
                let app = cal::meeting::app_url(&m);
                meeting_info(m, app)
            }),
            repeat: d.repeat,
            description_note: d.description_note,
        }
    }
}

impl TryFrom<&EventDraft> for draft::EventDraft {
    type Error = CoreError;

    /// Locks and what is only shown don't come back: the core reads them
    /// from the store again.
    fn try_from(d: &EventDraft) -> Result<Self> {
        let base = d.base.as_deref().map(serde_json::from_str::<cal::EventData>).transpose().map_err(failed)?;
        Ok(draft::EventDraft {
            calendar: CalendarId(d.calendar_id),
            base,
            uid: d.uid.clone(),
            title: d.title.clone(),
            span: span_of(&d.time, zone_of(&d.zone))?,
            people: d
                .people
                .iter()
                .map(|p| cal::Attendee {
                    person: cal::Person { address: p.address.trim().to_string(), name: p.name.clone() },
                    role: p.role.into(),
                    response: p.response.into(),
                    comment: p.comment.clone(),
                    is_me: p.is_me,
                })
                .collect(),
            room: d.room.clone(),
            location: d.location.clone(),
            description: d.description.clone(),
            video: d.video.map(Into::into),
            show_as: d.show_as.into(),
            reminder: match d.reminder {
                DraftReminder::Default => Reminder::Default,
                DraftReminder::Off => Reminder::Off,
                DraftReminder::Before { minutes } => Reminder::Before(minutes),
            },
            visibility: d.visibility.into(),
            locks: Vec::new(),
            notice: None,
            organizer: None,
            meeting: None,
            repeat: None,
            description_note: None,
        })
    }
}

impl Core {
    /// After a save or delete: the worker sends it, the views redraw.
    fn edited(&self, calendar: CalendarId) {
        if let Some(scheduler) = self.scheduler() {
            scheduler.ops_queued();
        }
        self.calendar_changed([calendar]);
    }
}

fn horizon() -> Horizon {
    Horizon::around(Utc::now(), tz::here())
}

#[uniffi::export]
impl Core {
    /// A draft for a new event at `time` (the next half hour for the
    /// default length without one) in `calendar_id`, or the default calendar.
    pub fn new_draft(&self, calendar_id: Option<i64>, time: Option<OccurrenceTime>) -> Result<EventDraft> {
        let here = tz::here();
        let span = time.as_ref().map(|t| span_of(t, Zone::Tz(here))).transpose()?;
        let d = self.jolta.store.read(|c| draft::draft_for_new(c, calendar_id.map(CalendarId), span, Utc::now(), here))?;
        Ok(d.into())
    }

    /// A new event filled in by another feature (C4's Invite and Book, C5's
    /// Schedule meeting).
    pub fn draft_from_prefill(&self, prefill: EventPrefill) -> Result<EventDraft> {
        let here = tz::here();
        let core = draft::EventPrefill {
            calendar: prefill.calendar_id.map(CalendarId),
            title: prefill.title,
            span: prefill.time.as_ref().map(|t| span_of(t, Zone::Tz(here))).transpose()?,
            required: prefill.required.iter().map(person).collect(),
            optional: prefill.optional.iter().map(person).collect(),
            room: prefill.room,
            description: prefill.description,
            video: prefill.video.map(Into::into),
        };
        Ok(self.jolta.store.read(|c| draft::draft_from_prefill(c, &core, Utc::now(), here))?.into())
    }

    /// A draft of a stored event at one occurrence (`time` from its row),
    /// with what the user may not change and why.
    pub fn draft_for_event(&self, event_id: i64, time: Option<OccurrenceTime>) -> Result<EventDraft> {
        let at = time.map(cal::Bounds::try_from).transpose()?;
        Ok(self.jolta.store.read(|c| draft::draft_for_event(c, EventId(event_id), at, tz::here()))?.into())
    }

    /// Everything wrong with the draft for its calendar; run on every change.
    pub fn check_draft(&self, draft: EventDraft) -> Result<Vec<DraftProblem>> {
        let d = draft::EventDraft::try_from(&draft)?;
        let problems = self.jolta.store.read(|c| draft::check_draft(c, &d, tz::here()))?;
        Ok(problems.into_iter().map(|p| DraftProblem { field: p.field.map(Into::into), text: p.text, blocking: p.blocking }).collect())
    }

    pub fn plan_save(&self, draft: EventDraft) -> Result<SavePlan> {
        let d = draft::EventDraft::try_from(&draft)?;
        Ok(SavePlan { unchanged: draft::plan_save(&d).unchanged })
    }

    /// Saves the draft, telling nobody. The returned token undoes it.
    pub fn save_event(&self, draft: EventDraft) -> Result<SavedEdit> {
        let d = draft::EventDraft::try_from(&draft)?;
        let h = horizon();
        let saved = self.jolta.store.write(|tx| draft::save_draft(tx, &d, Utc::now().timestamp(), &h, tz::here()))?;
        self.edited(saved.calendar);
        Ok(SavedEdit { token: saved.op.map(|o| o.0), message: saved.message })
    }

    pub fn plan_delete(&self, event_id: i64) -> Result<DeletePlan> {
        let plan = self.jolta.store.read(|c| draft::plan_delete(c, EventId(event_id), tz::here()))?;
        Ok(DeletePlan { refusal: plan.refusal, title: plan.title })
    }

    /// Deletes an event without guests, telling nobody. The returned token undoes it.
    pub fn delete_event(&self, event_id: i64) -> Result<SavedEdit> {
        let h = horizon();
        let saved = self.jolta.store.write(|tx| draft::delete_event(tx, EventId(event_id), Utc::now().timestamp(), &h, tz::here()))?;
        self.edited(saved.calendar);
        Ok(SavedEdit { token: saved.op.map(|o| o.0), message: saved.message })
    }

    /// Deletes the stored event a draft opened, found again by its UID, as
    /// the editor's Delete does. The returned token undoes it.
    pub fn delete_draft(&self, draft: EventDraft) -> Result<SavedEdit> {
        let d = draft::EventDraft::try_from(&draft)?;
        let h = horizon();
        let saved = self.jolta.store.write(|tx| draft::delete_drafted(tx, &d, Utc::now().timestamp(), &h, tz::here()))?;
        self.edited(saved.calendar);
        Ok(SavedEdit { token: saved.op.map(|o| o.0), message: saved.message })
    }

    /// A time typed in a time field, as minutes after midnight: "930",
    /// "14:30", "2pm", "9u30". None when it isn't a time.
    pub fn parse_time_input(&self, text: String) -> Option<i32> {
        cal::quick::time(&text).map(|t: NaiveTime| (t.hour() * 60 + t.minute()) as i32)
    }

    /// Minutes a new event lasts.
    pub fn default_event_length(&self) -> Result<i64> {
        Ok(self.jolta.store.read(store::default_event_minutes)?)
    }

    pub fn set_default_event_length(&self, minutes: i64) -> Result<()> {
        Ok(self.jolta.store.write(|tx| store::set_default_event_minutes(tx, minutes))?)
    }

    /// Makes the calendar the one new events go to.
    pub fn set_default_calendar(&self, calendar_id: i64) -> Result<()> {
        self.jolta.store.write(|tx| store::set_default_calendar(tx, Some(CalendarId(calendar_id))))?;
        self.calendar_changed([CalendarId(calendar_id)]);
        Ok(())
    }

    /// The calendar new events go to, if any calendar takes them.
    pub fn default_calendar(&self) -> Result<Option<i64>> {
        Ok(self.jolta.store.read(draft::default_calendar)?.map(|c| c.id.0))
    }
}
