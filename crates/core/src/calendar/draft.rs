//! The event editor's model (C3 spec, section 5): a draft made from a new
//! slot, another feature's prefill or a stored event; what the user may
//! change in it and why not; what is wrong with it; and saving or deleting
//! through the op queue.
//!
//! Until invitations exist (U5a), a meeting with guests is read-only here,
//! as the op queue refuses to change one. A change to one occurrence of a
//! series is saved for This event, This and following events or All events,
//! which the user picks from `SavePlan::scopes`.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;

use super::change::{self, CancelDialog, Field, UpdateDialog};
use super::ops::{self, CalendarOpId, Change, Edit, Extra, Scope};
use super::series::{self, Horizon};
use super::{
    Alarm, AlarmTrigger, Attendee, Bounds, Calendar, CalendarId, CalendarSource, EventData, EventId, Local, Meeting, MeetingOptions, MeetingProvider,
    Moment, Person, Recurrence, Response, Role, ShowAs, Span, Visibility, Zone, caps, description, detail, merge, organizer, repeat, split, tz,
};
use crate::{AccountId, Error, Result, store};

/// A part of the editor, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DraftField {
    Title,
    Time,
    Repeat,
    People,
    /// The organizer options under People.
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

const EVERY_FIELD: [DraftField; 13] = [
    DraftField::Title,
    DraftField::Time,
    DraftField::Repeat,
    DraftField::People,
    DraftField::Options,
    DraftField::Room,
    DraftField::Location,
    DraftField::Video,
    DraftField::Description,
    DraftField::Calendar,
    DraftField::ShowAs,
    DraftField::Reminder,
    DraftField::Visibility,
];

/// The reminder the user sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reminder {
    /// A new event left on the calendar's own default, which the provider
    /// applies: Jolta sends no reminder.
    Default,
    Off,
    /// Minutes before the start.
    Before(i64),
}

/// The event as the editor holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct EventDraft {
    pub calendar: CalendarId,
    /// The stored event the editor opened; None for a new one. Saving
    /// changes only the fields that differ from it, so a change made
    /// elsewhere meanwhile to another field stays.
    pub base: Option<EventData>,
    pub uid: String,
    pub title: String,
    pub span: Span,
    /// Everyone on the event but rooms, with their answers.
    pub people: Vec<Attendee>,
    /// What the organizer lets the guests do.
    pub options: MeetingOptions,
    /// One room's address.
    pub room: String,
    pub location: String,
    /// The user's own text: a Teams, Meet or Zoom block stays out of it and
    /// is written back after it unchanged.
    pub description: String,
    /// A video call to make on save.
    pub video: Option<MeetingProvider>,
    pub show_as: ShowAs,
    pub reminder: Reminder,
    pub visibility: Visibility,
    pub locks: Vec<DraftField>,
    /// Why fields are locked, said once at the top of the panel.
    pub notice: Option<String>,
    /// Shown, never saved: the organizer, the meeting already on the
    /// event, the repeat rule in words and a note on the description.
    pub organizer: Option<Person>,
    pub meeting: Option<Meeting>,
    pub repeat: Option<String>,
    /// How the series repeats, as the user set it: for an occurrence, its
    /// series' rule. None for a single event.
    pub rule: Option<Recurrence>,
    /// `rule` as the editor opened it.
    pub rule_was: Option<Recurrence>,
    /// "Formatting from Outlook is removed when you change this text."
    pub description_note: Option<String>,
    /// "4 accepted, 1 maybe, 2 no answer"; empty without guests.
    pub responses: String,
    /// Under Show as on a meeting the user organizes on Microsoft 365, which
    /// sends the organizer's show as with the next update (C3 spec,
    /// section 3): "Your guests see this with the next update."
    pub show_as_note: Option<String>,
}

/// What another feature fills in when it opens the editor: C4's Invite
/// from Find a time and Book a hold, C5's Schedule meeting.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventPrefill {
    /// The default calendar when None.
    pub calendar: Option<CalendarId>,
    pub title: String,
    /// The next half hour when None.
    pub span: Option<Span>,
    pub required: Vec<Person>,
    pub optional: Vec<Person>,
    pub room: Option<String>,
    pub description: String,
    pub video: Option<MeetingProvider>,
}

/// Something wrong with a draft, said where the user fixes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// None for one about the whole event, shown above the buttons.
    pub field: Option<DraftField>,
    pub text: String,
    /// Save waits until it is fixed; otherwise a warning.
    pub blocking: bool,
}

/// What saving needs before it can go ahead: the scope prompt, then the
/// Send an update? dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavePlan {
    /// The draft says what the stored event says: saving only closes.
    pub unchanged: bool,
    /// For a change to one occurrence of a series, the choices of the
    /// scope prompt in order, the first the default; empty otherwise.
    pub scopes: Vec<ScopeChoice>,
    /// The prompt's line: "You moved Standup to 10:00."
    pub prompt: Option<String>,
    /// Send an update?, for a change the guests of a meeting the user
    /// organizes would hear about. Saving then takes one of its choices.
    pub dialog: Option<UpdateDialog>,
    /// Send invitations from: the accounts to pick from first, when the
    /// meeting's calendar has no account to mail its guests from.
    pub senders: Vec<Sender>,
}

/// An account the user can pick to send a calendar's invitations from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sender {
    pub account: AccountId,
    pub address: String,
    pub name: String,
}

/// One row of the scope prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeChoice {
    pub scope: Scope,
    /// "This and following events".
    pub label: String,
    /// What it costs: "Starts a new series from Tue 24 Nov."
    pub detail: Option<String>,
}

/// A save or delete queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The Undo token; None when nothing changed.
    pub op: Option<CalendarOpId>,
    pub calendar: CalendarId,
    /// The UID the saved event has now: a new one for This and following.
    pub uid: String,
    /// The toast's words: "Saved Standup".
    pub message: String,
}

/// Whether and how an event can be deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletePlan {
    /// Why it can't be, if it can't.
    pub refusal: Option<String>,
    pub title: String,
    /// For one occurrence of a series, the scope prompt's choices.
    pub scopes: Vec<ScopeChoice>,
    /// The prompt's line: "You're deleting Standup on Tue 24 Nov."
    pub prompt: Option<String>,
    /// For a meeting the user organizes, the cancel dialog for each scope
    /// (one for an event that doesn't repeat), in the order of `scopes`.
    pub cancel: Vec<(Scope, CancelDialog)>,
    /// For a meeting someone else organizes: Decline and remove? goes in
    /// place of a delete.
    pub decline: Option<Decline>,
    /// Send invitations from, asked before the cancellation can go.
    pub senders: Vec<Sender>,
}

/// "Decline and remove?" for a meeting someone else organizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decline {
    pub title: String,
    /// "Jan Peeters gets your decline, and the meeting leaves your calendar."
    pub body: String,
    /// The organizer, for "Tell Jan Peeters", ticked by default.
    pub organizer: String,
}

/// Minutes from `now` to the next half hour, for a new event without a slot.
pub fn next_half_hour(now: DateTime<Utc>, here: Tz) -> NaiveDateTime {
    let local = now.with_timezone(&here).naive_local();
    let past = i64::from(local.minute() % 30) * 60 + i64::from(local.second());
    (local + Duration::seconds(30 * 60 - past)).with_nanosecond(0).unwrap_or(local)
}

/// A timed span from unix seconds, held in `zone`.
pub fn timed_span(start: i64, end: i64, zone: Zone, here: Tz) -> Span {
    let local = |t: i64| Local { wall: tz::wall(DateTime::from_timestamp(t, 0).unwrap_or_default(), zone, here), zone };
    Span::Timed { start: local(start), end: local(end) }
}

/// A draft for a new event on `span` (the next half hour for the default
/// length without one) in `calendar`, or the default calendar.
pub fn draft_for_new(conn: &Connection, calendar: Option<CalendarId>, span: Option<Span>, now: DateTime<Utc>, here: Tz) -> Result<EventDraft> {
    let calendar = match calendar {
        Some(id) => store::calendar(conn, id)?,
        None => default_calendar(conn)?.ok_or_else(|| Error::Other("No calendar takes new events. Make one in Settings › Calendar.".into()))?,
    };
    let span = match span {
        Some(span) => span,
        None => {
            let start = next_half_hour(now, here);
            let end = start + Duration::minutes(store::default_event_minutes(conn)?);
            let zone = Zone::Tz(here);
            Span::Timed { start: Local { wall: start, zone }, end: Local { wall: end, zone } }
        }
    };
    Ok(EventDraft {
        calendar: calendar.id,
        base: None,
        uid: ops::new_uid()?,
        title: String::new(),
        span,
        people: Vec::new(),
        options: MeetingOptions::default(),
        room: String::new(),
        location: String::new(),
        description: String::new(),
        video: None,
        show_as: ShowAs::Busy,
        reminder: calendar.default_reminder.map_or(Reminder::Default, Reminder::Before),
        visibility: Visibility::Default,
        locks: Vec::new(),
        notice: None,
        organizer: None,
        meeting: None,
        repeat: None,
        rule: None,
        rule_was: None,
        description_note: None,
        responses: String::new(),
        show_as_note: None,
    })
}

/// A new event filled in by another feature.
pub fn draft_from_prefill(conn: &Connection, prefill: &EventPrefill, now: DateTime<Utc>, here: Tz) -> Result<EventDraft> {
    let mut draft = draft_for_new(conn, prefill.calendar, prefill.span, now, here)?;
    let person = |p: &Person, role| Attendee { person: p.clone(), role, response: Response::NeedsAction, comment: String::new(), is_me: false };
    draft.title.clone_from(&prefill.title);
    draft.people = prefill.required.iter().map(|p| person(p, Role::Required)).chain(prefill.optional.iter().map(|p| person(p, Role::Optional))).collect();
    draft.room = prefill.room.clone().unwrap_or_default();
    draft.description.clone_from(&prefill.description);
    draft.video = prefill.video;
    Ok(draft)
}

/// A draft of a stored event, at the occurrence `at` of a series, with what
/// the user may not change and why.
pub fn draft_for_event(conn: &Connection, event: EventId, at: Option<Bounds>, here: Tz) -> Result<EventDraft> {
    let d = detail::open(conn, event, at, here, "")?;
    let account = store::calendar_address(conn, &d.calendar)?.unwrap_or_default();
    let rule = match &d.event.recurrence_id {
        None => d.event.recurrence.clone(),
        Some(_) => ops::stored(conn, d.calendar.id, &d.event.uid, None)?.and_then(|m| m.event.recurrence),
    };
    let series_span = match &d.event.recurrence_id {
        None => d.event.span,
        Some(_) => ops::stored(conn, d.calendar.id, &d.event.uid, None)?.map_or(d.event.span, |m| m.event.span),
    };
    let (locks, notice) = access(&d.calendar, &account, &d.event, rule.as_ref(), &series_span, here);
    let conflict = ops::conflict(conn, d.calendar.id, &d.event.uid)?.map(|c| c.message);
    // An unchanged occurrence of a series is the master moved to its time:
    // the version the user starts from, as the occurrence the view shows.
    let base = match at.filter(|_| d.event.recurrence.is_some() && d.event.recurrence_id.is_none()) {
        Some(at) => series::occurrence(std::slice::from_ref(&d.event), &occurrence_span(&d.event.span, at, here).start(), here).unwrap_or(d.event),
        None => d.event,
    };
    let mut draft = opened(&base, d.calendar.id);
    let organizes = other_organizer(&d.calendar, &account, &base).is_none() && !guests(&d.calendar, &account, &base).is_empty();
    draft.show_as_note = (organizes && d.calendar.source == CalendarSource::Graph).then(|| "Your guests see this with the next update.".into());
    draft.responses = d.responses;
    draft.locks = locks;
    draft.notice = notice.or(conflict);
    draft.repeat = d.repeat;
    draft.rule.clone_from(&rule);
    draft.rule_was = rule;
    Ok(draft)
}

