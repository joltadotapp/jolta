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
    Options,
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
            F::Options => DraftField::Options,
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

/// What a meeting's organizer lets its guests do. The calendar's
/// `capabilities` say which of these it has.
#[derive(uniffi::Record, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeetingOptions {
    pub request_responses: bool,
    pub allow_new_times: bool,
    pub hide_guests: bool,
    pub guests_invite: bool,
    pub guests_modify: bool,
}

impl From<cal::MeetingOptions> for MeetingOptions {
    fn from(o: cal::MeetingOptions) -> Self {
        MeetingOptions {
            request_responses: o.request_responses,
            allow_new_times: o.allow_new_times,
            hide_guests: o.hide_guests,
            guests_invite: o.guests_invite,
            guests_modify: o.guests_modify,
        }
    }
}

impl From<MeetingOptions> for cal::MeetingOptions {
    fn from(o: MeetingOptions) -> Self {
        cal::MeetingOptions {
            request_responses: o.request_responses,
            allow_new_times: o.allow_new_times,
            hide_guests: o.hide_guests,
            guests_invite: o.guests_invite,
            guests_modify: o.guests_modify,
        }
    }
}

/// Whom a save or delete tells, from the dialog's choice.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyChoice {
    All,
    /// Google's "Only guests who don't use Google Calendar".
    Outside,
    /// Only the people added or removed.
    NewOnly,
    None,
}

impl From<cal::ops::Notify> for NotifyChoice {
    fn from(n: cal::ops::Notify) -> Self {
        match n {
            cal::ops::Notify::All => NotifyChoice::All,
            cal::ops::Notify::Outside => NotifyChoice::Outside,
            cal::ops::Notify::Changed => NotifyChoice::NewOnly,
            cal::ops::Notify::Nobody => NotifyChoice::None,
        }
    }
}

impl From<NotifyChoice> for cal::ops::Notify {
    fn from(n: NotifyChoice) -> Self {
        match n {
            NotifyChoice::All => cal::ops::Notify::All,
            NotifyChoice::Outside => cal::ops::Notify::Outside,
            NotifyChoice::NewOnly => cal::ops::Notify::Changed,
            NotifyChoice::None => cal::ops::Notify::Nobody,
        }
    }
}

/// Where a dialog's button sits and which key takes it.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceStyle {
    /// Return.
    Primary,
    /// Esc: back to the editor.
    Back,
    /// Apart on the left, as "Don't save" in macOS.
    Apart,
    /// A small link under the buttons.
    Link,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct SendChoice {
    pub label: String,
    /// What saving with it tells the guests; None for Keep editing.
    pub notify: Option<NotifyChoice>,
    pub style: ChoiceStyle,
}

/// Send an update?, its words all from the core.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct UpdateDialogInfo {
    pub title: String,
    pub body: String,
    /// Where a message field would be: "To add a note, use Email guests after saving."
    pub note: Option<String>,
    /// Shows a message field: Jolta's own mail carries the user's words.
    pub message: bool,
    pub choices: Vec<SendChoice>,
}

/// An account the user can pick in Send invitations from.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct SenderInfo {
    pub account_id: i64,
    pub address: String,
    pub name: String,
}

fn senders(list: Vec<draft::Sender>) -> Vec<SenderInfo> {
    list.into_iter().map(|s| SenderInfo { account_id: s.account.0, address: s.address, name: s.name }).collect()
}

impl From<cal::change::UpdateDialog> for UpdateDialogInfo {
    fn from(d: cal::change::UpdateDialog) -> Self {
        use cal::change::ChoiceStyle as S;
        UpdateDialogInfo {
            title: d.title,
            body: d.body,
            note: d.note,
            message: d.message,
            choices: d
                .choices
                .into_iter()
                .map(|c| SendChoice {
                    label: c.label,
                    notify: c.notify.map(Into::into),
                    style: match c.style {
                        S::Primary => ChoiceStyle::Primary,
                        S::Back => ChoiceStyle::Back,
                        S::Apart => ChoiceStyle::Apart,
                        S::Link => ChoiceStyle::Link,
                    },
                })
                .collect(),
        }
    }
}

