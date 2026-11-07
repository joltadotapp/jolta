//! The event editor's model (C3 spec, section 5): a draft made from a new
//! slot, another feature's prefill or a stored event; what the user may
//! change in it and why not; what is wrong with it; and saving or deleting
//! through the op queue.
//!
//! Until invitations exist (U5a), a meeting with guests is read-only here,
//! as the op queue refuses to change one; until the repeat editor and its
//! scopes exist (U4), so is a repeating event, which has no way yet to say
//! whether an edit is for one occurrence or all.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;

use super::change::Field;
use super::ops::{self, CalendarOpId, Change, Edit};
use super::series::{self, Horizon};
use super::{
    Alarm, AlarmTrigger, Attendee, Bounds, Calendar, CalendarId, CalendarSource, EventData, EventId, Local, Meeting, MeetingProvider,
    Person, Response, Role, ShowAs, Span, Visibility, Zone, caps, description, detail, merge, tz,
};
use crate::{Error, Result, store};

/// A part of the editor, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

const EVERY_FIELD: [DraftField; 12] = [
    DraftField::Title,
    DraftField::Time,
    DraftField::Repeat,
    DraftField::People,
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
    /// "Formatting from Outlook is removed when you change this text."
    pub description_note: Option<String>,
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

/// What saving needs before it can go ahead. U4 adds the scope prompt and
/// U5a the Send an update? dialog here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavePlan {
    /// The draft says what the stored event says: saving only closes.
    pub unchanged: bool,
}

/// A save or delete queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    /// The Undo token; None when nothing changed.
    pub op: Option<CalendarOpId>,
    pub calendar: CalendarId,
    /// The toast's words: "Saved Standup".
    pub message: String,
}