/// A new event like the occurrence `at` of `event`, at its time: ⌘D and
/// ⌥-drag. It keeps the title, the place, the description and the user's
/// own settings, and leaves out the people, the repeat and the meeting: a
/// copy never carries the original's call, since Google warns against
/// reusing conference data and a Teams link belongs to its meeting. It goes
/// in the same calendar when the user can write there, else the default one.
pub fn draft_for_copy(conn: &Connection, event: EventId, at: Option<Bounds>, now: DateTime<Utc>, here: Tz) -> Result<EventDraft> {
    let source = draft_for_event(conn, event, at, here)?;
    let writable = caps::of(&store::calendar(conn, source.calendar)?).writable;
    let mut copy = draft_for_new(conn, writable.then_some(source.calendar), Some(source.span), now, here)?;
    let (location, description) = match &source.meeting {
        Some(m) => (
            source.location.replace(&m.join_url, "").trim_matches(|c: char| c.is_whitespace() || ",;·|-".contains(c)).to_owned(),
            without_meeting(&source.description, m),
        ),
        None => (source.location, source.description),
    };
    copy.title = source.title;
    copy.location = location;
    copy.room = source.room;
    copy.description = description;
    copy.show_as = source.show_as;
    copy.visibility = source.visibility;
    if let Reminder::Before(_) = source.reminder {
        copy.reminder = source.reminder;
    }
    Ok(copy)
}

/// `text` without its lines that name `meeting`: the link, the ID, the
/// passcode or a dial-in number, and with the blank lines that leaves
/// folded into one.
fn without_meeting(text: &str, meeting: &Meeting) -> String {
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let numbers: Vec<String> = meeting
        .meeting_id
        .iter()
        .map(|id| digits(id))
        .chain(meeting.phones.iter().map(|p| digits(p.split(',').next().unwrap_or(p))))
        .filter(|n| n.len() >= 6)
        .collect();
    let names = |line: &str| {
        line.contains(&meeting.join_url)
            || meeting.meeting_id.as_deref().is_some_and(|id| line.contains(id))
            || meeting.passcode.as_deref().is_some_and(|code| line.contains(code))
            || numbers.iter().any(|n| digits(line).contains(n.as_str()))
    };
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines().filter(|l| !names(l)) {
        if line.trim().is_empty() && out.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_owned()
}

/// The draft fields of a stored event, nothing locked.
fn opened(e: &EventData, calendar: CalendarId) -> EventDraft {
    let parts = description::split(&e.description, e.description_html);
    let room = room_of(e).map(|a| a.person.address.clone()).unwrap_or_default();
    EventDraft {
        calendar,
        base: Some(e.clone()),
        uid: e.uid.clone(),
        title: e.title.clone(),
        span: e.span,
        people: e.attendees.iter().filter(|a| !is_room(a)).cloned().collect(),
        options: e.options,
        // A location that only repeats the room, as saving a room puts it
        // there, isn't one of its own.
        location: if mirrors_room(&e.location, &room) { String::new() } else { e.location.clone() },
        room,
        description: parts.editable.clone(),
        video: None,
        show_as: e.show_as,
        reminder: reminder_of(&e.alarms).map_or(Reminder::Off, Reminder::Before),
        visibility: e.visibility,
        locks: Vec::new(),
        notice: None,
        organizer: e.organizer.clone(),
        meeting: e.meeting.clone(),
        repeat: None,
        rule: e.recurrence.clone(),
        rule_was: e.recurrence.clone(),
        description_note: parts.loses_formatting().then(|| "Formatting from Outlook is removed when you change this text.".into()),
        responses: String::new(),
        show_as_note: None,
    }
}

/// An occurrence's span in the series' own form: a timed series keeps its
/// zone, an all-day one its days.
fn occurrence_span(series: &Span, at: Bounds, here: Tz) -> Span {
    match (series, at) {
        (Span::Timed { start, .. }, Bounds::Utc { start: s, end: e }) => timed_span(s.timestamp(), e.timestamp(), start.zone, here),
        (_, Bounds::Days { start, end }) => {
            let day = |n: i32| NaiveDate::default() + Duration::days(n.into());
            Span::AllDay { start: day(start), end: day(end) }
        }
        (Span::AllDay { .. }, Bounds::Utc { .. }) => *series,
    }
}

/// What the user may not change in `event` of `calendar`, and the one
/// sentence that says why (the read-only table in section 5). `account` is
/// the address of the calendar's account. In a meeting someone else
/// organizes only the user's own settings stay open.
/// `rule` is the series' rule, for a series or one of its occurrences, and
/// `series_span` the series' own time.
pub fn access(calendar: &Calendar, account: &str, event: &EventData, rule: Option<&Recurrence>, series_span: &Span, here: Tz) -> (Vec<DraftField>, Option<String>) {
    if !caps::of(calendar).writable {
        return (EVERY_FIELD.to_vec(), Some("This calendar is read-only.".into()));
    }
    if let Some(o) = other_organizer(calendar, account, event) {
        return (MEETING_FIELDS.to_vec(), Some(format!("{} organizes this meeting. You can change only your own settings.", change::name(o))));
    }
    if let Some(rule) = rule
        && calendar.source == CalendarSource::Graph
        && super::sources::graph::pattern(rule, series_span, here).is_none()
    {
        return (vec![DraftField::Repeat], Some("Microsoft 365 can't change this repeat rule.".into()));
    }
    (Vec::new(), None)
}

/// What only a meeting's organizer changes; the user's own settings stay.
const MEETING_FIELDS: [DraftField; 10] = [
    DraftField::Title,
    DraftField::Time,
    DraftField::Repeat,
    DraftField::People,
    DraftField::Options,
    DraftField::Room,
    DraftField::Location,
    DraftField::Video,
    DraftField::Description,
    DraftField::Calendar,
];

/// The user's addresses on `event`: the account's, and in a calendar the
/// account owns (`Calendar::is_own`) the attendees and organizer it marks
/// as the user.
/// Google marks the owner of a calendar shared with the account the same
/// way, and that owner isn't the user.
pub fn mine(calendar: &Calendar, account: &str, event: &EventData) -> Vec<String> {
    let own = calendar.is_own(account);
    let marked = event.attendees.iter().filter(|a| own && a.is_me).map(|a| a.person.address.clone());
    let organizer = event.organizer.as_ref().filter(|_| own && event.organizer_is_me).map(|o| o.address.clone());
    (!account.is_empty()).then(|| account.to_string()).into_iter().chain(marked).chain(organizer).collect()
}

/// The organizer of `event` when it is a meeting with guests that someone
/// other than the user organizes: the user may change only their own
/// settings, and its dialogs are a guest's (check 1).
pub fn other_organizer<'a>(calendar: &Calendar, account: &str, event: &'a EventData) -> Option<&'a Person> {
    let me = mine(calendar, account, event);
    let is_me = |address: &str| me.iter().any(|m| m.eq_ignore_ascii_case(address.trim()));
    // Anyone on it but the organizer makes it a meeting, the user alone
    // too: Graph and iCloud list the organizer of an invitation among no
    // attendees. An event without attendees keeps the organizer Google
    // gives any event of a secondary calendar, and the owner of a shared
    // calendar on their own event is its organizer.
    event.organizer.as_ref().filter(|o| {
        let meeting = event.attendees.iter().any(|a| !is_room(a) && !a.person.address.eq_ignore_ascii_case(&o.address));
        meeting && !is_me(&o.address)
    })
}

/// Everyone on `event` the server mails about a change: guests and rooms,
/// never the user.
pub fn guests<'a>(calendar: &Calendar, account: &str, event: &'a EventData) -> Vec<&'a Attendee> {
    let me = mine(calendar, account, event);
    event.attendees.iter().filter(|a| !a.is_me && !me.iter().any(|m| m.eq_ignore_ascii_case(&a.person.address))).collect()
}

fn mirrors_room(location: &str, room: &str) -> bool {
    !room.is_empty() && location.trim().eq_ignore_ascii_case(room)
}

fn is_room(a: &Attendee) -> bool {
    matches!(a.role, Role::Room | Role::Resource)
}

fn room_of(e: &EventData) -> Option<&Attendee> {
    e.attendees.iter().find(|a| a.role == Role::Room)
}

/// The first alert that pops up some minutes before the start.
fn reminder_of(alarms: &[Alarm]) -> Option<i64> {
    alarms.iter().find_map(|a| match a.trigger {
        AlarmTrigger::MinutesBefore(m) if !a.email && m >= 0 => Some(m),
        _ => None,
    })
}

/// The fields of `draft` that differ from the stored event it opened.
fn changed(draft: &EventDraft, base: &EventData) -> Vec<DraftField> {
    let was = opened(base, draft.calendar);
    let people = |list: &[Attendee]| list.iter().map(|a| (a.person.address.to_lowercase(), a.role)).collect::<Vec<_>>();
    EVERY_FIELD
        .into_iter()
        .filter(|f| match f {
            DraftField::Title => draft.title != was.title,
            DraftField::Time => draft.span != was.span,
            DraftField::Repeat => draft.rule != draft.rule_was,
            DraftField::People => people(&draft.people) != people(&was.people),
            DraftField::Options => draft.options != was.options,
            DraftField::Room => !draft.room.trim().eq_ignore_ascii_case(&was.room),
            DraftField::Location => draft.location != was.location,
            DraftField::Video => draft.video.is_some(),
            DraftField::Description => draft.description.trim_end() != was.description,
            DraftField::Calendar => false,
            DraftField::ShowAs => draft.show_as != was.show_as,
            DraftField::Reminder => draft.reminder != was.reminder,
            DraftField::Visibility => draft.visibility != was.visibility,
        })
        .collect()
}