/// "Cancel the meeting for 6 people?": Return and Esc keep it, ⌘⌫ cancels it.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct CancelDialogInfo {
    /// The scope it is for, after the scope prompt; All for an event that doesn't repeat.
    pub scope: EditScope,
    pub title: String,
    pub body: String,
    /// Shows a message field: the cancellation carries the user's words.
    pub message: bool,
    pub note: Option<String>,
}

/// "Decline and remove?" for a meeting someone else organizes.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct DeclineInfo {
    pub title: String,
    pub body: String,
    /// For "Tell Jan Peeters", ticked by default.
    pub organizer: String,
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
    pub options: MeetingOptions,
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
    /// The RRULE the series repeats by, as the user set it: for an
    /// occurrence, its series'. None for a single event. Set it from a
    /// preset's or the custom part's rule (`repeat_presets`, `custom_rule`).
    pub rule: Option<String>,
    /// The series' repeat as the editor opened it, for the core: pass it back.
    pub rule_was: Option<String>,
    pub description_note: Option<String>,
    /// "4 accepted, 1 maybe, 2 no answer"; empty without guests.
    pub responses: String,
    /// Under Show as: "Your guests see this with the next update."
    pub show_as_note: Option<String>,
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
    /// For a change to one occurrence of a series, the scope prompt's
    /// choices in order, the first the default (Return); keys 1, 2, 3.
    pub scopes: Vec<ScopeChoice>,
    /// The prompt's line: "You moved Standup to 10:00."
    pub prompt: Option<String>,
    /// Send an update?, after the scope prompt; saving then passes the
    /// chosen button's notify to `save_event`.
    pub dialog: Option<UpdateDialogInfo>,
    /// Send invitations from, before the dialog: the accounts to pick from
    /// with `set_send_from` when the calendar has none to mail from yet.
    pub senders: Vec<SenderInfo>,
}

/// Which events of a series a change or delete is for.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditScope {
    This,
    Following,
    All,
}

impl From<cal::ops::Scope> for EditScope {
    fn from(s: cal::ops::Scope) -> Self {
        match s {
            cal::ops::Scope::This => EditScope::This,
            cal::ops::Scope::Following => EditScope::Following,
            cal::ops::Scope::All => EditScope::All,
        }
    }
}

impl From<EditScope> for cal::ops::Scope {
    fn from(s: EditScope) -> Self {
        match s {
            EditScope::This => cal::ops::Scope::This,
            EditScope::Following => cal::ops::Scope::Following,
            EditScope::All => cal::ops::Scope::All,
        }
    }
}

/// One row of the scope prompt.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct ScopeChoice {
    pub scope: EditScope,
    /// "This and following events".
    pub label: String,
    /// What it costs here: "Starts a new series from Tue 24 Nov."
    pub detail: Option<String>,
}

fn choices(list: Vec<draft::ScopeChoice>) -> Vec<ScopeChoice> {
    list.into_iter().map(|c| ScopeChoice { scope: c.scope.into(), label: c.label, detail: c.detail }).collect()
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct SavedEdit {
    /// For `undo_calendar`; None when nothing changed.
    pub token: Option<i64>,
    /// The UID the saved event has now, to select it: a new one after This
    /// and following.
    pub uid: String,
    /// "Saved Standup".
    pub message: String,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct DeletePlan {
    /// Why the event can't be deleted, if it can't.
    pub refusal: Option<String>,
    pub title: String,
    /// For one occurrence of a series, the scope prompt's choices.
    pub scopes: Vec<ScopeChoice>,
    /// The prompt's line: "You're deleting Standup on Tue 24 Nov."
    pub prompt: Option<String>,
    /// For a meeting the user organizes, the cancel dialog per scope.
    pub cancel: Vec<CancelDialogInfo>,
    /// For a meeting someone else organizes, in place of a delete.
    pub decline: Option<DeclineInfo>,
    /// Send invitations from, before the cancel dialog.
    pub senders: Vec<SenderInfo>,
}

impl From<draft::DeletePlan> for DeletePlan {
    fn from(plan: draft::DeletePlan) -> Self {
        DeletePlan {
            refusal: plan.refusal,
            title: plan.title,
            scopes: choices(plan.scopes),
            prompt: plan.prompt,
            cancel: plan
                .cancel
                .into_iter()
                .map(|(scope, d)| CancelDialogInfo { scope: scope.into(), title: d.title, body: d.body, message: d.message, note: d.note })
                .collect(),
            decline: plan.decline.map(|d| DeclineInfo { title: d.title, body: d.body, organizer: d.organizer }),
            senders: senders(plan.senders),
        }
    }
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
            options: d.options.into(),
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
            rule: d.rule.as_ref().map(|r| r.rules.join("\n")),
            rule_was: d.rule_was.as_ref().and_then(|r| serde_json::to_string(r).ok()),
            description_note: d.description_note,
            responses: d.responses,
            show_as_note: d.show_as_note,
        }
    }
}