/// Whether and how an event can be deleted. U5a adds the cancel dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletePlan {
    /// Why it can't be, if it can't.
    pub refusal: Option<String>,
    pub title: String,
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
        description_note: None,
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
    let account = d.calendar.account_id.map(|a| store::account(conn, a)).transpose()?.map(|a| a.address).unwrap_or_default();
    let series = d.repeat.is_some() || d.event.recurrence_id.is_some();
    let (locks, notice) = access(&d.calendar, &account, &d.event, series);
    let conflict = ops::conflict(conn, d.calendar.id, &d.event.uid)?.map(|c| c.message);
    // An unchanged occurrence of a series is the master moved to its time:
    // the version the user starts from, as the occurrence the view shows.
    let base = match at.filter(|_| d.event.recurrence.is_some() && d.event.recurrence_id.is_none()) {
        Some(at) => series::occurrence(std::slice::from_ref(&d.event), &occurrence_span(&d.event.span, at, here).start(), here).unwrap_or(d.event),
        None => d.event,
    };
    let mut draft = opened(&base, d.calendar.id);
    draft.locks = locks;
    draft.notice = notice.or(conflict);
    draft.repeat = d.repeat;
    Ok(draft)
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
        description_note: parts.loses_formatting().then(|| "Formatting from Outlook is removed when you change this text.".into()),
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
/// the address of the calendar's account; the attendees a calendar marks
/// as the user count as the user only in the account's own calendar
/// (`Calendar::is_own`): Google marks the owner of a calendar shared with
/// the account the same way.
pub fn access(calendar: &Calendar, account: &str, event: &EventData, series: bool) -> (Vec<DraftField>, Option<String>) {
    if !caps::of(calendar).writable {
        return (EVERY_FIELD.to_vec(), Some("This calendar is read-only.".into()));
    }
    let own = calendar.is_own(account);
    let is_me = |p: &Person| {
        p.address.eq_ignore_ascii_case(account) && !account.is_empty()
            || own && event.attendees.iter().any(|a| a.is_me && a.person.address.eq_ignore_ascii_case(&p.address))
    };
    // An attendee the calendar marks as its owner isn't a guest, as for the
    // op queue: in a shared calendar that is the owner's own event.
    let guests = event.attendees.iter().any(|a| !is_room(a) && !a.is_me && !is_me(&a.person));
    let organizer = event.organizer.as_ref().filter(|o| !is_me(o));
    if guests {
        let first = match organizer {
            Some(o) => format!("{} organizes this meeting. ", if o.name.is_empty() { &o.address } else { &o.name }),
            None => String::new(),
        };
        return (EVERY_FIELD.to_vec(), Some(format!("{first}Jolta can't change meetings with guests yet.")));
    }
    if series {
        return (EVERY_FIELD.to_vec(), Some("Jolta can't change repeating events yet.".into()));
    }
    (Vec::new(), None)
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
            DraftField::Repeat => false,
            DraftField::People => people(&draft.people) != people(&was.people),
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
        } else if calendar.source != CalendarSource::Local && (touched.contains(&DraftField::Room) || !stored) {
            problem(Some(DraftField::Room), format!("Jolta can't book a room on {} calendars yet.", provider(&calendar)), true);
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
        } else if calendar.source != CalendarSource::Local {
            problem(Some(DraftField::People), format!("Jolta can't invite people on {} calendars yet.", provider(&calendar)), true);
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
    if out.iter().all(|p| !p.blocking) && !touched.is_empty() {
        let refusal = match ops::refusal(conn, &edit(draft)) {
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
    let series = stored.event.recurrence.is_some() || base.recurrence_id.is_some();
    Ok(access(calendar, &account, &stored.event, series))
}

/// The account address of a calendar, if it has an account.
fn account_of(conn: &Connection, calendar: &Calendar) -> Result<Option<String>> {
    calendar.account_id.map(|a| store::account(conn, a).map(|a| a.address)).transpose()
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

fn provider(calendar: &Calendar) -> &'static str {
    match calendar.source {
        CalendarSource::Google => "Google",
        CalendarSource::Graph => "Microsoft 365",
        CalendarSource::CalDav => "iCloud and CalDAV",
        CalendarSource::Local => "this Mac's",
        CalendarSource::Webcal => "subscribed",
        CalendarSource::EventKit => "macOS",
    }
}

fn edit(draft: &EventDraft) -> Edit {
    let event = event_of(draft);
    let change = if draft.base.is_some() { Change::Update(event) } else { Change::Create(vec![event]) };
    Edit::quiet(draft.calendar, change)
}

/// What saving the draft needs first.
pub fn plan_save(draft: &EventDraft) -> SavePlan {
    SavePlan { unchanged: draft.base.as_ref().is_some_and(|b| changed(draft, b).is_empty()) }
}

/// Saves the draft through the op queue, telling nobody. When the stored
/// event changed since the editor opened it, the user's changes are merged
/// into it field by field; a field both changed keeps the stored value and
/// the message says so.
pub fn save_draft(conn: &Connection, draft: &EventDraft, now: i64, horizon: &Horizon, here: Tz) -> Result<Saved> {
    if let Some(p) = check_draft(conn, draft, here)?.into_iter().find(|p| p.blocking) {
        return Err(Error::Other(p.text));
    }
    let mine = event_of(draft);
    let title = if mine.title.is_empty() { "(no title)".to_string() } else { mine.title.clone() };
    let (change, lost) = match &draft.base {
        None => (Change::Create(vec![mine]), Vec::new()),
        Some(base) => {
            let now_stored = ops::stored(conn, draft.calendar, &base.uid, base.recurrence_id.as_ref())?
                .ok_or_else(|| Error::Other(format!("{title} was deleted while you edited it.")))?
                .event;
            if now_stored == *base {
                (Change::Update(mine), Vec::new())
            } else {
                let merged = merge::three_way(base, &mine, &now_stored);
                (Change::Update(merged.event), merged.lost)
            }
        }
    };
    let creating = matches!(change, Change::Create(_));
    let op = ops::queue(conn, &Edit::quiet(draft.calendar, change), now, horizon)?;
    let message = match (creating, lost.as_slice()) {
        (true, _) => format!("Added {title}"),
        (false, []) => format!("Saved {title}"),
        (false, lost) => format!("Saved {title}. It changed elsewhere while you edited; your new {} wasn't saved.", nouns(lost)),
    };
    Ok(Saved { op, calendar: draft.calendar, message })
}

fn nouns(fields: &[Field]) -> String {
    let words: Vec<&str> = fields.iter().map(|f| f.noun()).collect();
    match words.as_slice() {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Whether the event can be deleted from the editor or the grid.
pub fn plan_delete(conn: &Connection, event: EventId, here: Tz) -> Result<DeletePlan> {
    let draft = draft_for_event(conn, event, None, here)?;
    let refusal = (!draft.locks.is_empty()).then(|| draft.notice.clone().unwrap_or_else(|| "This event can't be deleted here.".into()));
    Ok(DeletePlan { refusal, title: draft.title })
}

/// Deletes the event through the op queue, telling nobody.
pub fn delete_event(conn: &Connection, event: EventId, now: i64, horizon: &Horizon, here: Tz) -> Result<Saved> {
    let plan = plan_delete(conn, event, here)?;
    if let Some(refusal) = plan.refusal {
        return Err(Error::Other(refusal));
    }
    let (calendar, uid): (CalendarId, String) = conn.query_row("SELECT calendar_id, uid FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let op = ops::queue(conn, &Edit::quiet(calendar, Change::Delete { uid }), now, horizon)?;
    let title = if plan.title.is_empty() { "(no title)".into() } else { plan.title };
    Ok(Saved { op, calendar, message: format!("Deleted {title}") })
}

/// The calendar new events go to: the one the user chose in Settings ›
/// Calendar, else the first shown calendar Jolta can write to.
pub fn default_calendar(conn: &Connection) -> Result<Option<Calendar>> {
    let writable: Vec<Calendar> = store::calendars(conn)?.into_iter().filter(|c| c.shown && caps::of(c).writable).collect();
    Ok(writable.iter().find(|c| c.is_default).or(writable.first()).cloned())
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

    fn sentence(c: &Calendar, account: &str, e: &EventData, series: bool) -> (bool, Option<String>) {
        let (locks, notice) = access(c, account, e, series);
        (locks.len() == EVERY_FIELD.len(), notice)
    }

    /// The read-only table in section 5, and check 1: who "me" is.
    #[test]
    fn locks_follow_the_read_only_table() {
        let google = calendar(CalendarSource::Google, Access::Owner, ME);
        let alone = EventData { title: "Focus".into(), ..EventData::new("a1", span("2026-11-24 09:00", "2026-11-24 10:00")) };
        assert_eq!(access(&google, ME, &alone, false), (Vec::new(), None), "the user's own event without guests is open");

        for c in [calendar(CalendarSource::Webcal, Access::Read, "feed"), calendar(CalendarSource::Graph, Access::Read, "x"), calendar(CalendarSource::Graph, Access::FreeBusy, "x")] {
            assert_eq!(sentence(&c, ME, &alone, false), (true, Some("This calendar is read-only.".into())), "{:?} {:?}", c.source, c.access);
        }

        let theirs = meeting("jan@example.com", vec![attendee("jan@example.com", false, Role::Chair), attendee(ME, true, Role::Required)]);
        assert_eq!(
            sentence(&google, ME, &theirs, false),
            (true, Some("Jan Peeters organizes this meeting. Jolta can't change meetings with guests yet.".into())),
            "someone else's meeting"
        );

        let mine = meeting(ME, vec![attendee(ME, true, Role::Chair), attendee("els@example.com", false, Role::Required)]);
        assert_eq!(sentence(&google, ME, &mine, false), (true, Some("Jolta can't change meetings with guests yet.".into())), "the user's own meeting with guests");

        let shared = calendar(CalendarSource::CalDav, Access::Write, "https://dav.example.com/cal/team/");
        assert_eq!(access(&shared, ME, &alone, false), (Vec::new(), None), "a shared calendar with write access is open");

        let room_only = EventData { attendees: vec![attendee("room-214@example.com", false, Role::Room)], ..alone.clone() };
        assert_eq!(access(&google, ME, &room_only, false), (Vec::new(), None), "a room isn't a guest");

        assert_eq!(sentence(&google, ME, &alone, true), (true, Some("Jolta can't change repeating events yet.".into())), "a series waits for the repeat editor");
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
        assert_eq!(access(&primary, ME, &organized, false).1, Some("Jolta can't change meetings with guests yet.".into()), "in the primary calendar the marked organizer is the user");
        assert_eq!(
            access(&shared, ME, &organized, false).1,
            Some("Jan Peeters organizes this meeting. Jolta can't change meetings with guests yet.".into()),
            "in a shared calendar the marked organizer is its owner"
        );
        let owners_own = EventData { attendees: vec![attendee(owner, true, Role::Chair)], ..organized };
        assert_eq!(access(&shared, ME, &owners_own, false).0, [DraftField::Title; 0], "the owner's event without guests stays open to write access");
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
        s.write(|tx| store::set_default_calendar(tx, test)).unwrap();
        assert_eq!(s.read(|c| default_calendar(c)).unwrap().map(|c| c.id), Some(test));
        s.write(|tx| store::set_default_calendar(tx, family)).unwrap();
        let defaults: i64 = s.read(|c| Ok(c.query_row("SELECT count(*) FROM calendar WHERE is_default", [], |r| r.get(0))?)).unwrap();
        assert_eq!(defaults, 1, "one default at a time");
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
        let saved = s.write(|tx| save_draft(tx, &d, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(saved.message, "Added Design review");
        let id = event_id(&s, cal, &d.uid);

        let mut d = s.read(|c| draft_for_event(c, id, None, Brussels)).unwrap();
        assert_eq!((d.title.as_str(), d.locks.len(), plan_save(&d).unchanged), ("Design review", 0, true));
        d.span = span("2026-11-24 15:00", "2026-11-24 16:00");
        d.reminder = Reminder::Before(10);
        assert!(!plan_save(&d).unchanged);
        let saved = s.write(|tx| save_draft(tx, &d, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(saved.message, "Saved Design review");
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!((stored.span, stored.alarms.as_slice()), (d.span, [Alarm { trigger: AlarmTrigger::MinutesBefore(10), email: false }].as_slice()));

        let again = s.write(|tx| save_draft(tx, &d, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(again.op, None, "the same save twice queues one op");

        let id = event_id(&s, cal, &d.uid);
        let deleted = s.write(|tx| delete_event(tx, id, NOW, &horizon(), Brussels)).unwrap();
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
        s.write(|tx| save_draft(tx, &d, NOW, &horizon(), Brussels)).unwrap();
        let id = event_id(&s, cal, &d.uid);
        let mut editing = s.read(|c| draft_for_event(c, id, None, Brussels)).unwrap();

        let mut elsewhere = editing.base.clone().unwrap();
        elsewhere.location = "Room 2.14".into();
        elsewhere.title = "Daily standup".into();
        s.write(|tx| ops::queue(tx, &Edit::quiet(cal, Change::Update(elsewhere)), NOW, &horizon())).unwrap();

        editing.span = span("2026-11-24 16:00", "2026-11-24 16:30");
        editing.title = "Standup moved".into();
        let saved = s.write(|tx| save_draft(tx, &editing, NOW, &horizon(), Brussels)).unwrap();
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

    #[test]
    fn a_room_is_a_room_attendee_and_fills_an_empty_location() {
        let (s, cal) = setup("room");
        let mut d = new_draft(&s, cal, "Review");
        d.room = "room-214@example.com".into();
        s.write(|tx| save_draft(tx, &d, NOW, &horizon(), Brussels)).unwrap();
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!(stored.attendees.iter().map(|a| (a.person.address.as_str(), a.role)).collect::<Vec<_>>(), [("room-214@example.com", Role::Room)]);
        assert_eq!(stored.location, "room-214@example.com");
        let mut reopened = s.read(|c| draft_for_event(c, event_id(&s, cal, &d.uid), None, Brussels)).unwrap();
        assert_eq!((reopened.room.as_str(), reopened.location.as_str(), reopened.people.len()), ("room-214@example.com", "", 0), "the location only repeated the room");
        assert!(plan_save(&reopened).unchanged);

        reopened.room = "room-301@example.com".into();
        s.write(|tx| save_draft(tx, &reopened, NOW, &horizon(), Brussels)).unwrap();
        let stored = s.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().event;
        assert_eq!(stored.location, "room-301@example.com", "the location follows the room while the user has none");

        let mut reopened = s.read(|c| draft_for_event(c, event_id(&s, cal, &d.uid), None, Brussels)).unwrap();
        reopened.location = "Gent, Belfort".into();
        s.write(|tx| save_draft(tx, &reopened, NOW, &horizon(), Brussels)).unwrap();
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
        s.write(|tx| save_draft(tx, &d, NOW, &horizon(), Brussels)).unwrap();
        assert_eq!(s.read(|c| ops::stored(c, cal, "d1", None)).unwrap().unwrap().event.description, format!("New notes{block}"));
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
        let mut d = s.read(|c| draft_for_event(c, id, None, Brussels)).unwrap();
        assert_eq!(d.notice.as_deref(), Some("Jolta can't change repeating events yet."));
        d.title = "Changed".into();
        d.locks.clear();
        d.notice = None;
        assert!(s.write(|tx| save_draft(tx, &d, NOW, &horizon(), Brussels)).is_err(), "the store's locks count, not the draft's");
        assert!(s.read(|c| plan_delete(c, id, Brussels)).unwrap().refusal.is_some());
        assert!(s.write(|tx| delete_event(tx, id, NOW, &horizon(), Brussels)).is_err());
    }
}