/// The event the draft saves: the stored one with the fields the user
/// changed, or a new one.
fn event_of(draft: &EventDraft) -> EventData {
    let (mut e, fields) = match &draft.base {
        Some(base) => (base.clone(), changed(draft, base)),
        None => (EventData::new(draft.uid.clone(), draft.span), EVERY_FIELD.to_vec()),
    };
    for &field in &fields {
        match field {
            DraftField::Title => e.title = draft.title.trim().to_string(),
            DraftField::Time => e.span = draft.span,
            DraftField::People | DraftField::Room => {
                let room = draft.room.trim();
                let kept_room = room_of(&e).filter(|a| a.person.address.eq_ignore_ascii_case(room)).cloned();
                let others = e.attendees.iter().filter(|a| a.role == Role::Resource).cloned();
                let mut attendees: Vec<Attendee> = draft.people.clone();
                attendees.extend(others);
                if !room.is_empty() {
                    attendees.push(kept_room.unwrap_or_else(|| Attendee {
                        person: Person { address: room.to_string(), name: String::new() },
                        role: Role::Room,
                        response: Response::NeedsAction,
                        comment: String::new(),
                        is_me: false,
                    }));
                }
                e.attendees = attendees;
            }
            DraftField::Options => e.options = draft.options,
            DraftField::Location => {}
            DraftField::Description => {
                let base = draft.base.as_ref().map(|b| description::split(&b.description, b.description_html));
                let (text, html) = match base {
                    Some(parts) => parts.join(&draft.description),
                    None => (draft.description.trim_end().to_string(), false),
                };
                e.description = text;
                e.description_html = html;
            }
            DraftField::ShowAs => e.show_as = draft.show_as,
            DraftField::Reminder => {
                let at = e.alarms.iter().position(|a| !a.email && matches!(a.trigger, AlarmTrigger::MinutesBefore(m) if m >= 0));
                let alarm = |m| Alarm { trigger: AlarmTrigger::MinutesBefore(m), email: false };
                match (draft.reminder, at) {
                    (Reminder::Before(m), Some(i)) => e.alarms[i] = alarm(m),
                    (Reminder::Before(m), None) => e.alarms.insert(0, alarm(m)),
                    (Reminder::Off | Reminder::Default, Some(i)) => {
                        e.alarms.remove(i);
                    }
                    (Reminder::Off | Reminder::Default, None) => {}
                }
            }
            DraftField::Visibility => e.visibility = draft.visibility,
            // An occurrence's series repeats by the rule; This and following
            // and All events take it from the draft.
            DraftField::Repeat if e.recurrence_id.is_none() => e.recurrence.clone_from(&draft.rule),
            DraftField::Repeat | DraftField::Video | DraftField::Calendar => {}
        }
    }
    // The location is the user's, or else the room's address (C4 spec,
    // "C3-room-field"), which follows the room while the user has none.
    if fields.contains(&DraftField::Location) || fields.contains(&DraftField::Room) {
        let own = draft.location.trim();
        e.location = if own.is_empty() { draft.room.trim().to_string() } else { own.to_string() };
    }
    e
}

/// Everything wrong with the draft for its calendar, run on every change
/// and again at save.
pub fn check_draft(conn: &Connection, draft: &EventDraft, here: Tz) -> Result<Vec<Problem>> {
    let calendar = store::calendar(conn, draft.calendar)?;
    let caps = caps::of(&calendar);
    let mut out = Vec::new();
    let mut problem = |field: Option<DraftField>, text: String, blocking: bool| out.push(Problem { field, text, blocking });
    let stored = draft.base.is_some();
    let touched = draft.base.as_ref().map_or(EVERY_FIELD.to_vec(), |b| changed(draft, b));
    if !caps.writable {
        problem(Some(DraftField::Calendar), format!("{} is read-only.", calendar.name), true);
    }
    let (locks, notice) = locks_now(conn, draft, &calendar)?;
    for f in &touched {
        if locks.contains(f) {
            problem(Some(*f), notice.clone().unwrap_or_else(|| "This can't change here.".into()), true);
        }
    }
    match draft.span {
        Span::Timed { start, end } => {
            if end.instant(here) <= start.instant(here) {
                problem(Some(DraftField::Time), "The end is before the start.".into(), true);
            }
            for t in [start, end] {
                if let Some(text) = skipped(t, here) {
                    problem(Some(DraftField::Time), text, false);
                }
            }
        }
        Span::AllDay { start, end } => {
            if end <= start {
                problem(Some(DraftField::Time), "The last day is before the first.".into(), true);
            }
        }
    }
    let room = draft.room.trim();
    if !room.is_empty() {
        if !is_address(room) {
            problem(Some(DraftField::Room), format!("{room} isn't an email address. Type the room's address."), true);
        }
    }
    let new_people = touched.contains(&DraftField::People) || !stored;
    if new_people && !draft.people.is_empty() {
        let this = account_of(conn, &calendar)?;
        let own: Vec<String> = store::accounts(conn)?.into_iter().map(|a| a.address).filter(|a| Some(a) != this.as_ref()).collect();
        for p in &draft.people {
            if !is_address(&p.person.address) {
                problem(Some(DraftField::People), format!("{} isn't an email address.", p.person.address), true);
            } else if own.iter().any(|a| a.eq_ignore_ascii_case(&p.person.address)) {
                problem(Some(DraftField::People), format!("{} is one of your own accounts.", p.person.address), false);
            }
        }
        if !caps.invite {
            problem(Some(DraftField::People), format!("{} can't have guests.", calendar.name), true);
        } else if let Some(max) = caps.max_people.filter(|m| draft.people.len() > *m) {
            problem(Some(DraftField::People), format!("{} takes at most {max} people.", provider(&calendar)), true);
        }
    }
    if let Some(video) = draft.video {
        let text = match caps.video.contains(&video) {
            true => "Jolta can't make video calls yet. Remove it to save.".to_string(),
            false => format!("{} calendars can't make this video call.", provider(&calendar)),
        };
        problem(Some(DraftField::Video), text, true);
    }
    if !caps.show_as.is_empty() && !caps.show_as.contains(&draft.show_as) && touched.contains(&DraftField::ShowAs) {
        problem(Some(DraftField::ShowAs), format!("{} calendars can't show an event this way.", provider(&calendar)), true);
    }
    if touched.contains(&DraftField::Repeat)
        && let Some(rule) = &draft.rule
    {
        let start = series_of(conn, draft)?.map_or(draft.span, |m| m.span);
        if let Some(text) = repeat::refusal(rule, &start, &calendar, here) {
            problem(Some(DraftField::Repeat), format!("{text}."), true);
        }
    }
    if touched.contains(&DraftField::Time)
        && calendar.source == CalendarSource::Graph
        && let Some(text) = crossing(conn, draft, here)?
    {
        problem(Some(DraftField::Time), text, true);
    }
    if out.iter().all(|p| !p.blocking) && !touched.is_empty() {
        let scope = if draft.base.as_ref().is_some_and(|b| b.recurrence_id.is_some()) && !touched.contains(&DraftField::Repeat) { Scope::This } else { Scope::All };
        let refusal = match edit(conn, draft, &calendar, scope).and_then(|(edit, _)| ops::refusal(conn, &edit)) {
            Ok(refusal) => refusal,
            Err(e) => Some(e.to_string()),
        };
        if let Some(text) = refusal {
            out.push(Problem { field: None, text, blocking: true });
        }
    }
    Ok(out)
}

/// What the stored event the draft opened may not change now: the store's
/// word, never the locks a caller hands back.
fn locks_now(conn: &Connection, draft: &EventDraft, calendar: &Calendar) -> Result<(Vec<DraftField>, Option<String>)> {
    let Some(base) = &draft.base else { return Ok((Vec::new(), None)) };
    let stored = match ops::stored(conn, draft.calendar, &base.uid, base.recurrence_id.as_ref())? {
        Some(s) => Some(s),
        None => ops::stored(conn, draft.calendar, &base.uid, None)?,
    };
    let Some(stored) = stored else {
        return Ok((EVERY_FIELD.to_vec(), Some("It was deleted while you edited it.".into())));
    };
    let account = account_of(conn, calendar)?.unwrap_or_default();
    let master = ops::stored(conn, draft.calendar, &base.uid, None)?.map(|m| m.event);
    let rule = master.as_ref().and_then(|m| m.recurrence.clone());
    let span = master.map_or(stored.event.span, |m| m.span);
    Ok(access(calendar, &account, &stored.event, rule.as_ref(), &span, tz::here()))
}

/// The address that is the user in a calendar: its account's, or the one a
/// calendar on this Mac sends invitations from.
fn account_of(conn: &Connection, calendar: &Calendar) -> Result<Option<String>> {
    store::calendar_address(conn, calendar)
}

/// The accounts to pick from when saving or deleting `event` would mail its
/// guests from a calendar that has no account to send from yet: Send
/// invitations from, asked once per calendar. Empty otherwise.
fn senders_needed(conn: &Connection, calendar: &Calendar, event: &EventData) -> Result<Vec<Sender>> {
    let account = account_of(conn, calendar)?.unwrap_or_default();
    if !organizer::sends_itself(calendar) || guests(calendar, &account, event).is_empty() || organizer::sender(conn, calendar, event)?.is_some() {
        return Ok(Vec::new());
    }
    Ok(organizer::senders(conn)?.into_iter().map(|a| Sender { account: a.id, address: a.address, name: a.display_name }).collect())
}

/// Who a calendar's mail comes from, for the dialogs: the provider, or for a
/// calendar Jolta sends from itself, the address it sends from.
fn mailer(conn: &Connection, calendar: &Calendar, event: &EventData) -> Result<(String, Option<String>)> {
    if !organizer::sends_itself(calendar) {
        return Ok((caps::mailer(calendar).to_string(), None));
    }
    let from = organizer::sender(conn, calendar, event)?.map(|a| a.address);
    Ok((from.clone().unwrap_or_else(|| "Jolta".into()), from))
}

/// "02:30 doesn't exist on 29 March; the event starts at 03:30".
fn skipped(t: Local, here: Tz) -> Option<String> {
    let zone = t.zone.or(here);
    if !matches!(zone.from_local_datetime(&t.wall), chrono::LocalResult::None) {
        return None;
    }
    let becomes = tz::wall(t.instant(here), t.zone, here);
    Some(format!("{} doesn't exist on {}; it becomes {}.", t.wall.format("%H:%M"), t.wall.format("%-d %B"), becomes.format("%H:%M")))
}

/// A plain address check: one @, a dot in the domain, no spaces.
fn is_address(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else { return false };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !s.contains(char::is_whitespace) && !domain.contains('@')
}

pub(crate) fn provider(calendar: &Calendar) -> &'static str {
    match calendar.source {
        CalendarSource::Google => "Google",
        CalendarSource::Graph => "Microsoft 365",
        CalendarSource::CalDav => "iCloud and CalDAV",
        CalendarSource::Local => "this Mac's",
        CalendarSource::Webcal => "subscribed",
        CalendarSource::EventKit => "macOS",
    }
}