/// The draft's repeat: its rule text over the series' EXDATEs and RDATEs
/// as opened, or exactly what it opened with when the rule is the same.
pub(crate) fn rule_of(rule: Option<&str>, was: Option<&str>) -> Result<(Option<cal::Recurrence>, Option<cal::Recurrence>)> {
    let was: Option<cal::Recurrence> = was.map(serde_json::from_str).transpose().map_err(failed)?;
    let rules: Option<Vec<String>> = rule.map(|r| r.lines().map(|l| l.trim().trim_start_matches("RRULE:").to_string()).filter(|l| !l.is_empty()).collect());
    let rule = match (rules, &was) {
        (None, _) => None,
        (Some(rules), _) if rules.is_empty() => None,
        (Some(rules), Some(w)) if rules == w.rules => Some(w.clone()),
        (Some(rules), Some(w)) => Some(cal::Recurrence { rules, ..w.clone() }),
        (Some(rules), None) => Some(cal::Recurrence { rules, ..Default::default() }),
    };
    Ok((rule, was))
}

impl TryFrom<&EventDraft> for draft::EventDraft {
    type Error = CoreError;

    /// Locks and what is only shown don't come back: the core reads them
    /// from the store again.
    fn try_from(d: &EventDraft) -> Result<Self> {
        let base = d.base.as_deref().map(serde_json::from_str::<cal::EventData>).transpose().map_err(failed)?;
        let (rule, rule_was) = rule_of(d.rule.as_deref(), d.rule_was.as_deref())?;
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
            options: d.options.into(),
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
            rule,
            rule_was,
            description_note: None,
            responses: String::new(),
            show_as_note: None,
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

    /// A new event like the occurrence `time` of a stored event, at its
    /// time, without its people, repeat or meeting: ⌘D and ⌥-drag.
    pub fn copy_draft(&self, event_id: i64, time: Option<OccurrenceTime>) -> Result<EventDraft> {
        let at = time.map(cal::Bounds::try_from).transpose()?;
        Ok(self.jolta.store.read(|c| draft::draft_for_copy(c, EventId(event_id), at, Utc::now(), tz::here()))?.into())
    }

    /// Everything wrong with the draft for its calendar; run on every change.
    pub fn check_draft(&self, draft: EventDraft) -> Result<Vec<DraftProblem>> {
        let d = draft::EventDraft::try_from(&draft)?;
        let problems = self.jolta.store.read(|c| draft::check_draft(c, &d, tz::here()))?;
        Ok(problems.into_iter().map(|p| DraftProblem { field: p.field.map(Into::into), text: p.text, blocking: p.blocking }).collect())
    }

    /// What saving needs first: for one occurrence of a series, the scope prompt.
    pub fn plan_save(&self, draft: EventDraft) -> Result<SavePlan> {
        let d = draft::EventDraft::try_from(&draft)?;
        let plan = self.jolta.store.read(|c| draft::plan_save(c, &d, tz::here()))?;
        Ok(SavePlan { unchanged: plan.unchanged, scopes: choices(plan.scopes), prompt: plan.prompt, dialog: plan.dialog.map(Into::into), senders: senders(plan.senders) })
    }

    /// Saves the draft for `scope` when `plan_save` asked for one, telling
    /// the guests what `notify` says when it showed Send an update? (the
    /// chosen button's), else nobody. One that mails waits the undo window.
    /// The returned token undoes it.
    pub fn save_event(&self, draft: EventDraft, scope: Option<EditScope>, notify: Option<NotifyChoice>, message: Option<String>) -> Result<SavedEdit> {
        let d = draft::EventDraft::try_from(&draft)?;
        let h = horizon();
        let saved = self.jolta.store.write(|tx| draft::save_draft(tx, &d, scope.map(Into::into), notify.map(Into::into), message, Utc::now().timestamp(), &h, tz::here()))?;
        self.edited(saved.calendar);
        Ok(SavedEdit { token: saved.op.map(|o| o.0), uid: saved.uid, message: saved.message })
    }

    /// Whether the event can be deleted, and at the occurrence `time` of a
    /// series, the scope prompt's choices.
    pub fn plan_delete(&self, event_id: i64, time: Option<OccurrenceTime>) -> Result<DeletePlan> {
        let at = time.map(cal::Bounds::try_from).transpose()?;
        Ok(self.jolta.store.read(|c| draft::plan_delete(c, EventId(event_id), at, tz::here()))?.into())
    }

    /// `plan_delete` for the stored event a draft opened, found again by its
    /// UID and occurrence, as `delete_draft` deletes it.
    pub fn plan_delete_draft(&self, draft: EventDraft) -> Result<DeletePlan> {
        let d = draft::EventDraft::try_from(&draft)?;
        Ok(self.jolta.store.read(|c| draft::plan_delete_drafted(c, &d, tz::here()))?.into())
    }

    /// Deletes an event, or cancels a meeting the user organizes for its
    /// guests with `message` where the cancellation carries one. The
    /// returned token undoes it until the cancellation goes out.
    pub fn delete_event(&self, event_id: i64, message: Option<String>) -> Result<SavedEdit> {
        let h = horizon();
        let saved = self.jolta.store.write(|tx| draft::delete_event(tx, EventId(event_id), message, Utc::now().timestamp(), &h, tz::here()))?;
        self.edited(saved.calendar);
        Ok(SavedEdit { token: saved.op.map(|o| o.0), uid: saved.uid, message: saved.message })
    }

    /// Deletes the stored event a draft opened, found again by its UID, as
    /// the editor's Delete does; one occurrence of a series for `scope`.
    /// The returned token undoes it.
    pub fn delete_draft(&self, draft: EventDraft, scope: Option<EditScope>, message: Option<String>) -> Result<SavedEdit> {
        let d = draft::EventDraft::try_from(&draft)?;
        let h = horizon();
        let saved = self.jolta.store.write(|tx| draft::delete_drafted(tx, &d, scope.map(Into::into), message, Utc::now().timestamp(), &h, tz::here()))?;
        self.edited(saved.calendar);
        Ok(SavedEdit { token: saved.op.map(|o| o.0), uid: saved.uid, message: saved.message })
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

    /// The account a calendar on this Mac sends its meetings' invitations
    /// from: Send invitations from, and Settings › Calendar.
    pub fn set_send_from(&self, calendar_id: i64, account_id: i64) -> Result<()> {
        let id = CalendarId(calendar_id);
        self.jolta.store.write(|tx| {
            let calendar = store::calendar(tx, id)?;
            if !cal::organizer::sends_itself(&calendar) {
                return Err(jolta_core::Error::Other(format!("{} mails its guests itself", calendar.name)));
            }
            store::account(tx, jolta_core::AccountId(account_id))?;
            store::set_calendar_send_from(tx, id, Some(jolta_core::AccountId(account_id)))
        })?;
        self.calendar_changed([id]);
        Ok(())
    }

    /// The calendar new events go to, if any calendar takes them.
    pub fn default_calendar(&self) -> Result<Option<i64>> {
        Ok(self.jolta.store.read(draft::default_calendar)?.map(|c| c.id.0))
    }
}