/// The edit saving the draft queues, with the fields a change made
/// elsewhere kept: the user's changes merged into the stored event when it
/// changed since the editor opened it.
fn edit(conn: &Connection, draft: &EventDraft, calendar: &Calendar, scope: Scope) -> Result<(Edit, Vec<Field>)> {
    let mine = match scope {
        Scope::All if draft.base.as_ref().is_some_and(|b| b.recurrence_id.is_some()) => {
            let gone = || Error::Other(format!("{} was deleted while you edited it.", draft.title));
            let base = draft.base.as_ref().ok_or_else(gone)?;
            let series = series_of(conn, draft)?.ok_or_else(gone)?;
            // A field changed elsewhere since the editor opened keeps the
            // server's value, as for one event.
            let now = stored_now(conn, draft, base)?.ok_or_else(gone)?;
            let mut lost = if now == *base { Vec::new() } else { merge::three_way(base, &event_of(draft), &now).lost };
            // An occurrence has no rule of its own: the series' says whether
            // the rule changed elsewhere too.
            if draft.rule != draft.rule_was && series.recurrence != draft.rule_was && series.recurrence != draft.rule {
                lost.push(Field::Repeat);
            }
            let kept = reset(draft, base, &lost);
            return Ok((Edit::quiet(draft.calendar, Change::Update(whole_series(&kept, &series, tz::here())?)), lost));
        }
        _ => event_of(draft),
    };
    let (change, lost) = match &draft.base {
        None => (Change::Create(vec![mine]), Vec::new()),
        Some(base) => {
            let title = if mine.title.is_empty() { "(no title)" } else { &mine.title };
            let now_stored = stored_now(conn, draft, base)?.ok_or_else(|| Error::Other(format!("{title} was deleted while you edited it.")))?;
            if now_stored == *base {
                (Change::Update(mine), Vec::new())
            } else {
                let merged = merge::three_way(base, &mine, &now_stored);
                (Change::Update(merged.event), merged.lost)
            }
        }
    };
    let extra = match (&change, draft.reminder) {
        (Change::Create(_), Reminder::Off) => no_reminder(calendar.source),
        _ => None,
    };
    Ok((Edit { extra, ..Edit::quiet(draft.calendar, change) }, lost))
}

/// The event the draft opened as the store has it now: its row, or for an
/// occurrence without one, the series moved there while it still has it.
fn stored_now(conn: &Connection, draft: &EventDraft, base: &EventData) -> Result<Option<EventData>> {
    if let Some(row) = ops::stored(conn, draft.calendar, &base.uid, base.recurrence_id.as_ref())? {
        return Ok(Some(row.event));
    }
    let (Some(rid), Some(series)) = (base.recurrence_id, series_of(conn, draft)?) else { return Ok(None) };
    let here = tz::here();
    Ok(ops::is_occurrence(&series, &rid, here).then(|| series::occurrence(std::slice::from_ref(&series), &rid, here)).flatten())
}

/// A new event without alerts carries none to Google and Microsoft 365,
/// which would otherwise give it the calendar's default.
fn no_reminder(source: CalendarSource) -> Option<Extra> {
    let fields = match source {
        CalendarSource::Google => serde_json::json!({ "reminders": { "useDefault": false, "overrides": [] } }),
        CalendarSource::Graph => serde_json::json!({ "isReminderOn": false }),
        _ => return None,
    };
    fields.as_object().cloned().map(Extra::Json)
}

/// What saving the draft needs first: for a change to one occurrence of a
/// series, the scope prompt, its choices saying what each costs here.
pub fn plan_save(conn: &Connection, draft: &EventDraft, here: Tz) -> Result<SavePlan> {
    let touched = draft.base.as_ref().map_or_else(Vec::new, |b| changed(draft, b));
    let unchanged = draft.base.is_some() && touched.is_empty();
    let occurrence = draft.base.as_ref().and_then(|b| b.recurrence_id);
    let dialog = if unchanged { None } else { send_dialog(conn, draft)? };
    let calendar = store::calendar(conn, draft.calendar)?;
    let senders = if dialog.is_some() { senders_needed(conn, &calendar, &event_of(draft))? } else { Vec::new() };
    let (Some(at), false) = (occurrence, unchanged) else { return Ok(SavePlan { unchanged, scopes: Vec::new(), prompt: None, dialog, senders }) };
    let series = series_of(conn, draft)?.ok_or_else(|| Error::Other(format!("{} was deleted while you edited it.", draft.title)))?;
    let mut scopes = Vec::new();
    if !touched.contains(&DraftField::Repeat) {
        scopes.push(ScopeChoice { scope: Scope::This, label: "This event".into(), detail: None });
    }
    if caps::of(&calendar).split_series && !series.span.start().names(&at, here) {
        let day = match draft.span.start() {
            Moment::Date(d) => d,
            Moment::Time(t) => t.wall.date(),
        };
        let guests = if draft.people.iter().any(|p| !p.is_me) { " Guests answer again." } else { "" };
        scopes.push(ScopeChoice { scope: Scope::Following, label: "This and following events".into(), detail: Some(format!("Starts a new series from {}.{guests}", day.format("%a %-d %b"))) });
    }
    let whole = whole_series(draft, &series, here)?;
    let lost = ops::dropped_count(conn, &calendar, &series, &whole, here)?;
    let detail = match lost {
        0 => None,
        1 => Some("Removes the changes made to 1 single event.".into()),
        n => Some(format!("Removes the changes made to {n} single events.")),
    };
    scopes.push(ScopeChoice { scope: Scope::All, label: "All events".into(), detail });
    let title = if draft.title.trim().is_empty() { "(no title)" } else { draft.title.trim() };
    let prompt = match (touched.as_slice(), draft.span.start()) {
        ([DraftField::Time], Moment::Time(t)) => format!("You moved {title} to {}.", t.wall.format("%H:%M")),
        _ => format!("You changed {title}."),
    };
    Ok(SavePlan { unchanged, scopes, prompt: Some(prompt), dialog, senders })
}

/// The calendar's account address, and what the change the draft makes is.
fn classified(conn: &Connection, draft: &EventDraft) -> Result<(Calendar, String, change::Classified)> {
    let calendar = store::calendar(conn, draft.calendar)?;
    let account = account_of(conn, &calendar)?.unwrap_or_default();
    let after = event_of(draft);
    let before = draft.base.clone().unwrap_or_else(|| EventData::new(draft.uid.clone(), draft.span));
    let mut me = mine(&calendar, &account, &before);
    me.extend(mine(&calendar, &account, &after));
    let mut c = change::classify(&before, &after, &me);
    // An occurrence's draft holds its series' rule beside it: a new rule
    // is a change to the meeting too.
    if draft.rule != draft.rule_was && !c.meeting.contains(&Field::Repeat) {
        c.meeting.push(Field::Repeat);
    }
    Ok((calendar, account, c))
}

/// Send an update?, when saving the draft emails people: never for a
/// meeting someone else organizes (check 1).
fn send_dialog(conn: &Connection, draft: &EventDraft) -> Result<Option<UpdateDialog>> {
    let (calendar, account, c) = classified(conn, draft)?;
    if draft.base.as_ref().is_some_and(|b| other_organizer(&calendar, &account, b).is_some()) {
        return Ok(None);
    }
    let (mailer, from) = mailer(conn, &calendar, &event_of(draft))?;
    Ok(change::dialog(&c, &caps::of(&calendar), &mailer, from.as_deref(), draft.base.is_none()))
}

/// The toast's words for a save that mails people: "Update sent to 6
/// people", said as the update waits the undo window.
fn sent_line(c: &change::Classified, notify: ops::Notify, creating: bool, mailer: &str) -> String {
    let people = |n: usize| if n == 1 { "1 person".to_string() } else { format!("{n} people") };
    let (what, n) = match (creating, notify) {
        (true, _) => ("Invitation", c.added.len()),
        (false, ops::Notify::Changed) => ("Update", c.added.len() + c.removed.len()),
        (false, _) => ("Update", c.kept.len() + c.added.len() + c.removed.len()),
    };
    match notify {
        ops::Notify::Outside => format!("{what} sent to guests who don't use {mailer} Calendar"),
        _ => format!("{what} sent to {}", people(n)),
    }
}

/// `draft` with the fields in `lost` back as `base` had them.
fn reset(draft: &EventDraft, base: &EventData, lost: &[Field]) -> EventDraft {
    let was = opened(base, draft.calendar);
    let mut out = draft.clone();
    for field in lost {
        match field {
            Field::Title => out.title.clone_from(&was.title),
            Field::Time => out.span = was.span,
            Field::Repeat => out.rule.clone_from(&draft.rule_was),
            Field::Location => out.location.clone_from(&was.location),
            Field::Description => out.description.clone_from(&was.description),
            Field::People => {
                out.people.clone_from(&was.people);
                out.room.clone_from(&was.room);
            }
            Field::Options => out.options = was.options,
            Field::Alarms => out.reminder = was.reminder,
            Field::ShowAs => out.show_as = was.show_as,
            Field::Visibility => out.visibility = was.visibility,
            Field::MeetingLink | Field::Status | Field::Color | Field::Categories => {}
        }
    }
    out
}

/// The stored series the draft's occurrence belongs to.
fn series_of(conn: &Connection, draft: &EventDraft) -> Result<Option<EventData>> {
    let Some(base) = &draft.base else { return Ok(None) };
    Ok(ops::stored(conn, draft.calendar, &base.uid, None)?.map(|s| s.event).filter(|m| m.recurrence.is_some()))
}

/// The series `series` with the changes the user made to one of its
/// occurrences: its time moved as far as the occurrence's was, and every
/// other field as the user set it.
fn whole_series(draft: &EventDraft, series: &EventData, here: Tz) -> Result<EventData> {
    let base = draft.base.as_ref().ok_or_else(|| Error::Other("A new event has no series".into()))?;
    let mut whole = opened(series, draft.calendar);
    for field in changed(draft, base) {
        match field {
            DraftField::Time => whole.span = moved(&series.span, &base.span, &draft.span, here)?,
            DraftField::Title => whole.title.clone_from(&draft.title),
            DraftField::People => whole.people.clone_from(&draft.people),
            DraftField::Options => whole.options = draft.options,
            DraftField::Room => whole.room.clone_from(&draft.room),
            DraftField::Location => whole.location.clone_from(&draft.location),
            DraftField::Video => whole.video = draft.video,
            DraftField::Description => whole.description.clone_from(&draft.description),
            DraftField::ShowAs => whole.show_as = draft.show_as,
            DraftField::Reminder => whole.reminder = draft.reminder,
            DraftField::Visibility => whole.visibility = draft.visibility,
            DraftField::Repeat => whole.rule.clone_from(&draft.rule),
            DraftField::Calendar => {}
        }
    }
    Ok(event_of(&whole))
}

/// `series` moved as `from` moved to `to`: the same shift on the wall
/// clock, or in days for an all-day series.
fn moved(series: &Span, from: &Span, to: &Span, here: Tz) -> Result<Span> {
    match (series, from, to) {
        (Span::Timed { start, end }, Span::Timed { start: a, end: b }, Span::Timed { start: c, end: d }) => {
            let wall = |l: &Local| tz::wall(l.instant(here), start.zone, here);
            let (shift, length) = (wall(c) - wall(a), (wall(d) - wall(c)) - (wall(b) - wall(a)));
            Ok(Span::Timed { start: Local { wall: start.wall + shift, ..*start }, end: Local { wall: end.wall + shift + length, ..*end } })
        }
        (Span::AllDay { start, end }, Span::AllDay { start: a, end: b }, Span::AllDay { start: c, end: d }) => {
            Ok(Span::AllDay { start: *start + (*c - *a), end: *end + (*c - *a) + ((*d - *c) - (*b - *a)) })
        }
        _ => Err(Error::Other("A series can't change between all day and timed from one occurrence; open the series instead.".into())),
    }
}

/// Microsoft 365 refuses to move an occurrence onto or past the day of the
/// one before or after it (`ErrorOccurrenceCrossingBoundary`).
fn crossing(conn: &Connection, draft: &EventDraft, here: Tz) -> Result<Option<String>> {
    let (Some(base), Some(series)) = (&draft.base, series_of(conn, draft)?) else { return Ok(None) };
    let Some(at) = base.recurrence_id else { return Ok(None) };
    let day = |m: Moment| match m {
        Moment::Date(d) => d,
        Moment::Time(t) => tz::wall(t.instant(here), Zone::Tz(here), here).date(),
    };
    let rows = ops::series(conn, draft.calendar, &base.uid)?;
    let overrides: Vec<EventData> = rows.into_iter().filter(|e| e.recurrence_id.is_some()).collect();
    let around = at_instant(&at, here);
    let window = super::recur::Window { start: around - Duration::days(400), end: around + Duration::days(400) };
    let list = super::recur::expand(&series, &overrides, window, here);
    let mine = list.iter().position(|o| {
        let rid = match o.source {
            super::recur::Source::Master => Some(o.span.start()),
            super::recur::Source::Override(i) => overrides[i].recurrence_id,
        };
        rid.is_some_and(|r| r.names(&at, here))
    });
    let Some(i) = mine else { return Ok(None) };
    let new_day = day(draft.span.start());
    if let Some(next) = list.get(i + 1).map(|o| day(o.span.start()))
        && new_day >= next
    {
        return Ok(Some(format!("Microsoft 365 can't move this occurrence onto or past the next one ({}). Change the series instead.", next.format("%a %-d %b"))));
    }
    if let Some(prev) = i.checked_sub(1).and_then(|j| list.get(j)).map(|o| day(o.span.start()))
        && new_day <= prev
    {
        return Ok(Some(format!("Microsoft 365 can't move this occurrence onto or before the one before it ({}). Change the series instead.", prev.format("%a %-d %b"))));
    }
    Ok(None)
}

fn at_instant(m: &Moment, here: Tz) -> DateTime<Utc> {
    match m {
        Moment::Time(t) => t.instant(here),
        Moment::Date(d) => tz::instant(d.and_time(chrono::NaiveTime::MIN), here),
    }
}

/// Saves the draft through the op queue. A change to one occurrence of a
/// series needs its `scope`, and one the guests would hear about the
/// `notify` of one of the Send an update? dialog's choices; any other save
/// tells nobody, whatever `notify` says. When the stored event changed
/// since the editor opened it, the user's changes are merged into it field
/// by field; a field both changed keeps the stored value and the message
/// says so.
pub fn save_draft(conn: &Connection, draft: &EventDraft, scope: Option<Scope>, notify: Option<ops::Notify>, message: Option<String>, now: i64, horizon: &Horizon, here: Tz) -> Result<Saved> {
    if let Some(p) = check_draft(conn, draft, here)?.into_iter().find(|p| p.blocking) {
        return Err(Error::Other(p.text));
    }
    let title = if draft.title.trim().is_empty() { "(no title)".to_string() } else { draft.title.trim().to_string() };
    let calendar = store::calendar(conn, draft.calendar)?;
    let plan = plan_save(conn, draft, here)?;
    let scope = match (plan.scopes.is_empty(), scope) {
        (true, _) => Scope::All,
        (false, Some(s)) if plan.scopes.iter().any(|c| c.scope == s) => s,
        (false, _) => return Err(Error::Other(format!("Choose {} to save {title}.", plan.scopes.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ")))),
    };
    let notify = match (&plan.dialog, notify) {
        (None, _) => ops::Notify::Nobody,
        (Some(d), Some(n)) if d.choices.iter().any(|c| c.notify == Some(n)) => n,
        (Some(d), _) => {
            let offered: Vec<&str> = d.choices.iter().filter(|c| c.notify.is_some()).map(|c| c.label.as_str()).collect();
            return Err(Error::Other(format!("{} Choose {}.", d.title, offered.join(", "))));
        }
    };
    let message = message.filter(|m| !m.trim().is_empty() && notify != ops::Notify::Nobody);
    let told = match notify {
        ops::Notify::Nobody => None,
        n => Some(sent_line(&classified(conn, draft)?.2, n, draft.base.is_none(), &mailer(conn, &calendar, &event_of(draft))?.0)),
    };
    if scope == Scope::Following {
        let edited = event_of(draft);
        let rule = draft.rule.clone().ok_or_else(|| Error::Other(format!("{title} doesn't repeat")))?;
        let op = split::queue_following(conn, draft.calendar, &edited, &rule, notify, message, now, horizon)?;
        let uid = match op {
            Some(id) => store::calendar_op(conn, id)?.after.map(|a| store::calendar_op(conn, a)).transpose()?.map_or_else(|| draft.uid.clone(), |a| a.uid),
            None => draft.uid.clone(),
        };
        return Ok(Saved { op, calendar: draft.calendar, uid, message: told.unwrap_or_else(|| format!("Saved {title} from this event on")) });
    }
    let (mut edit, lost) = edit(conn, draft, &calendar, scope)?;
    edit.notify = notify;
    edit.message = message;
    let creating = matches!(edit.change, Change::Create(_));
    let op = ops::queue(conn, &edit, now, horizon)?;
    let message = match (creating, lost.as_slice(), plan.scopes.is_empty(), scope) {
        _ if told.is_some() && lost.is_empty() => told.unwrap_or_default(),
        (true, _, _, _) => format!("Added {title}"),
        (false, [], false, Scope::All) => format!("Saved all events of {title}"),
        (false, [], _, _) => format!("Saved {title}"),
        (false, lost, _, _) => format!("Saved {title}. It changed elsewhere while you edited; your new {} wasn't saved.", nouns(lost)),
    };
    Ok(Saved { op, calendar: draft.calendar, uid: draft.uid.clone(), message })
}

fn nouns(fields: &[Field]) -> String {
    let words: Vec<&str> = fields.iter().map(|f| f.noun()).collect();
    match words.as_slice() {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Whether the event can be deleted from the editor or the grid, and for
/// one occurrence of a series, for which of the series.
pub fn plan_delete(conn: &Connection, event: EventId, at: Option<Bounds>, here: Tz) -> Result<DeletePlan> {
    let draft = draft_for_event(conn, event, at, here)?;
    delete_plan(conn, &draft, here)
}

/// `plan_delete` for the stored event a draft opened, found again by its
/// UID and occurrence: a sync may have replaced its row meanwhile. The
/// locks are the store's.
pub fn plan_delete_drafted(conn: &Connection, draft: &EventDraft, here: Tz) -> Result<DeletePlan> {
    let calendar = store::calendar(conn, draft.calendar)?;
    let (locks, notice) = locks_now(conn, draft, &calendar)?;
    // The occurrence as it is stored, which the delete takes, not the
    // draft's unsaved fields.
    let base = draft.base.as_ref().ok_or_else(|| Error::Other("A new event has nothing to delete".into()))?;
    let stored = EventDraft { locks, notice, rule: draft.rule_was.clone(), rule_was: draft.rule_was.clone(), ..opened(base, draft.calendar) };
    delete_plan(conn, &stored, here)
}

fn delete_plan(conn: &Connection, draft: &EventDraft, here: Tz) -> Result<DeletePlan> {
    let calendar = store::calendar(conn, draft.calendar)?;
    let account = account_of(conn, &calendar)?.unwrap_or_default();
    let other = draft.base.as_ref().and_then(|b| other_organizer(&calendar, &account, b)).cloned();
    // A meeting someone else organizes is declined, which locks only its
    // meeting fields; anything else locked can't be deleted here.
    let refusal = (other.is_none() && draft.locks.contains(&DraftField::Title)).then(|| draft.notice.clone().unwrap_or_else(|| "This event can't be deleted here.".into()));
    let mut scopes = delete_scopes(conn, draft, here)?;
    if other.is_some() {
        scopes.retain(|c| c.scope != Scope::Following);
    }
    let prompt = (!scopes.is_empty()).then(|| {
        let day = match draft.span.start() {
            Moment::Date(d) => d,
            Moment::Time(t) => t.wall.date(),
        };
        let title = if draft.title.trim().is_empty() { "(no title)" } else { draft.title.trim() };
        format!("You're deleting {title} on {}.", day.format("%a %-d %b"))
    });
    let caps = caps::of(&calendar);
    let people = draft.base.as_ref().map_or(0, |b| guests(&calendar, &account, b).len());
    let cancel = match (&other, &draft.base) {
        (None, Some(base)) if people > 0 && (caps.sends_itself || organizer::sends_itself(&calendar)) => {
            let list: Vec<Scope> = if scopes.is_empty() { vec![Scope::All] } else { scopes.iter().map(|c| c.scope).collect() };
            let (mailer, _) = mailer(conn, &calendar, base)?;
            list.into_iter().map(|s| (s, change::cancel_dialog(people, &caps, &mailer, s))).collect()
        }
        _ => Vec::new(),
    };
    let senders = match (&other, &draft.base) {
        (None, Some(base)) => senders_needed(conn, &calendar, base)?,
        _ => Vec::new(),
    };
    let decline = other.map(|o| Decline {
        title: "Decline and remove?".into(),
        body: format!("{} gets your decline, and the meeting leaves your calendar.", change::name(&o)),
        organizer: change::name(&o).to_string(),
    });
    Ok(DeletePlan { refusal, title: draft.title.clone(), scopes, prompt, cancel, decline, senders })
}

/// Who a delete tells, from its plan: a meeting the user organizes is
/// cancelled for its guests, with the user's words where the cancellation
/// carries them; a meeting someone else organizes is declined instead.
fn cancelling(plan: &DeletePlan, scope: Scope, message: Option<String>) -> Result<(ops::Notify, Option<String>, Option<String>)> {
    if let Some(d) = &plan.decline {
        return Err(Error::Other(format!("{} organizes this meeting. Decline it instead.", d.organizer)));
    }
    Ok(match plan.cancel.iter().find(|(s, _)| *s == scope) {
        Some((_, dialog)) => {
            let line = format!("Cancellation sent to {} {}", dialog.people, if dialog.people == 1 { "person" } else { "people" });
            (ops::Notify::All, message.filter(|m| dialog.message && !m.trim().is_empty()), Some(line))
        }
        None => (ops::Notify::Nobody, None, None),
    })
}

fn delete_scopes(conn: &Connection, draft: &EventDraft, here: Tz) -> Result<Vec<ScopeChoice>> {
    let (Some(at), Some(series)) = (draft.base.as_ref().and_then(|b| b.recurrence_id), series_of(conn, draft)?) else { return Ok(Vec::new()) };
    let mut scopes = vec![ScopeChoice { scope: Scope::This, label: "This event".into(), detail: None }];
    if !series.span.start().names(&at, here) {
        scopes.push(ScopeChoice { scope: Scope::Following, label: "This and following events".into(), detail: Some("Ends the series before this event.".into()) });
    }
    scopes.push(ScopeChoice { scope: Scope::All, label: "All events".into(), detail: None });
    Ok(scopes)
}

/// Deletes a single event, or a whole series, through the op queue. A
/// meeting the user organizes is cancelled for its guests, with `message`
/// where the cancellation carries one; anything else tells nobody.
pub fn delete_event(conn: &Connection, event: EventId, message: Option<String>, now: i64, horizon: &Horizon, here: Tz) -> Result<Saved> {
    let plan = plan_delete(conn, event, None, here)?;
    if let Some(refusal) = plan.refusal.clone() {
        return Err(Error::Other(refusal));
    }
    let (notify, message, told) = cancelling(&plan, Scope::All, message)?;
    let (calendar, uid): (CalendarId, String) = conn.query_row("SELECT calendar_id, uid FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let edit = Edit { notify, message, ..Edit::quiet(calendar, Change::Delete { uid: uid.clone() }) };
    let op = ops::queue(conn, &edit, now, horizon)?;
    let title = if plan.title.is_empty() { "(no title)".into() } else { plan.title };
    Ok(Saved { op, calendar, uid, message: told.unwrap_or_else(|| format!("Deleted {title}")) })
}

/// Deletes the stored event a draft opened, found again by its UID: the
/// row may have been replaced by a sync, or left the period on screen. An
/// occurrence of a series goes for `scope`: itself, it and the ones after
/// it, or the whole series.
pub fn delete_drafted(conn: &Connection, draft: &EventDraft, scope: Option<Scope>, message: Option<String>, now: i64, horizon: &Horizon, here: Tz) -> Result<Saved> {
    let base = draft.base.as_ref().ok_or_else(|| Error::Other("A new event has nothing to delete".into()))?;
    let title = if draft.title.trim().is_empty() { "(no title)".to_string() } else { draft.title.trim().to_string() };
    let scopes = delete_scopes(conn, draft, here)?;
    let (Some(at), false) = (base.recurrence_id, scopes.is_empty()) else {
        let stored = match ops::stored(conn, draft.calendar, &base.uid, base.recurrence_id.as_ref())? {
            Some(s) => s,
            None => ops::stored(conn, draft.calendar, &base.uid, None)?.ok_or_else(|| Error::Other(format!("{} is already gone.", base.title)))?,
        };
        return delete_event(conn, stored.id, message, now, horizon, here);
    };
    let plan = plan_delete_drafted(conn, draft, here)?;
    let (notify, message, told) = cancelling(&plan, scope.unwrap_or(Scope::All), message)?;
    let (_, locks_notice) = locks_now(conn, draft, &store::calendar(conn, draft.calendar)?)?;
    if let Some(notice) = locks_notice.filter(|_| !draft.locks.is_empty()) {
        return Err(Error::Other(notice));
    }
    let change = match scope {
        Some(Scope::This) => Change::DeleteOccurrence { uid: base.uid.clone(), recurrence_id: at },
        Some(Scope::Following) if scopes.iter().any(|c| c.scope == Scope::Following) => {
            let series = series_of(conn, draft)?.ok_or_else(|| Error::Other(format!("{title} is already gone.")))?;
            Change::Update(split::ended(&series, &at, here))
        }
        Some(Scope::All) => Change::Delete { uid: base.uid.clone() },
        _ => return Err(Error::Other(format!("Choose {} to delete {title}.", scopes.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ")))),
    };
    let op = ops::queue(conn, &Edit { notify, message, ..Edit::quiet(draft.calendar, change) }, now, horizon)?;
    let message = match scope {
        _ if told.is_some() => told.unwrap_or_default(),
        Some(Scope::This) => format!("Deleted {title} on {}", match at {
            Moment::Date(d) => d,
            Moment::Time(t) => t.wall.date(),
        }.format("%a %-d %b")),
        Some(Scope::Following) => format!("Deleted {title} from this event on"),
        _ => format!("Deleted all events of {title}"),
    };
    Ok(Saved { op, calendar: draft.calendar, uid: base.uid.clone(), message })
}

/// The calendar new events go to: the one the user chose in Settings ›
/// Calendar, shown or not, else the first shown calendar Jolta can write to.
pub fn default_calendar(conn: &Connection) -> Result<Option<Calendar>> {
    let writable: Vec<Calendar> = store::calendars(conn)?.into_iter().filter(|c| caps::of(c).writable).collect();
    Ok(writable.iter().find(|c| c.is_default).or_else(|| writable.iter().find(|c| c.shown)).cloned())
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Access, Recurrence};
    use crate::store::Store;

    const NOW: i64 = 1_795_000_000;
    const ME: &str = "me@example.com";

    fn horizon() -> Horizon {
        Horizon::around(Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap(), Brussels)
    }

    fn setup(name: &str) -> (Store, CalendarId) {
        let dir = std::env::temp_dir().join(format!("jolta-cal-draft-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        let cal = s.write(|tx| ops::new_local_calendar(tx, "Test", Some("#3366ff"))).unwrap().id;
        (s, cal)
    }

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn span(a: &str, b: &str) -> Span {
        Span::Timed { start: at(a), end: at(b) }
    }

    fn calendar(source: CalendarSource, access: Access, remote_id: &str) -> Calendar {
        Calendar {
            id: CalendarId(1),
            account_id: None,
            source,
            remote_id: remote_id.into(),
            name: "Work".into(),
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

    fn person(address: &str, name: &str) -> Person {
        Person { address: address.into(), name: name.into() }
    }

    fn attendee(address: &str, is_me: bool, role: Role) -> Attendee {
        Attendee { person: person(address, ""), role, response: Response::Accepted, comment: String::new(), is_me }
    }

    fn meeting(organizer: &str, attendees: Vec<Attendee>) -> EventData {
        EventData { title: "Planning".into(), organizer: Some(person(organizer, "Jan Peeters")), attendees, ..EventData::new("m1", span("2026-11-26 10:00", "2026-11-26 11:30")) }
    }

    fn open(c: &Calendar, account: &str, e: &EventData) -> (Vec<DraftField>, Option<String>) {
        access(c, account, e, e.recurrence.as_ref(), &e.span, Brussels)
    }

    fn sentence(c: &Calendar, account: &str, e: &EventData) -> (bool, Option<String>) {
        let (locks, notice) = open(c, account, e);
        (locks.len() == EVERY_FIELD.len(), notice)
    }

    /// The read-only table in section 5, and check 1: who "me" is.
    #[test]
    fn locks_follow_the_read_only_table() {
        let google = calendar(CalendarSource::Google, Access::Owner, ME);
        let alone = EventData { title: "Focus".into(), ..EventData::new("a1", span("2026-11-24 09:00", "2026-11-24 10:00")) };
        assert_eq!(open(&google, ME, &alone), (Vec::new(), None), "the user's own event without guests is open");

        for c in [calendar(CalendarSource::Webcal, Access::Read, "feed"), calendar(CalendarSource::Graph, Access::Read, "x"), calendar(CalendarSource::Graph, Access::FreeBusy, "x")] {
            assert_eq!(sentence(&c, ME, &alone), (true, Some("This calendar is read-only.".into())), "{:?} {:?}", c.source, c.access);
        }

        let theirs = meeting("jan@example.com", vec![attendee("jan@example.com", false, Role::Chair), attendee(ME, true, Role::Required)]);
        assert_eq!(
            open(&google, ME, &theirs),
            (MEETING_FIELDS.to_vec(), Some("Jan Peeters organizes this meeting. You can change only your own settings.".into())),
            "someone else's meeting leaves the user's own settings open"
        );
        let (locks, _) = open(&google, ME, &theirs);
        assert!(![DraftField::ShowAs, DraftField::Reminder, DraftField::Visibility].iter().any(|f| locks.contains(f)));

        let mine = meeting(ME, vec![attendee(ME, true, Role::Chair), attendee("els@example.com", false, Role::Required)]);
        assert_eq!(open(&google, ME, &mine), (Vec::new(), None), "the user's own meeting with guests is open");
        let alias = meeting("me.alias@example.com", vec![attendee("me.alias@example.com", true, Role::Chair), attendee("els@example.com", false, Role::Required)]);
        assert_eq!(open(&google, ME, &alias), (Vec::new(), None), "an alias the user's own calendar marks organizes as the user");

        let shared = calendar(CalendarSource::CalDav, Access::Write, "https://dav.example.com/cal/team/");
        assert_eq!(open(&shared, ME, &alone), (Vec::new(), None), "a shared calendar with write access is open");

        let room_only = EventData { attendees: vec![attendee("room-214@example.com", false, Role::Room)], ..alone.clone() };
        assert_eq!(open(&google, ME, &room_only), (Vec::new(), None), "a room isn't a guest");

        let weekly = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY".into()], ..Default::default() }), ..alone.clone() };
        assert_eq!(open(&google, ME, &weekly), (Vec::new(), None), "a series without guests is open");
        let graph = calendar(CalendarSource::Graph, Access::Owner, ME);
        let hourly = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=HOURLY;COUNT=3".into()], ..Default::default() }), ..alone.clone() };
        assert_eq!(open(&graph, ME, &hourly), (vec![DraftField::Repeat], Some("Microsoft 365 can't change this repeat rule.".into())), "a rule Graph can't hold");
    }

    /// iCloud rewrites the ORGANIZER of a meeting Jolta made to the Apple
    /// ID's preferred address, which isn't the account's (iCloud test
    /// account, 2026-11-08): the source's word that it is the user's makes
    /// the meeting the user's, only in the account's own calendar.
    #[test]
    fn an_organizer_the_source_marks_as_the_user_organizes_as_the_user() {
        let mut theirs = meeting("preferred@example.net", vec![attendee("els@example.com", false, Role::Required)]);
        let icloud = calendar(CalendarSource::CalDav, Access::Owner, "https://dav.example.com/calendars/home/");
        assert!(open(&icloud, ME, &theirs).1.is_some(), "an address of someone else");
        theirs.organizer_is_me = true;
        assert_eq!(open(&icloud, ME, &theirs), (Vec::new(), None), "the principal's own address");
        let shared = calendar(CalendarSource::CalDav, Access::Write, "https://dav.example.com/calendars/team/");
        assert!(open(&shared, ME, &theirs).1.is_some(), "a calendar shared with the user marks its owner");
    }

    /// Check 1: Graph and iCloud list a guest's copy with the user as its
    /// only attendee, the organizer among none; it is still someone else's.
    #[test]
    fn an_invitation_listing_only_the_user_is_someone_elses() {
        let google = calendar(CalendarSource::Graph, Access::Owner, ME);
        let only_me = meeting("jan@example.com", vec![attendee(ME, true, Role::Required)]);
        assert_eq!(open(&google, ME, &only_me).0, MEETING_FIELDS.to_vec());
        let own = EventData { attendees: Vec::new(), ..meeting("team@group.calendar.google.com", Vec::new()) };
        assert_eq!(open(&google, ME, &own), (Vec::new(), None), "an event without attendees stays open whatever organizer the source names");
    }

    /// Google gives its owner role to a calendar shared with "Make changes
    /// and manage sharing", and marks that calendar's owner as `self`: only
    /// the primary calendar's marked attendee is the user.
    #[test]
    fn google_shared_owner_calendar_marks_its_owner_not_the_user() {
        let owner = "anna@example.com";
        let organized = meeting(owner, vec![attendee(owner, true, Role::Chair), attendee("els@example.com", false, Role::Required)]);
        let primary = calendar(CalendarSource::Google, Access::Owner, ME);
        let shared = calendar(CalendarSource::Google, Access::Owner, "team@group.calendar.google.com");
        assert_eq!(open(&primary, ME, &organized), (Vec::new(), None), "in the primary calendar the marked organizer is the user");
        assert_eq!(
            open(&shared, ME, &organized).1,
            Some("Jan Peeters organizes this meeting. You can change only your own settings.".into()),
            "in a shared calendar the marked organizer is its owner"
        );
        let owners_own = EventData { attendees: vec![attendee(owner, true, Role::Chair)], ..organized };
        assert_eq!(open(&shared, ME, &owners_own).0, [DraftField::Title; 0], "the owner's event without guests stays open to write access");
    }

    #[test]
    fn a_new_draft_starts_at_the_next_half_hour_for_the_default_length() {
        let (s, cal) = setup("new");
        let now = Brussels.with_ymd_and_hms(2026, 11, 7, 15, 10, 0).unwrap().with_timezone(&Utc);
        let d = s.read(|c| draft_for_new(c, None, None, now, Brussels)).unwrap();
        assert_eq!((d.calendar, d.span), (cal, span("2026-11-07 15:30", "2026-11-07 16:00")), "30 minutes, the default calendar");
        s.write(|tx| store::set_default_event_minutes(tx, 45)).unwrap();
        let d = s.read(|c| draft_for_new(c, None, None, now, Brussels)).unwrap();
        assert_eq!(d.span, span("2026-11-07 15:30", "2026-11-07 16:15"));
        assert_eq!(next_half_hour(Brussels.with_ymd_and_hms(2026, 11, 7, 15, 30, 0).unwrap().with_timezone(&Utc), Brussels).to_string(), "2026-11-07 16:00:00");
    }

    #[test]
    fn the_default_calendar_is_the_chosen_one() {
        let (s, test) = setup("default");
        let family = s.write(|tx| ops::new_local_calendar(tx, "Family", None)).unwrap().id;
        assert_eq!(s.read(|c| default_calendar(c)).unwrap().map(|c| c.id), Some(family), "without a choice, the first in the sidebar's order");
        s.write(|tx| store::set_default_calendar(tx, Some(test))).unwrap();
        assert_eq!(s.read(|c| default_calendar(c)).unwrap().map(|c| c.id), Some(test));
        s.write(|tx| store::set_default_calendar(tx, Some(family))).unwrap();
        let defaults: i64 = s.read(|c| Ok(c.query_row("SELECT count(*) FROM calendar WHERE is_default", [], |r| r.get(0))?)).unwrap();
        assert_eq!(defaults, 1, "one default at a time");
        s.write(|tx| store::set_default_calendar(tx, None)).unwrap();
        assert_eq!(s.read(|c| default_calendar(c)).unwrap().map(|c| c.id), Some(family), "cleared: the first in the sidebar's order again");
        s.write(|tx| store::set_calendar_shown(tx, test, false)).unwrap();
        s.write(|tx| store::set_default_calendar(tx, Some(test))).unwrap();
        assert_eq!(s.read(|c| default_calendar(c)).unwrap().map(|c| c.id), Some(test), "a chosen default counts even while hidden");
    }

    fn event_id(s: &Store, cal: CalendarId, uid: &str) -> EventId {
        s.read(|c| Ok(c.query_row("SELECT id FROM event WHERE calendar_id = ?1 AND uid = ?2", rusqlite::params![cal, uid], |r| r.get(0))?)).unwrap()
    }

    fn new_draft(s: &Store, cal: CalendarId, title: &str) -> EventDraft {
        let now = Utc.timestamp_opt(NOW, 0).unwrap();
        let mut d = s.read(|c| draft_for_new(c, Some(cal), Some(span("2026-11-24 14:00", "2026-11-24 15:00")), now, Brussels)).unwrap();
        d.title = title.into();
        d
    }

    #[test]
    fn create_edit_and_delete_through_the_queue_with_undo() {
        let (s, cal) = setup("flow");
        let d = new_draft(&s, cal, "Design review");
        let saved = s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(saved.message, "Added Design review");
        let id = event_id(&s, cal, &d.uid);

        let mut d = s.read(|c| draft_for_event(c, id, None, Brussels)).unwrap();
        assert_eq!((d.title.as_str(), d.locks.len(), s.read(|c| plan_save(c, &d, Brussels)).unwrap().unchanged), ("Design review", 0, true));
        d.span = span("2026-11-24 15:00", "2026-11-24 16:00");
        d.reminder = Reminder::Before(10);
        assert!(!s.read(|c| plan_save(c, &d, Brussels)).unwrap().unchanged);
        let saved = s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(saved.message, "Saved Design review");
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!((stored.span, stored.alarms.as_slice()), (d.span, [Alarm { trigger: AlarmTrigger::MinutesBefore(10), email: false }].as_slice()));

        let again = s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(again.op, None, "the same save twice queues one op");

        let id = event_id(&s, cal, &d.uid);
        let deleted = s.write(|tx| delete_event(tx, id, None, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(deleted.message, "Deleted Design review");
        assert!(s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().is_none());
        s.write(|tx| ops::undo(tx, deleted.op.unwrap(), NOW, &horizon())).unwrap();
        assert_eq!(s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event.span, d.span, "Undo brings it back");
    }

    /// Check 4: a change made elsewhere while the editor was open stays.
    #[test]
    fn a_change_made_while_editing_is_merged_not_overwritten() {
        let (s, cal) = setup("merge");
        let d = new_draft(&s, cal, "Standup");
        s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        let id = event_id(&s, cal, &d.uid);
        let mut editing = s.read(|c| draft_for_event(c, id, None, Brussels)).unwrap();

        let mut elsewhere = editing.base.clone().unwrap();
        elsewhere.location = "Room 2.14".into();
        elsewhere.title = "Daily standup".into();
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Update(elsewhere)), NOW, &horizon())).unwrap();

        editing.span = span("2026-11-24 16:00", "2026-11-24 16:30");
        editing.title = "Standup moved".into();
        let saved = s.write(|tx| save_draft(tx, &editing, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!((stored.location.as_str(), stored.span), ("Room 2.14", editing.span), "the other field stays, the user's time lands");
        assert_eq!(stored.title, "Daily standup", "both changed the title: the stored one stays");
        assert_eq!(saved.message, "Saved Standup moved. It changed elsewhere while you edited; your new title wasn't saved.");
    }

    #[test]
    fn check_draft_names_each_problem_where_it_is_fixed() {
        let (s, cal) = setup("check");
        let problems = |d: &EventDraft| s.read(|c| check_draft(c, d, Brussels)).unwrap();
        let fields = |d: &EventDraft| problems(d).into_iter().map(|p| (p.field, p.blocking)).collect::<Vec<_>>();

        let mut d = new_draft(&s, cal, "Review");
        assert!(problems(&d).is_empty());
        d.span = span("2026-11-24 15:00", "2026-11-24 14:00");
        assert_eq!(fields(&d), [(Some(DraftField::Time), true)]);
        d.span = Span::AllDay { start: NaiveDate::from_ymd_opt(2026, 11, 24).unwrap(), end: NaiveDate::from_ymd_opt(2026, 11, 24).unwrap() };
        assert_eq!(fields(&d), [(Some(DraftField::Time), true)], "an all-day end on the start day");

        let mut d = new_draft(&s, cal, "Review");
        d.span = span("2027-03-28 02:30", "2027-03-28 04:00");
        let p = problems(&d);
        assert_eq!(p.iter().map(|p| (p.text.as_str(), p.blocking)).collect::<Vec<_>>(), [("02:30 doesn't exist on 28 March; it becomes 03:30.", false)]);

        let mut d = new_draft(&s, cal, "Review");
        d.room = "room 2.14".into();
        assert_eq!(fields(&d), [(Some(DraftField::Room), true)]);
        d.room = "room-214@example.com".into();
        assert!(problems(&d).is_empty(), "a room saves on a calendar on this Mac");

        let mut d = new_draft(&s, cal, "Review");
        d.video = Some(MeetingProvider::Teams);
        assert_eq!(fields(&d), [(Some(DraftField::Video), true)], "no video call can be made yet, so it never drops silently");

        let mut d = new_draft(&s, cal, "Review");
        d.people = vec![attendee("not an address", false, Role::Required)];
        assert_eq!(fields(&d), [(Some(DraftField::People), true)]);
    }

    /// Google and Microsoft 365 give a new event without alerts the
    /// calendar's default reminder; "None" must say so in the request.
    #[test]
    fn a_new_event_without_a_reminder_tells_the_provider() {
        let (s, _) = setup("reminder");
        let remote = store::RemoteCalendar { remote_id: "me@example.com".into(), name: "Work".into(), color: None, zone: None, access: crate::calendar::Access::Owner, primary: true, refresh: None };
        let google = s.write(|tx| store::add_calendar(tx, CalendarSource::Google, &remote)).unwrap().id;
        let now = Utc.timestamp_opt(NOW, 0).unwrap();
        let queued = |reminder: Reminder| {
            let mut d = s.read(|c| draft_for_new(c, Some(google), Some(span("2026-11-24 14:00", "2026-11-24 15:00")), now, Brussels)).unwrap();
            d.reminder = reminder;
            let op = s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap().op.unwrap();
            s.read(|c| store::calendar_op(c, op)).unwrap().extra
        };
        assert_eq!(queued(Reminder::Default), None, "the calendar's own default");
        let off = serde_json::json!({ "reminders": { "useDefault": false, "overrides": [] } });
        assert_eq!(queued(Reminder::Off), off.as_object().cloned().map(Extra::Json));
    }

    /// Check 4: the dry run behind Save's problems checks the change Save
    /// will queue, merged with a change made elsewhere.
    #[test]
    fn the_refusal_check_sees_the_merged_change() {
        let (s, cal) = setup("refusal");
        let d = new_draft(&s, cal, "Review");
        s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        let mut editing = s.read(|c| draft_for_event(c, event_id(&s, cal, &d.uid), None, Brussels)).unwrap();
        let mut elsewhere = editing.base.clone().unwrap();
        elsewhere.location = "Room 2.14".into();
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Update(elsewhere)), NOW, &horizon())).unwrap();
        editing.title = "Review 2".into();
        let (edit, lost) = s.read(|c| edit(c, &editing, &store::calendar(c, cal)?, Scope::All)).unwrap();
        let Change::Update(event) = edit.change else { panic!("an update") };
        assert_eq!((event.title.as_str(), event.location.as_str(), lost.len()), ("Review 2", "Room 2.14", 0));
    }

    #[test]
    fn delete_finds_the_event_a_draft_opened_again() {
        let (s, cal) = setup("delete");
        let d = new_draft(&s, cal, "Review");
        s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        let mut opened = s.read(|c| draft_for_event(c, event_id(&s, cal, &d.uid), None, Brussels)).unwrap();
        opened.title = "Review moved".into();
        s.write(|tx| save_draft(tx, &opened, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        let deleted = s.write(|tx| delete_drafted(tx, &opened, None, None, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(deleted.message, "Deleted Review moved", "the row was replaced since the draft opened");
        assert!(s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().is_none());
    }

    #[test]
    fn a_room_is_a_room_attendee_and_fills_an_empty_location() {
        let (s, cal) = setup("room");
        let mut d = new_draft(&s, cal, "Review");
        d.room = "room-214@example.com".into();
        s.write(|tx| save_draft(tx, &d, None, Some(ops::Notify::Nobody), None, NOW, &horizon(), Brussels)).unwrap();
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!(stored.attendees.iter().map(|a| (a.person.address.as_str(), a.role)).collect::<Vec<_>>(), [("room-214@example.com", Role::Room)]);
        assert_eq!(stored.location, "room-214@example.com");
        let mut reopened = s.read(|c| draft_for_event(c, event_id(&s, cal, &d.uid), None, Brussels)).unwrap();
        assert_eq!((reopened.room.as_str(), reopened.location.as_str(), reopened.people.len()), ("room-214@example.com", "", 0), "the location only repeated the room");
        assert!(s.read(|c| plan_save(c, &reopened, Brussels)).unwrap().unchanged);

        reopened.room = "room-301@example.com".into();
        s.write(|tx| save_draft(tx, &reopened, None, Some(ops::Notify::Nobody), None, NOW, &horizon(), Brussels)).unwrap();
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!(stored.location, "room-301@example.com", "the location follows the room while the user has none");

        let mut reopened = s.read(|c| draft_for_event(c, event_id(&s, cal, &d.uid), None, Brussels)).unwrap();
        reopened.location = "Gent, Belfort".into();
        s.write(|tx| save_draft(tx, &reopened, None, Some(ops::Notify::Nobody), None, NOW, &horizon(), Brussels)).unwrap();
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!((stored.location.as_str(), stored.attendees.len()), ("Gent, Belfort", 1), "a location of the user's own stays");
    }

    #[test]
    fn a_prefill_fills_the_fields_and_its_guests_wait_for_invitations() {
        let (s, cal) = setup("prefill");
        let prefill = EventPrefill {
            title: "Project kick-off".into(),
            span: Some(span("2026-11-25 10:00", "2026-11-25 11:00")),
            required: vec![person("jan@example.com", "Jan Peeters")],
            optional: vec![person("els@example.com", "Els Maes")],
            room: Some("room-214@example.com".into()),
            description: "From the thread".into(),
            ..Default::default()
        };
        let now = Utc.timestamp_opt(NOW, 0).unwrap();
        let d = s.read(|c| draft_from_prefill(c, &prefill, now, Brussels)).unwrap();
        assert_eq!((d.calendar, d.title.as_str(), d.room.as_str(), d.description.as_str()), (cal, "Project kick-off", "room-214@example.com", "From the thread"));
        assert_eq!(d.people.iter().map(|a| a.role).collect::<Vec<_>>(), [Role::Required, Role::Optional]);
        assert!(s.read(|c| check_draft(c, &d, Brussels)).unwrap().is_empty(), "a calendar on this Mac takes people");
    }

    #[test]
    fn a_description_edit_keeps_the_meeting_block() {
        let (s, cal) = setup("desc");
        let block = "\n\n-::~:~::~:~::-\nJoin with Google Meet: https://meet.google.com/abc-defg-hij\n-::~:~::~:~::-";
        let e = EventData { title: "Sync".into(), description: format!("Old notes{block}"), ..EventData::new("d1", span("2026-11-24 09:00", "2026-11-24 09:30")) };
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Create(vec![e])), NOW, &horizon())).unwrap();
        let mut d = s.read(|c| draft_for_event(c, event_id(&s, cal, "d1"), None, Brussels)).unwrap();
        assert_eq!(d.description, "Old notes");
        d.description = "New notes".into();
        s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(s.read(|c| ops::stored(c, cal, "d1", None)).unwrap().unwrap().event.description, format!("New notes{block}"));
    }

    /// ⌘D and ⌥-drag: a new single event, without the people or the meeting.
    #[test]
    fn a_copy_leaves_out_the_people_the_repeat_and_the_meeting() {
        let (s, cal) = setup("copy");
        let join = "https://teams.microsoft.com/l/meetup-join/19%3ameeting_ZjE2%40thread.v2/0?context=%7b%7d";
        let e = EventData {
            title: "Design review".into(),
            location: format!("Room 4, {join}"),
            description: format!(
                "Weekly look at the designs.\n\nJoin on your computer: {join}\nMeeting ID: 349 112 803 75\nPasscode: kX7pQ2\n\nCall in: +32 2 588 63 12,,34911280375#"
            ),
            organizer: Some(person(ME, "")),
            attendees: vec![attendee(ME, true, Role::Chair), attendee("els@example.com", false, Role::Required)],
            recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY".into()], ..Default::default() }),
            meeting: Some(Meeting {
                provider: MeetingProvider::Teams,
                join_url: join.into(),
                meeting_id: Some("349 112 803 75".into()),
                passcode: Some("kX7pQ2".into()),
                phones: vec!["tel:+3225886312,,34911280375#".into()],
                other_links: Vec::new(),
            }),
            show_as: ShowAs::Free,
            ..EventData::new("c1", span("2026-11-24 09:00", "2026-11-24 10:00"))
        };
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Create(vec![e])), NOW, &horizon())).unwrap();
        let now = Utc.timestamp_opt(NOW, 0).unwrap();
        let next = Bounds::Utc { start: Utc.with_ymd_and_hms(2026, 12, 1, 8, 0, 0).unwrap(), end: Utc.with_ymd_and_hms(2026, 12, 1, 9, 0, 0).unwrap() };
        let copy = s.read(|c| draft_for_copy(c, event_id(&s, cal, "c1"), Some(next), now, Brussels)).unwrap();
        assert!(copy.base.is_none() && copy.uid != "c1", "a new event with a UID of its own");
        assert_eq!((copy.calendar, copy.title.as_str(), copy.span, copy.show_as), (cal, "Design review", span("2026-12-01 09:00", "2026-12-01 10:00"), ShowAs::Free),
                   "the occurrence's time, title and the user's own settings");
        assert!(copy.people.is_empty() && copy.rule.is_none() && copy.meeting.is_none() && copy.video.is_none(), "no guests, repeat or call");
        assert_eq!(copy.location, "Room 4", "the place stays, the link goes");
        assert_eq!(copy.description, "Weekly look at the designs.", "the notes stay, the joining lines go");
        assert!(s.read(|c| plan_save(c, &copy, Brussels)).unwrap().dialog.is_none(), "saving a copy mails nobody");

        let meet = "https://meet.google.com/abc-defg-hij";
        let e = EventData {
            title: "1:1".into(),
            location: meet.into(),
            description: format!("Agenda first.\n\nJoin: {meet}\nOr call +32 2 588 63 12 (PIN 418 207#)\n\nBring the prints."),
            meeting: Some(Meeting {
                provider: MeetingProvider::Meet,
                join_url: meet.into(),
                meeting_id: None,
                passcode: None,
                phones: vec!["tel:+3225886312,,418207#".into()],
                other_links: Vec::new(),
            }),
            ..EventData::new("c2", span("2026-11-25 11:00", "2026-11-25 12:00"))
        };
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Create(vec![e])), NOW, &horizon())).unwrap();
        let copy = s.read(|c| draft_for_copy(c, event_id(&s, cal, "c2"), None, now, Brussels)).unwrap();
        assert_eq!((copy.location.as_str(), copy.description.as_str()), ("", "Agenda first.\n\nBring the prints."), "a link in the user's own text goes too");
    }

    #[test]
    fn a_locked_event_refuses_to_save_or_delete() {
        let (s, cal) = setup("locked");
        let series = EventData {
            title: "Weekly".into(),
            recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY".into()], ..Default::default() }),
            ..EventData::new("s1", span("2026-11-24 09:00", "2026-11-24 09:30"))
        };
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Create(vec![series])), NOW, &horizon())).unwrap();
        let id = event_id(&s, cal, "s1");
        let week3 = Bounds::Utc { start: at("2026-12-08 09:00").instant(Brussels), end: at("2026-12-08 09:30").instant(Brussels) };
        let occurrence = s.read(|c| draft_for_event(c, id, Some(week3), Brussels)).unwrap();
        assert_eq!(occurrence.span, span("2026-12-08 09:00", "2026-12-08 09:30"), "the occurrence the view shows");
        assert!(s.read(|c| check_draft(c, &occurrence, Brussels)).unwrap().is_empty(), "opening an occurrence changes nothing");
        assert!(occurrence.locks.is_empty(), "a series without guests is open");
        let meeting = EventData {
            title: "Planning".into(),
            organizer: Some(person("jan@example.com", "Jan Peeters")),
            attendees: vec![Attendee { person: person("els@example.com", ""), role: Role::Required, response: Response::NeedsAction, comment: String::new(), is_me: false }],
            ..EventData::new("m1", span("2026-11-25 09:00", "2026-11-25 09:30"))
        };
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Create(vec![meeting])), NOW, &horizon())).unwrap();
        let id = event_id(&s, cal, "m1");
        let mut d = s.read(|c| draft_for_event(c, id, None, Brussels)).unwrap();
        assert_eq!(d.notice.as_deref(), Some("Jan Peeters organizes this meeting. You can change only your own settings."));
        d.title = "Changed".into();
        d.locks.clear();
        d.notice = None;
        assert!(s.write(|tx| save_draft(tx, &d, None, None, None, NOW, &horizon(), Brussels)).is_err(), "the store's locks count, not the draft's");
        let plan = s.read(|c| plan_delete(c, id, None, Brussels)).unwrap();
        assert_eq!((plan.refusal, plan.decline.map(|d| d.organizer)), (None, Some("Jan Peeters".into())), "a guest declines instead of deleting");
        assert!(s.write(|tx| delete_event(tx, id, None, NOW, &horizon(), Brussels)).is_err());
    }
}
