//! What the reader shows above a message that carries an invitation: the
//! event in the user's own time, what the user's calendars say about it,
//! and the warning an invitation from a stranger needs. Everything is words
//! and positions; `render` only lays it out.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;

use super::detail::{city, instant_only, offset, range};
use super::invitation::{Invitation, Method};
use super::recur::Source;
use super::series::{self, Horizon};
use super::update;
use super::{Bounds, EventData, EventStatus, MeetingProvider, Moment, Person, Response, Role, Span, Zone, meeting, recur, tz};
use crate::blobs::Blobs;
use crate::model::{AccountId, MessageId};
use crate::{Result, store};

#[derive(Clone, Debug, PartialEq)]
pub enum Card {
    /// A request, cancellation, proposal or shared event.
    Event(EventCard),
    /// An answer to one of the user's own meetings, as one line.
    Reply(ReplyCard),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventCard {
    /// "Invitation", "Cancelled", "New time proposed".
    pub label: &'static str,
    pub title: String,
    pub cancelled: bool,
    /// "Thu 5 Nov · 16:00 to 16:45" in the user's zone; the next
    /// occurrence of a series that has started. None when neither the
    /// message nor the calendar gives a time.
    pub when: Option<String>,
    /// "10:00 to 10:45 New York", when the event's own clock differs.
    pub elsewhere: Option<String>,
    pub repeat: Option<String>,
    pub location: Option<String>,
    pub join: Option<Join>,
    pub organizer: Option<String>,
    /// "Ann, Bo and 4 others".
    pub guests: Option<String>,
    /// What an update changed since the version the user had.
    pub changes: Changes,
    /// A newer version of this invitation replaces it.
    pub outdated: bool,
    pub notes: Vec<Note>,
    /// The user's answer: in their own calendar, else the one they gave
    /// from Jolta for an event no calendar of theirs holds.
    pub answer: Option<Response>,
    pub availability: Option<Availability>,
    /// Set when the user can answer: an invitation to them that they don't
    /// organize.
    pub answering: Option<Answering>,
    /// Set on a cancellation whose meeting the calendar of the account it
    /// came to still holds.
    pub removing: Option<Removing>,
}

/// What Remove from calendar needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Removing {
    pub message: MessageId,
    pub uid: String,
    /// The occurrence cancelled alone, and its day: "Thu 5 Nov".
    pub occurrence: Option<(Moment, String)>,
}

/// What the answer buttons need.
#[derive(Clone, Debug, PartialEq)]
pub struct Answering {
    pub message: MessageId,
    pub uid: String,
    /// For a repeating event, the occurrence the card shows, which "This
    /// occurrence only" answers; for an invitation to one occurrence of a
    /// series, that occurrence.
    pub occurrence: Option<ShownOccurrence>,
    /// "Ann", whom the answer tells.
    pub organizer: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShownOccurrence {
    pub recurrence_id: Moment,
    /// "Thu 5 Nov".
    pub day: String,
    /// The invitation is for this occurrence only, without its series, so
    /// it is answered alone and there is no scope to choose.
    pub alone: bool,
}

/// The lines an update changed, each with its old value, which the card
/// strikes through before the new one: "~~Tue 10 Nov~~ Wed 11 Nov · 14:00
/// to 15:00".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub when: Option<Edit>,
    pub repeat: Option<Edit>,
    pub location: Option<Edit>,
    /// The meeting link: "Zoom" to "Google Meet", or "New Zoom link".
    pub join: Option<Edit>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Changes::default()
    }
}

/// One changed line: `lead`, `old` struck through, `new`, then `tail`, the
/// parts both versions share around what changed. An empty `old` is a line
/// the update added, an empty `new` one it took away.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    pub lead: String,
    pub old: String,
    pub new: String,
    pub tail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    /// "Zoom", "Microsoft Teams".
    pub provider: &'static str,
    pub url: String,
    pub app_url: Option<String>,
    /// From 5 minutes before the start until the end.
    pub live: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    pub warn: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Availability {
    /// "Overlaps with Standup", "Free at this time".
    pub text: String,
    pub conflict: bool,
    pub strip: Option<Strip>,
}

/// A few hours of the user's day around the event, in percent of its width.
#[derive(Clone, Debug, PartialEq)]
pub struct Strip {
    /// Whole hours: (left, "09:00").
    pub hours: Vec<(f32, String)>,
    /// The user's busy events, in lanes so overlapping ones stack.
    pub busy: Vec<Block>,
    pub lanes: u8,
    pub this: Block,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub left: f32,
    pub width: f32,
    pub lane: u8,
    pub title: String,
    pub conflict: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyCard {
    /// "Ann accepted".
    pub line: String,
    pub comment: Option<String>,
    /// "Design review · Thu 5 Nov".
    pub about: String,
}

/// What a card needs besides the invitation.
pub struct Context<'a> {
    /// The message the invitation came in, and the account it came to.
    pub message: MessageId,
    pub account: AccountId,
    /// The user's own addresses.
    pub me: &'a [String],
    /// The mail's From address, for an invitation without an organizer.
    pub sender: &'a str,
    pub now: DateTime<Utc>,
    pub here: Tz,
    /// Stored messages, for the other versions of an invitation.
    pub blobs: &'a Blobs,
}

/// One card per event the message's calendar parts name, a series with its
/// changed occurrences being one event.
pub fn cards(conn: &Connection, invitations: &[Invitation], cx: &Context) -> Result<Vec<Card>> {
    let mut out = Vec::new();
    for (_, events) in series::group(invitations.iter().map(|i| i.event.clone()).collect()) {
        let sent = events.iter().find(|e| e.recurrence_id.is_none()).unwrap_or(&events[0]);
        let i = invitations.iter().find(|i| i.event.uid == sent.uid && i.event.recurrence_id == sent.recurrence_id).expect("grouped from these");
        // Without a start the message only names its event; the calendar's
        // copy, when there is one, says when and where it is.
        let copy;
        let described: &[EventData] = if i.dated {
            &events
        } else {
            copy = stored_copy(conn, sent, cx)?;
            &copy
        };
        out.push(match i.method {
            Method::Reply => Card::Reply(reply(sent, described.iter().find(|e| e.recurrence_id.is_none()).or(described.first()), cx)),
            _ => Card::Event(event(conn, i, &events, described, cx)?),
        });
    }
    Ok(out)
}

/// What the calendar says about the event an undated message names: the
/// whole series, so the next occurrence is chosen as for a dated message,
/// or for one named occurrence its override, else the master moved to it.
fn stored_copy(conn: &Connection, sent: &EventData, cx: &Context) -> Result<Vec<EventData>> {
    let series = store::stored_series(conn, &sent.uid, None)?;
    let Some(rid) = sent.recurrence_id else { return Ok(series) };
    Ok(series::occurrence(&series, &rid, cx.here).into_iter().collect())
}

/// `sent` is the invitation the card is about, the series or single event
/// of its message; `message` holds the events the mail sends, which say who
/// asked and which version it is; `described` the ones that give the time
/// and place: the same events, the calendar's copy, or none.
fn event(conn: &Connection, invitation: &Invitation, message: &[EventData], described: &[EventData], cx: &Context) -> Result<EventCard> {
    let method = invitation.method;
    let master = described.iter().find(|e| e.recurrence_id.is_none());
    let occurrence = (!described.is_empty()).then(|| next(master, described, cx));
    let repeats = master.is_some_and(|m| m.recurrence.is_some());
    let sent = &invitation.event;
    let shown = occurrence.map_or(sent, |(e, _)| e);
    let said = message.iter().find(|e| e.recurrence_id == shown.recurrence_id).unwrap_or(sent);
    let bounds = occurrence.map(|(_, span)| span.bounds(cx.here));
    let (when, elsewhere) = match occurrence {
        Some((e, span)) => {
            let (when, elsewhere) = when(&span, e, cx);
            (Some(when), elsewhere)
        }
        None => (None, None),
    };
    // The calendar may hold the occurrence shown as an override the message
    // doesn't have, with its own answer; the series' copy answers when it
    // doesn't, as in the list.
    let at = match occurrence {
        Some((e, span)) if repeats && e.recurrence_id.is_none() => Some(span.start()),
        _ => shown.recurrence_id,
    };
    let copy = |account: Option<AccountId>| -> Result<Option<store::StoredEvent>> {
        Ok(match store::stored_event(conn, &shown.uid, at.as_ref(), account, cx.here)? {
            None if at.is_some() && master.is_some() => store::stored_event(conn, &shown.uid, None, account, cx.here)?,
            found => found,
        })
    };
    let stored = copy(None)?;
    let seen = update::Seen { conn, blobs: cx.blobs, account: cx.account, message: cx.message, sender: cx.sender, me: cx.me, here: cx.here };
    let history = update::history(&seen, invitation)?;
    // A series' message shows one occurrence; the account's calendar may
    // hold that occurrence cancelled, which the list counts as the card does.
    let occurrence_cancelled = sent.recurrence_id.is_none()
        && at.is_some()
        && store::stored_event(conn, &shown.uid, at.as_ref(), Some(cx.account), cx.here)?.is_some_and(|c| update::cancels(&c, sent.sequence));
    let newer = history.newer.or(occurrence_cancelled.then_some(Method::Cancel));
    // An older version only points to the newer one: its time, link and
    // answers are out of date.
    let current = newer.is_none();
    // What changed matters to an update, not to a cancellation.
    let update = current && matches!(method, Method::Request | Method::Add);
    let changes = if update { changes(&history.before, described, cx) } else { Changes::default() };

    let mut notes = Vec::new();
    match method {
        Method::Cancel => notes.push(Note { text: "Cancelled by the organizer".into(), warn: true }),
        Method::Counter => notes.push(Note { text: with_comment(format!("{} proposed this new time", first_attendee(said)), said), warn: false }),
        Method::DeclineCounter => notes.push(Note { text: "The organizer kept the original time".into(), warn: false }),
        Method::Refresh => notes.push(Note { text: format!("{} asked for the latest version", first_attendee(said)), warn: false }),
        Method::Add => notes.push(Note { text: "Adds dates to a repeating event".into(), warn: false }),
        Method::Request | Method::Publish | Method::Reply => {}
    }
    match newer {
        Some(Method::Cancel) => notes.push(Note { text: "The organizer has since cancelled this meeting".into(), warn: false }),
        Some(_) => notes.push(Note { text: "There's a newer version of this invitation".into(), warn: false }),
        None if update && changes.is_empty() => notes.extend(retitled(&history.before, sent)),
        None => {}
    }
    if history.not_from_organizer {
        notes.push(Note { text: "This isn't from the meeting's organizer".into(), warn: true });
    }
    if method != Method::Cancel && stranger(conn, said, cx)? {
        notes.push(Note { text: "You haven't written to this sender before".into(), warn: true });
    }

    let receiver = store::account(conn, cx.account)?.address;
    let alone = sent.recurrence_id.is_some();
    // Another sender's message about the meeting answers nothing in it.
    let trusted = !history.not_from_organizer;
    let answering = (current && trusted && answerable(method, sent, &receiver)).then(|| Answering {
        message: cx.message,
        uid: sent.uid.clone(),
        occurrence: occurrence.filter(|_| repeats || alone).map(|(e, span)| ShownOccurrence {
            recurrence_id: e.recurrence_id.unwrap_or(span.start()),
            day: match span.bounds(cx.here) {
                Bounds::Utc { start, .. } => date(start.with_timezone(&cx.here).date_naive(), cx),
                Bounds::Days { start, .. } => date(day(start), cx),
            },
            alone,
        }),
        organizer: sent.organizer.as_ref().map_or_else(String::new, |o| name(o, &[], true)),
    });
    // An invitation the user answers is answered in the calendars of the
    // account it came to; another account's copy may be the organizer's own.
    let (remembered, answered) = match &answering {
        Some(a) => (store::remembered_answer(conn, cx.account, &a.uid, a.occurrence.as_ref().map(|o| &o.recurrence_id))?, copy(Some(cx.account))?),
        None => (None, stored.clone()),
    };

    let held = method == Method::Cancel && current && held(conn, sent, cx)?;
    let removing = if held && trusted { removable(conn, sent, cx)? } else { None };
    if method == Method::Cancel && current && !held && store::account_calendar_on(conn, cx.account)? {
        notes.push(Note { text: "Not in your calendar".into(), warn: false });
    }

    let wants_time = matches!(method, Method::Request | Method::Add | Method::Publish | Method::Counter);
    let availability = match bounds {
        Some(bounds) if current && wants_time && ends(bounds, cx) > cx.now => availability(conn, &shown.uid, bounds, &shown.title, cx)?,
        _ => None,
    };

    // The answer picked as for the list. A message that isn't the
    // organizer's says nothing about it.
    let answer = match current && trusted && method != Method::Cancel {
        true => store::shown_answer(answered.as_ref(), remembered.as_ref(), Some((message, at.as_ref())), cx.here),
        false => None,
    };

    Ok(EventCard {
        label: label(method, !history.before.is_empty()),
        title: title(method, shown, &history.before),
        cancelled: method == Method::Cancel || shown.status == EventStatus::Cancelled,
        when,
        elsewhere,
        repeat: repeat(master),
        location: location(shown),
        join: join(shown, bounds, cx.now).filter(|_| current),
        organizer: shown.organizer.as_ref().map(|o| name(o, cx.me, false)),
        guests: guests(shown, cx.me),
        changes,
        outdated: !current,
        notes,
        answer,
        availability,
        answering,
        removing,
    })
}

/// Whether the account's calendar still holds the cancelled event, or the
/// occurrence it cancels, and hasn't cancelled it there yet.
fn held(conn: &Connection, sent: &EventData, cx: &Context) -> Result<bool> {
    let live = |e: &EventData| e.status != EventStatus::Cancelled;
    let copy = |rid: Option<&Moment>| store::stored_event(conn, &sent.uid, rid, Some(cx.account), cx.here);
    Ok(match &sent.recurrence_id {
        None => copy(None)?.is_some_and(|s| live(&s.event)),
        Some(rid) => match copy(Some(rid))? {
            Some(changed) => live(&changed.event),
            None => copy(None)?.is_some_and(|m| live(&m.event) && occurs(&m.event, rid, cx.here)),
        },
    })
}

/// Remove from calendar for a cancellation the calendar holds, when its
/// copy is the user's as a guest.
fn removable(conn: &Connection, sent: &EventData, cx: &Context) -> Result<Option<Removing>> {
    let guest = super::remove::refusal(conn, &sent.uid, sent.recurrence_id.as_ref(), cx.account, cx.here)?.is_none();
    Ok(guest.then(|| Removing {
        message: cx.message,
        uid: sent.uid.clone(),
        occurrence: sent.recurrence_id.map(|rid| {
            let day = match rid {
                Moment::Date(d) => d,
                Moment::Time(t) => t.instant(cx.here).with_timezone(&cx.here).date_naive(),
            };
            (rid, date(day, cx))
        }),
    }))
}

/// Whether the series makes an occurrence at `rid`, one its EXDATEs don't
/// take out.
fn occurs(master: &EventData, rid: &Moment, here: Tz) -> bool {
    let Some(span) = series::occurrence_span(&master.span, rid, here) else { return false };
    let (start, end) = match span.bounds(here) {
        Bounds::Utc { start, end } => (start, end),
        Bounds::Days { start, end } => (tz::instant(day(start).and_time(chrono::NaiveTime::MIN), here), tz::instant(day(end).and_time(chrono::NaiveTime::MIN), here)),
    };
    let window = recur::Window { start: start - Duration::days(1), end: end + Duration::days(1) };
    series::place(Some(master), &[], &Horizon { window, here }).iter().any(|o| o.source == Source::Master && o.span.start().names(rid, here))
}

/// The event's title. Exchange writes its cancellation's as "Canceled:
/// Design review", in the organizer's language; the request before it
/// gives the title itself.
fn title(method: Method, shown: &EventData, before: &[EventData]) -> String {
    let own = shown.title.trim();
    let earlier = before.iter().find(|e| e.recurrence_id == shown.recurrence_id).or(before.first()).map(|e| e.title.trim()).filter(|t| !t.is_empty());
    match earlier {
        Some(t) if method == Method::Cancel && own.len() > t.len() && own.ends_with(t) => t.to_string(),
        _ if own.is_empty() => "(no title)".into(),
        _ => own.to_string(),
    }
}

/// "The organizer changed the title": an update that changed nothing the
/// card shows besides the title or the description.
fn retitled(before: &[EventData], sent: &EventData) -> Option<Note> {
    let old = before.iter().find(|e| e.recurrence_id == sent.recurrence_id).or(before.first())?;
    let what = match (old.title.trim() != sent.title.trim(), old.description.trim() != sent.description.trim()) {
        (true, true) => "the title and the description",
        (true, false) => "the title",
        (false, true) => "the description",
        (false, false) => return None,
    };
    Some(Note { text: format!("The organizer changed {what}"), warn: false })
}

/// The lines of a card an update can change, for one version.
struct Lines {
    /// "Thu 5 Nov" for a timed occurrence within one day.
    date: Option<String>,
    /// "16:00 to 16:45", or the whole span when it has no date of its own.
    time: String,
    repeat: Option<String>,
    location: Option<String>,
    meeting: Option<(&'static str, String)>,
}

impl Lines {
    fn of(events: &[EventData], cx: &Context) -> Option<Lines> {
        if events.is_empty() {
            return None;
        }
        let master = events.iter().find(|e| e.recurrence_id.is_none());
        let (e, span) = next(master, events, cx);
        let (date, time) = when_parts(&span, cx);
        Some(Lines {
            date,
            time,
            repeat: repeat(master),
            location: location(e),
            meeting: e.meeting.as_ref().map(|m| (provider_name(m.provider), m.join_url.clone())),
        })
    }

    fn when(&self) -> String {
        match &self.date {
            Some(d) => format!("{d} · {}", self.time),
            None => self.time.clone(),
        }
    }
}

/// What changed from `before` to `now`, line by line. The time line keeps
/// what both share: a new time on the same day strikes only the time.
fn changes(before: &[EventData], now: &[EventData], cx: &Context) -> Changes {
    let (Some(old), Some(new)) = (Lines::of(before, cx), Lines::of(now, cx)) else { return Changes::default() };
    let when = match (&old.date, &new.date) {
        _ if old.when() == new.when() => None,
        (Some(a), Some(b)) if a == b => Some(Edit { lead: format!("{b} · "), old: old.time.clone(), new: new.time.clone(), tail: String::new() }),
        (Some(a), Some(b)) if old.time == new.time => Some(Edit { lead: String::new(), old: a.clone(), new: b.clone(), tail: format!(" · {}", new.time) }),
        _ => Some(Edit { old: old.when(), new: new.when(), ..Edit::default() }),
    };
    let text = |old: Option<String>, new: Option<String>| (old != new).then(|| Edit { old: old.unwrap_or_default(), new: new.unwrap_or_default(), ..Edit::default() });
    let join = match (old.meeting, new.meeting) {
        (Some((_, a)), Some((_, b))) if a == b => None,
        (Some((a, _)), Some((b, _))) if a == b => Some(Edit { new: format!("New {b} link"), ..Edit::default() }),
        (None, None) => None,
        (a, b) => text(a.map(|(p, _)| p.to_string()), b.map(|(p, _)| p.to_string())),
    };
    Changes { when, repeat: text(old.repeat, new.repeat), location: text(old.location, new.location), join }
}

fn repeat(master: Option<&EventData>) -> Option<String> {
    master.and_then(|m| m.recurrence.as_ref().map(|r| recur::describe(r, &m.span).unwrap_or_else(|| "Custom repeat".into())))
}

/// An invitation the user can answer: a request, or an event added or
/// shared that names them, from an organizer other than the account it came
/// to. One of the user's accounts can invite another.
fn answerable(method: Method, said: &EventData, receiver: &str) -> bool {
    let Some(organizer) = &said.organizer else { return false };
    if organizer.address.trim().eq_ignore_ascii_case(receiver) {
        return false;
    }
    match method {
        Method::Request => true,
        Method::Add | Method::Publish => said.my_response().is_some(),
        Method::Cancel | Method::Reply | Method::Counter | Method::DeclineCounter | Method::Refresh => false,
    }
}

fn ends(bounds: Bounds, cx: &Context) -> DateTime<Utc> {
    match bounds {
        Bounds::Utc { end, .. } => end,
        Bounds::Days { end, .. } => tz::instant(day(end).and_time(chrono::NaiveTime::MIN), cx.here),
    }
}

/// `updated` when the user has an earlier version. SEQUENCE alone doesn't
/// say it: Exchange sends its first request at SEQUENCE 1.
fn label(method: Method, updated: bool) -> &'static str {
    match method {
        Method::Request if updated => "Updated invitation",
        Method::Request | Method::Add => "Invitation",
        Method::Publish => "Event",
        Method::Cancel => "Cancelled",
        Method::Counter => "New time proposed",
        Method::DeclineCounter => "Proposal declined",
        Method::Refresh => "Update requested",
        Method::Reply => "Reply",
    }
}

pub fn answer_word(r: Response) -> &'static str {
    match r {
        Response::Accepted => "Accepted",
        Response::Tentative => "Maybe",
        Response::Declined => "Declined",
        Response::Delegated => "Delegated",
        Response::NeedsAction => "Not answered",
    }
}

/// `about` is the event the answer is about, when the reply or the
/// calendar says when it is.
fn reply(e: &EventData, about: Option<&EventData>, cx: &Context) -> ReplyCard {
    let who = e.attendees.first();
    let verb = match who.map(|a| a.response) {
        Some(Response::Accepted) => "accepted",
        Some(Response::Tentative) => "said maybe",
        Some(Response::Declined) => "declined",
        Some(Response::Delegated) => "delegated",
        Some(Response::NeedsAction) | None => "replied",
    };
    let name = who.map_or_else(|| "Someone".into(), |a| name(&a.person, cx.me, false));
    let day = about.map(|a| match a.span.bounds(cx.here) {
        Bounds::Utc { start, .. } => date(start.with_timezone(&cx.here).date_naive(), cx),
        Bounds::Days { start, .. } => date(self::day(start), cx),
    });
    let title = about.into_iter().chain([e]).map(|a| a.title.trim()).find(|t| !t.is_empty());
    ReplyCard {
        line: format!("{name} {verb}"),
        comment: who.map(|a| a.comment.trim().to_string()).filter(|c| !c.is_empty()),
        about: match (title, day) {
            (Some(title), Some(day)) => format!("{title} · {day}"),
            (Some(title), None) => title.to_string(),
            (None, Some(day)) => day,
            (None, None) => "(no title)".into(),
        },
    }
}

/// The occurrence a card is about, with the event that describes it: the
/// event itself, or for a series the one `series::shown_occurrence` picks,
/// as the list does. A changed occurrence brings its own place, link and
/// guests.
fn next<'a>(master: Option<&'a EventData>, events: &'a [EventData], cx: &Context) -> (&'a EventData, Span) {
    let Some(m) = master.filter(|m| m.recurrence.is_some()) else {
        let e = master.unwrap_or(&events[0]);
        return (e, e.span);
    };
    let overrides: Vec<&EventData> = events.iter().filter(|e| e.recurrence_id.is_some()).collect();
    let owned: Vec<EventData> = overrides.iter().map(|&e| e.clone()).collect();
    match series::shown_occurrence(m, &owned, cx.now, cx.here) {
        None => (m, m.span),
        Some(o) => match o.source {
            Source::Master => (m, o.span),
            Source::Override(i) => (overrides[i], o.span),
        },
    }
}

/// "Thu 5 Nov · 16:00 to 16:45" here, and the event's own clock when it
/// differs: "10:00 to 10:45 New York".
fn when(span: &Span, e: &EventData, cx: &Context) -> (String, Option<String>) {
    let text = match when_parts(span, cx) {
        (Some(date), time) => format!("{date} · {time}"),
        (None, text) => text,
    };
    match span.bounds(cx.here) {
        Bounds::Days { .. } => (text, None),
        Bounds::Utc { start, end } => {
            let wall = |t: DateTime<Utc>, tz: Tz| t.with_timezone(&tz).naive_local();
            let own = match e.span {
                Span::Timed { start: own, .. } => match own.zone {
                    Zone::Tz(tz) if !instant_only(tz) && offset(tz, start) != offset(cx.here, start) => Some(tz),
                    _ => None,
                },
                Span::AllDay { .. } => None,
            };
            let elsewhere = own.map(|tz| format!("{} {}", range(wall(start, tz), wall(end, tz)), city(tz)));
            (text, elsewhere)
        }
    }
}

/// The day and the time of a timed span within one day here, ("Thu 5 Nov",
/// "16:00 to 16:45"); any other span as one text: ("Thu 5 Nov, all day").
fn when_parts(span: &Span, cx: &Context) -> (Option<String>, String) {
    match span.bounds(cx.here) {
        Bounds::Days { start, end } => {
            let (first, last) = (day(start), day((end - 1).max(start)));
            let text = if first == last {
                format!("{}, all day", date(first, cx))
            } else if first.month() == last.month() && first.year() == last.year() {
                format!("{} to {}", first.format("%a %-d"), date(last, cx))
            } else {
                format!("{} to {}", date(first, cx), date(last, cx))
            };
            (None, text)
        }
        Bounds::Utc { start, end } => {
            let here = (start.with_timezone(&cx.here).naive_local(), end.with_timezone(&cx.here).naive_local());
            if crosses_days(here.0, here.1) { (None, range(here.0, here.1)) } else { (Some(date(here.0.date(), cx)), range(here.0, here.1)) }
        }
    }
}

fn crosses_days(start: NaiveDateTime, end: NaiveDateTime) -> bool {
    let next_midnight = (start.date() + Duration::days(1)).and_time(chrono::NaiveTime::MIN);
    end.date() != start.date() && end != next_midnight
}

/// "Thu 5 Nov", with the year when it isn't this year.
fn date(d: NaiveDate, cx: &Context) -> String {
    if d.year() == cx.now.with_timezone(&cx.here).year() { d.format("%a %-d %b").to_string() } else { d.format("%a %-d %b %Y").to_string() }
}

fn day(n: i32) -> NaiveDate {
    NaiveDate::default() + Duration::days(n.into())
}

/// The place, unless it only names the meeting: its link, or Outlook's
/// "Microsoft Teams Meeting". A place written over several lines keeps its
/// first, the name.
fn location(e: &EventData) -> Option<String> {
    let first = e.location.lines().map(str::trim).find(|l| !l.is_empty())?;
    let lower = first.to_lowercase();
    let is_meeting = lower.starts_with("https://")
        || e.meeting.as_ref().is_some_and(|m| first.contains(&m.join_url) || lower.contains(&provider_name(m.provider).to_lowercase()));
    (!is_meeting).then(|| first.to_string())
}

/// Join for a known provider only: an unknown link from a stranger's
/// invitation stays in the mail, where opening it shows where it goes.
fn join(e: &EventData, bounds: Option<Bounds>, now: DateTime<Utc>) -> Option<Join> {
    let m = e.meeting.as_ref().filter(|m| m.provider != MeetingProvider::Other)?;
    let live = match bounds {
        Some(Bounds::Utc { start, end }) => now >= start - Duration::minutes(5) && now < end,
        Some(Bounds::Days { .. }) | None => false,
    };
    Some(Join { provider: provider_name(m.provider), url: m.join_url.clone(), app_url: meeting::app_url(m), live })
}

fn provider_name(p: MeetingProvider) -> &'static str {
    match p {
        MeetingProvider::Zoom => "Zoom",
        MeetingProvider::Teams => "Microsoft Teams",
        MeetingProvider::Meet => "Google Meet",
        MeetingProvider::Webex => "Webex",
        MeetingProvider::GoTo => "GoTo Meeting",
        MeetingProvider::Whereby => "Whereby",
        MeetingProvider::Jitsi => "Jitsi",
        MeetingProvider::Chime => "Amazon Chime",
        MeetingProvider::Slack => "Slack huddle",
        MeetingProvider::FaceTime => "FaceTime",
        MeetingProvider::RingCentral => "RingCentral",
        MeetingProvider::Zoho => "Zoho Meeting",
        MeetingProvider::Other => "Online meeting",
    }
}

fn is_me(address: &str, me: &[String]) -> bool {
    me.iter().any(|m| m.eq_ignore_ascii_case(address.trim()))
}

/// The name a person goes by, "You" for the user. `short` keeps the first
/// word of a name.
fn name(p: &Person, me: &[String], short: bool) -> String {
    if is_me(&p.address, me) {
        return "You".into();
    }
    match p.name.trim() {
        "" => p.address.clone(),
        n if short => n.split_whitespace().next().unwrap_or(n).to_string(),
        n => n.to_string(),
    }
}

fn first_attendee(e: &EventData) -> String {
    e.attendees.first().map_or_else(|| "A guest".into(), |a| name(&a.person, &[], false))
}

fn with_comment(text: String, e: &EventData) -> String {
    match e.attendees.first().map(|a| a.comment.trim()).filter(|c| !c.is_empty()) {
        Some(c) => format!("{text}: \"{c}\""),
        None => text,
    }
}

/// The other people, by first name: "Ann, Bo and 4 others". Rooms are the
/// location, the organizer has a line of their own and the user knows.
fn guests(e: &EventData, me: &[String]) -> Option<String> {
    let organizer = e.organizer.as_ref().map(|o| o.address.as_str()).unwrap_or("");
    let names: Vec<String> = e
        .attendees
        .iter()
        .filter(|a| !matches!(a.role, Role::Room | Role::Resource) && !a.is_me && !is_me(&a.person.address, me))
        .filter(|a| !a.person.address.eq_ignore_ascii_case(organizer))
        .map(|a| name(&a.person, me, true))
        .collect();
    match names.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        [a, b] => Some(format!("{a} and {b}")),
        [a, b, c] => Some(format!("{a}, {b} and {c}")),
        [a, b, rest @ ..] => Some(format!("{a}, {b} and {} others", rest.len())),
    }
}

/// An invitation from an address the user never wrote to. The organizer
/// counts, or the sender of the mail when there is none.
fn stranger(conn: &Connection, e: &EventData, cx: &Context) -> Result<bool> {
    let address = e.organizer.as_ref().map_or(cx.sender, |o| o.address.as_str());
    if address.trim().is_empty() || is_me(address, cx.me) {
        return Ok(false);
    }
    Ok(!store::has_written_to(conn, address)?)
}

/// Hours shown around the event in the strip, before and after.
const STRIP_MARGIN: i64 = 2;
/// Longer events get the sentence only.
const STRIP_MAX_HOURS: i64 = 8;

/// What the user's shown calendars hold at the event's time. None when the
/// user shows no calendar, so Jolta can't know.
fn availability(conn: &Connection, uid: &str, bounds: Bounds, title: &str, cx: &Context) -> Result<Option<Availability>> {
    let shows_any: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM calendar WHERE shown)", [], |r| r.get(0))?;
    if !shows_any {
        return Ok(None);
    }
    let (from, to) = match bounds {
        Bounds::Utc { start, end } => {
            let local = |t: DateTime<Utc>| t.with_timezone(&cx.here).date_naive();
            (local(start - Duration::hours(STRIP_MARGIN)), local(end + Duration::hours(STRIP_MARGIN)) + Duration::days(1))
        }
        Bounds::Days { start, end } => (day(start), day(end)),
    };
    let rows: Vec<store::OccurrenceRow> =
        store::occurrences(conn, from, to, cx.here)?.into_iter().filter(|o| o.busy && o.uid != uid).collect();
    let overlaps = |o: &store::OccurrenceRow| match (o.bounds, bounds) {
        (Bounds::Utc { start: s, end: e }, Bounds::Utc { start, end }) => s < end && e > start,
        (Bounds::Days { start: s, end: e }, Bounds::Days { start, end }) => s < end && e > start,
        (Bounds::Days { start: s, end: e }, Bounds::Utc { start, end }) => {
            let local = |t: DateTime<Utc>| tz::day_number(t.with_timezone(&cx.here).date_naive());
            s <= local(end - Duration::seconds(1)) && e > local(start)
        }
        // A timed meeting doesn't make a whole day busy.
        (Bounds::Utc { .. }, Bounds::Days { .. }) => false,
    };
    let titles: Vec<String> = rows
        .iter()
        .filter(|o| overlaps(o))
        .map(|o| if o.title.trim().is_empty() { "a busy event".to_string() } else { o.title.trim().to_string() })
        .collect();
    let text = match titles.as_slice() {
        [] => "Free at this time".to_string(),
        [one] => format!("Overlaps with {one}"),
        [a, b] => format!("Overlaps with {a} and {b}"),
        [a, rest @ ..] => format!("Overlaps with {a} and {} more", rest.len()),
    };
    let strip = match bounds {
        Bounds::Utc { start, end } if end - start <= Duration::hours(STRIP_MAX_HOURS) => {
            Some(strip(start, end, title, &rows, cx.here))
        }
        _ => None,
    };
    Ok(Some(Availability { conflict: !titles.is_empty(), text, strip }))
}

/// The hours around the event, from a whole hour before to a whole hour
/// after, with the user's timed busy events placed in lanes.
fn strip(start: DateTime<Utc>, end: DateTime<Utc>, title: &str, rows: &[store::OccurrenceRow], here: Tz) -> Strip {
    // On the timeline, not the wall clock: the hour the clocks repeat in
    // autumn would otherwise round back to its first pass, forever.
    let floor = |t: DateTime<Utc>| t - Duration::seconds(i64::from(t.with_timezone(&here).num_seconds_from_midnight() % 3600));
    let ceil = |t: DateTime<Utc>| if floor(t) < t { floor(t) + Duration::hours(1) } else { t };
    let (from, to) = (floor(start - Duration::hours(STRIP_MARGIN)), ceil(end + Duration::hours(STRIP_MARGIN)));
    let width = (to - from).num_seconds().max(1) as f32;
    let at = |t: DateTime<Utc>| ((t - from).num_seconds() as f32 / width * 100.0).clamp(0.0, 100.0);
    let block = |s: DateTime<Utc>, e: DateTime<Utc>, title: &str, lane: u8, conflict: bool| Block {
        left: at(s),
        width: at(e) - at(s),
        lane,
        title: title.trim().to_string(),
        conflict,
    };

    let mut hours = Vec::new();
    let mut t = from;
    while t <= to {
        hours.push((at(t), t.with_timezone(&here).format("%H:%M").to_string()));
        t = floor(t + Duration::hours(1));
    }

    let mut lane_ends: Vec<DateTime<Utc>> = Vec::new();
    let mut busy = Vec::new();
    for o in rows {
        let Bounds::Utc { start: s, end: e } = o.bounds else { continue };
        if e <= from || s >= to {
            continue;
        }
        let lane = match lane_ends.iter().position(|&last| last <= s) {
            Some(i) => i,
            None => {
                lane_ends.push(s);
                lane_ends.len() - 1
            }
        };
        lane_ends[lane] = e;
        busy.push(block(s, e, &o.title, lane as u8, s < end && e > start));
    }
    Strip { hours, lanes: lane_ends.len().max(1) as u8, busy, this: block(start, end, title, 0, false) }
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::invitation::tests::{ME, import, jolta, mail};
    use crate::calendar::{CalendarId, CalendarSource, Local, invitation};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    /// The From address of a raw message.
    fn from_of(raw: &[u8]) -> String {
        let msg = mail_parser::MessageParser::default().parse(raw);
        msg.and_then(|m| m.from().and_then(|f| f.first()).and_then(|a| a.address()).map(str::to_string)).unwrap_or_default()
    }

    fn cards_of(j: &crate::Jolta, raw: &[u8]) -> Vec<Card> {
        let me = [ME.to_string()];
        let found = invitation::find(raw, &me);
        let sender = from_of(raw);
        let cx = Context { message: MessageId(1), account: AccountId(1), me: &me, sender: &sender, now: now(), here: Brussels, blobs: &j.blobs };
        j.store.read(|c| cards(c, &found, &cx)).unwrap()
    }

    fn event_card(j: &crate::Jolta, raw: &[u8]) -> EventCard {
        match cards_of(j, raw).remove(0) {
            Card::Event(e) => e,
            other => panic!("{other:?}"),
        }
    }

    fn busy(j: &crate::Jolta, calendar: CalendarId, uid: &str, title: &str, start: &str, minutes: i64) {
        let wall = NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M").unwrap();
        let start = Local { wall, zone: Zone::Tz(Brussels) };
        let span = Span::Timed { start, end: Local { wall: wall + Duration::minutes(minutes), ..start } };
        let e = EventData { title: title.into(), ..EventData::new(uid, span) };
        let horizon = Horizon::around(now(), Brussels);
        j.store.write(|tx| store::apply_item(tx, calendar, uid, None, &[e], None, &horizon)).unwrap();
    }

    fn notes(c: &EventCard) -> Vec<&str> {
        c.notes.iter().map(|n| n.text.as_str()).collect()
    }

    #[test]
    fn a_google_invitation_with_a_conflict() {
        let (j, calendar) = jolta("card-google");
        busy(&j, calendar, "standup", "Standup", "2026-11-12 15:30", 45);
        busy(&j, calendar, "lunch", "Lunch", "2026-11-12 12:00", 60);
        let c = event_card(&j, mail!("google-request.eml"));
        assert_eq!(c.label, "Invitation");
        assert_eq!(c.title, "Roadmap <review> & budget");
        assert_eq!(c.when.as_deref(), Some("Thu 12 Nov · 16:00 to 16:45"));
        assert_eq!(c.elsewhere, None, "a time written in UTC has no zone of its own");
        assert_eq!(c.location.as_deref(), Some("Room 2.04 (8)"));
        assert_eq!(c.organizer.as_deref(), Some("Ann Dupont"));
        assert_eq!(c.guests.as_deref(), Some("Bo, Chris and 2 others"), "without the organizer, the user and the room");
        let join = c.join.as_ref().unwrap();
        assert_eq!((join.provider, join.url.as_str(), join.live), ("Google Meet", "https://meet.google.com/abc-defg-hij", false));
        assert_eq!(notes(&c), ["You haven't written to this sender before"]);
        assert!(c.notes[0].warn);
        assert_eq!(c.answer, None, "not in a calendar yet");
        let a = c.availability.unwrap();
        assert_eq!((a.text.as_str(), a.conflict), ("Overlaps with Standup", true));
        let strip = a.strip.unwrap();
        let hours: Vec<&str> = strip.hours.iter().map(|(_, h)| h.as_str()).collect();
        assert_eq!(hours, ["14:00", "15:00", "16:00", "17:00", "18:00", "19:00"]);
        assert_eq!(strip.busy.len(), 1, "lunch is outside the strip");
        assert!(strip.busy[0].conflict && strip.busy[0].title == "Standup");
        assert_eq!((strip.this.left, strip.this.width), (40.0, 15.0));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn the_calendars_copy_gives_the_answer_and_isnt_its_own_conflict() {
        let (j, calendar) = jolta("card-answer");
        let mut copy = invitation::find(mail!("google-request.eml"), &[ME.into()]).remove(0).event;
        copy.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = Response::Accepted);
        j.store.write(|tx| store::apply_item(tx, calendar, "g1", None, &[copy], None, &Horizon::around(now(), Brussels))).unwrap();
        j.store
            .write(|tx| {
                tx.execute("INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, date) VALUES (9, 1, 's', 9, 'me@example.com', 'ann@example.com', 0)", [])?;
                tx.execute("INSERT INTO message_fts (rowid, sender, recipients) VALUES (9, ' me@example.com', 'ann@example.com ')", [])?;
                Ok(())
            })
            .unwrap();
        let c = event_card(&j, mail!("google-request.eml"));
        assert_eq!(c.answer, Some(Response::Accepted));
        assert_eq!(c.availability.as_ref().unwrap().text, "Free at this time");
        assert!(notes(&c).is_empty(), "the user wrote to the organizer");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn outlook_from_another_zone_and_icloud() {
        let (j, _) = jolta("card-outlook");
        let c = event_card(&j, mail!("outlook-request.eml"));
        assert_eq!(c.when.as_deref(), Some("Mon 16 Nov · 16:00 to 17:00"));
        assert_eq!(c.elsewhere.as_deref(), Some("10:00 to 11:00 New York"));
        assert_eq!(c.location, None, "\"Microsoft Teams Meeting\" is the link, not a place");
        let join = c.join.unwrap();
        assert_eq!(join.provider, "Microsoft Teams");
        assert!(join.app_url.unwrap().starts_with("msteams:/l/meetup-join/"));
        assert_eq!(c.guests.as_deref(), Some("Robin"));

        let icloud = event_card(&j, mail!("icloud-request.eml"));
        assert_eq!(icloud.when.as_deref(), Some("Fri 6 Nov · 12:00 to 13:00"));
        assert_eq!(icloud.location.as_deref(), Some("Grote Markt"), "the name, not the address under it");
        assert_eq!(icloud.organizer.as_deref(), Some("Pat Morgan"));
        assert_eq!(icloud.guests, None, "only the organizer and the user");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_cancellation_and_a_reply() {
        let (j, _) = jolta("card-cancel");
        let c = event_card(&j, mail!("cancel.eml"));
        assert_eq!((c.label, c.cancelled), ("Cancelled", true));
        assert_eq!(notes(&c), ["Cancelled by the organizer", "Not in your calendar"], "the account's calendar is on");
        assert_eq!((c.availability, c.removing), (None, None));

        let Card::Reply(r) = cards_of(&j, mail!("reply.eml")).remove(0) else { panic!("a reply") };
        assert_eq!(r, ReplyCard { line: "Bo Nilsson accepted".into(), comment: Some("See you there".into()), about: "Design crit · Fri 13 Nov".into() });
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A one-event REQUEST mail whose VEVENT carries `lines`.
    fn request(lines: &str) -> String {
        format!(
            "Subject: s\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
             UID:u1\r\nSUMMARY:Sync\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\n{lines}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// An iTIP mail without DTSTART, as RFC 5546 has REFRESH and allows for
    /// CANCEL, REPLY and DECLINECOUNTER.
    fn undated(method: &str, uid: &str, lines: &str) -> String {
        format!(
            "Subject: s\r\nContent-Type: text/calendar; method={method}\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\n\
             UID:{uid}\r\nDTSTAMP:20261104T080000Z\r\nORGANIZER;CN=Ann Dupont:mailto:ann@example.com\r\n{lines}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    #[test]
    fn a_message_without_a_start_takes_the_event_from_the_calendar() {
        let (j, calendar) = jolta("card-undated");
        let copy = invitation::find(mail!("google-request.eml"), &[ME.into()]).remove(0).event;
        let uid = copy.uid.clone();
        j.store.write(|tx| store::apply_item(tx, calendar, "g1", None, &[copy], None, &Horizon::around(now(), Brussels))).unwrap();
        let asked = "ATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\n";

        let refresh = event_card(&j, undated("REFRESH", &uid, asked).as_bytes());
        assert_eq!((refresh.label, refresh.title.as_str()), ("Update requested", "Roadmap <review> & budget"));
        assert_eq!(refresh.when.as_deref(), Some("Thu 12 Nov · 16:00 to 16:45"));
        assert_eq!(refresh.location.as_deref(), Some("Room 2.04 (8)"));
        assert_eq!(refresh.notes[0].text, "Bo Nilsson asked for the latest version", "the guest who asked, not the copy's first guest");
        assert_eq!(refresh.answer, Some(Response::NeedsAction));

        let Card::Reply(reply) = cards_of(&j, undated("REPLY", &uid, "ATTENDEE;PARTSTAT=DECLINED;CN=Bo Nilsson:mailto:bo@example.com\r\n").as_bytes()).remove(0) else {
            panic!("a reply")
        };
        assert_eq!((reply.line.as_str(), reply.about.as_str()), ("Bo Nilsson declined", "Roadmap <review> & budget · Thu 12 Nov"));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_message_without_a_start_takes_the_whole_series_from_the_calendar() {
        let (j, calendar) = jolta("card-undated-series");
        let copy: Vec<EventData> = invitation::find(mail!("series.eml"), &[ME.into()]).into_iter().map(|i| i.event).collect();
        assert_eq!(copy.len(), 2, "the series and its moved Thursday");
        let uid = copy[0].uid.clone();
        j.store.write(|tx| store::apply_item(tx, calendar, "s1", None, &copy, None, &Horizon::around(now(), Brussels))).unwrap();

        let refresh = event_card(&j, undated("REFRESH", &uid, "ATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\n").as_bytes());
        assert_eq!(refresh.when.as_deref(), Some("Thu 5 Nov · 10:00 to 10:15"), "the next occurrence is the moved one");

        let named = "RECURRENCE-ID;TZID=Europe/Brussels:20261110T093000\r\nATTENDEE;PARTSTAT=ACCEPTED;CN=Bo Nilsson:mailto:bo@example.com\r\n";
        let Card::Reply(reply) = cards_of(&j, undated("REPLY", &uid, named).as_bytes()).remove(0) else { panic!("a reply") };
        assert_eq!(reply.about, "Team standup · Tue 10 Nov", "an occurrence the series makes, without an override");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_message_without_a_start_or_a_copy_shows_what_it_says() {
        let (j, _) = jolta("card-undated-alone");
        let cancel = event_card(&j, undated("CANCEL", "gone@example.com", "SUMMARY:Board dinner\r\nSEQUENCE:2\r\n").as_bytes());
        assert_eq!((cancel.label, cancel.title.as_str(), cancel.cancelled), ("Cancelled", "Board dinner", true));
        assert_eq!((cancel.when, cancel.availability), (None, None));
        assert_eq!(cancel.organizer.as_deref(), Some("Ann Dupont"));

        let refresh = event_card(&j, undated("REFRESH", "unknown@example.com", "ATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\n").as_bytes());
        assert_eq!((refresh.title.as_str(), refresh.when.as_deref()), ("(no title)", None));
        assert!(cards_of(&j, undated("REQUEST", "u", "SUMMARY:No time\r\n").as_bytes()).is_empty(), "a request needs a start");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn join_only_for_a_link_the_provider_table_knows() {
        let (j, _) = jolta("card-join");
        let claimed = event_card(&j, request("X-GOOGLE-CONFERENCE:https://evil.example/phish\r\n").as_bytes());
        assert_eq!(claimed.join, None, "the property names Meet, the link doesn't");
        let teams = event_card(&j, request("X-MICROSOFT-SKYPETEAMSMEETINGURL:https://evil.example/l/meetup-join/1\r\n").as_bytes());
        assert_eq!(teams.join, None);
        let real = event_card(&j, request("X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij\r\n").as_bytes());
        assert_eq!(real.join.unwrap().provider, "Google Meet");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn the_strip_across_the_autumn_clock_change() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let start = Utc.with_ymd_and_hms(2026, 10, 24, 23, 30, 0).unwrap();
            let _ = tx.send(strip(start, start + Duration::hours(1), "Late call", &[], Brussels));
        });
        let s = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the strip finishes");
        let hours: Vec<&str> = s.hours.iter().map(|(_, h)| h.as_str()).collect();
        assert_eq!(hours, ["23:00", "00:00", "01:00", "02:00", "02:00", "03:00", "04:00"], "the repeated hour shows twice");
        let lefts: Vec<f32> = s.hours.iter().map(|(l, _)| *l).collect();
        assert!(lefts.windows(2).all(|w| (w[1] - w[0] - 100.0 / 6.0).abs() < 0.01), "an hour apart: {lefts:?}");
    }

    #[test]
    fn a_series_shows_its_next_occurrence() {
        let (j, _) = jolta("card-series");
        let c = event_card(&j, mail!("series.eml"));
        assert_eq!(c.when.as_deref(), Some("Thu 5 Nov · 10:00 to 10:15"), "Tuesday's has passed and Thursday's moved");
        assert_eq!(c.repeat.as_deref(), Some("Weekly on Tuesday and Thursday, until 17 December 2026"));
        assert_eq!(c.location, None, "the Zoom link is the Join button");
        assert_eq!(c.join.unwrap().provider, "Zoom");
        assert_eq!(cards_of(&j, mail!("series.eml")).len(), 1, "a series and its changed occurrence are one card");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A REQUEST from Ann for `uid`, as a whole message with its own
    /// Message-ID, at a SEQUENCE and DTSTAMP, with the VEVENT's other `lines`.
    pub(crate) fn version(id: &str, uid: &str, method: &str, seq: i64, stamp: &str, lines: &str) -> String {
        format!(
            "From: ann@example.com\r\nTo: me@example.com\r\nSubject: s\r\nMessage-ID: <{id}@example.com>\r\n\
             Content-Type: text/calendar; method={method}\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             SEQUENCE:{seq}\r\nDTSTAMP:{stamp}\r\nORGANIZER;CN=Ann Dupont:mailto:ann@example.com\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\n{lines}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// The card of a message stored in the data folder, which can see the
    /// other versions next to it.
    fn stored_card(j: &crate::Jolta, message: MessageId, raw: &str) -> EventCard {
        let me = [ME.to_string()];
        let found = invitation::find(raw.as_bytes(), &me);
        let sender = from_of(raw.as_bytes());
        let cx = Context { message, account: AccountId(1), me: &me, sender: &sender, now: now(), here: Brussels, blobs: &j.blobs };
        match j.store.read(|c| cards(c, &found, &cx)).unwrap().remove(0) {
            Card::Event(e) => e,
            other => panic!("{other:?}"),
        }
    }

    fn edit(old: &str, new: &str) -> Option<Edit> {
        Some(Edit { old: old.into(), new: new.into(), ..Edit::default() })
    }

    pub(crate) const V1: &str = "DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Sync\r\nLOCATION:Room A\r\n\
                      DESCRIPTION:Join https://us02web.zoom.us/j/81234567890\r\n";

    #[test]
    fn an_update_strikes_what_changed() {
        let (j, _) = jolta("card-update");
        let first = crate::calendar::invitation::tests::import(&j, version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1).as_bytes());
        let moved = "DTSTART:20261112T130000Z\r\nDTEND:20261112T140000Z\r\nSUMMARY:Sync\r\nLOCATION:Room B\r\n\
                     X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij\r\n";
        let v2 = version("v2", "sync", "REQUEST", 1, "20261104T090000Z", moved);
        let second = crate::calendar::invitation::tests::import(&j, v2.as_bytes());

        let c = stored_card(&j, second, &v2);
        assert_eq!(c.label, "Updated invitation");
        assert_eq!(c.when.as_deref(), Some("Thu 12 Nov · 14:00 to 15:00"));
        assert_eq!(
            c.changes.when,
            Some(Edit { lead: "Thu 12 Nov · ".into(), old: "16:00 to 17:00".into(), new: "14:00 to 15:00".into(), tail: String::new() }),
            "the same day: only the time is struck"
        );
        assert_eq!(c.changes.location, edit("Room A", "Room B"));
        assert_eq!(c.changes.join, edit("Zoom", "Google Meet"));
        assert_eq!(c.changes.repeat, None);
        assert!(c.answering.is_some(), "the newest version is answered");

        let old = stored_card(&j, first, &version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1));
        assert_eq!(notes(&old), ["There's a newer version of this invitation", "You haven't written to this sender before"]);
        assert_eq!(old.label, "Invitation", "the first version, though a newer one exists");
        assert_eq!((old.answering, old.availability, old.join, old.answer), (None, None, None, None), "no answers, conflicts or link on an old version");
        assert!(old.changes.is_empty());
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn each_line_an_update_can_change() {
        let (j, _) = jolta("card-update-lines");
        crate::calendar::invitation::tests::import(&j, version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1).as_bytes());
        let check = |n: i64, lines: &str| {
            let raw = version(&format!("n{n}"), "sync", "REQUEST", n, "20261104T090000Z", lines);
            let id = crate::calendar::invitation::tests::import(&j, raw.as_bytes());
            let c = stored_card(&j, id, &raw);
            j.store.write(|tx| Ok(tx.execute("DELETE FROM message WHERE id = ?1", [id]).map(|_| ())?)).unwrap();
            c.changes
        };
        let zoom = "DESCRIPTION:Join https://us02web.zoom.us/j/81234567890\r\n";

        let next_day = check(1, &format!("DTSTART:20261113T150000Z\r\nDTEND:20261113T160000Z\r\nSUMMARY:Sync\r\nLOCATION:Room A\r\n{zoom}"));
        assert_eq!(
            next_day.when,
            Some(Edit { lead: String::new(), old: "Thu 12 Nov".into(), new: "Fri 13 Nov".into(), tail: " · 16:00 to 17:00".into() }),
            "the same time: only the day is struck"
        );
        let both = check(2, &format!("DTSTART:20261113T080000Z\r\nDTEND:20261113T090000Z\r\nSUMMARY:Sync\r\nLOCATION:Room A\r\n{zoom}"));
        assert_eq!(both.when, edit("Thu 12 Nov · 16:00 to 17:00", "Fri 13 Nov · 09:00 to 10:00"));

        let weekly = check(3, &format!("DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Sync\r\nLOCATION:Room A\r\n{zoom}"));
        assert_eq!(weekly.repeat, edit("", "Weekly on Thursday"), "a rule added has nothing to strike");
        assert_eq!(weekly.when, None);

        let no_place = check(4, &format!("DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Sync\r\n{zoom}"));
        assert_eq!(no_place.location, edit("Room A", ""), "a place taken away is only struck");
        let other_zoom = check(5, "DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Sync\r\nLOCATION:Room A\r\nDESCRIPTION:Join https://us02web.zoom.us/j/99999999999\r\n");
        assert_eq!(other_zoom.join, edit("", "New Zoom link"));
        let no_link = check(6, "DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Sync\r\nLOCATION:Room A\r\n");
        assert_eq!(no_link.join, edit("Zoom", ""));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_new_title_alone_is_a_note() {
        let (j, _) = jolta("card-retitled");
        crate::calendar::invitation::tests::import(&j, version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1).as_bytes());
        let raw = version("v2", "sync", "REQUEST", 0, "20261104T090000Z", &V1.replace("SUMMARY:Sync", "SUMMARY:Weekly sync"));
        let id = crate::calendar::invitation::tests::import(&j, raw.as_bytes());
        let c = stored_card(&j, id, &raw);
        assert!(c.changes.is_empty());
        assert_eq!(c.label, "Updated invitation", "a later DTSTAMP at the same SEQUENCE");
        assert_eq!(notes(&c)[0], "The organizer changed the title");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// RFC 5546 orders versions by SEQUENCE, then DTSTAMP.
    #[test]
    fn the_newest_version_by_sequence_then_dtstamp() {
        let (j, _) = jolta("card-newest");
        let raw = |id: &str, seq, stamp| version(id, "sync", "REQUEST", seq, stamp, V1);
        let a = crate::calendar::invitation::tests::import(&j, raw("a", 1, "20261104T080000Z").as_bytes());
        let b = crate::calendar::invitation::tests::import(&j, raw("b", 1, "20261104T100000Z").as_bytes());
        let c = crate::calendar::invitation::tests::import(&j, raw("c", 0, "20261104T120000Z").as_bytes());
        let newer = |id, raw: String| stored_card(&j, id, &raw).answering.is_none();
        assert!(newer(a, raw("a", 1, "20261104T080000Z")), "b has the same SEQUENCE and a later DTSTAMP");
        assert!(!newer(b, raw("b", 1, "20261104T100000Z")), "b is the newest");
        assert!(newer(c, raw("c", 0, "20261104T120000Z")), "a later DTSTAMP doesn't beat a higher SEQUENCE");

        let cancel = crate::calendar::invitation::tests::import(&j, version("x", "sync", "CANCEL", 2, "20261104T130000Z", &V1.replace("SUMMARY:Sync", "SUMMARY:Canceled: Sync")).as_bytes());
        assert_eq!(notes(&stored_card(&j, b, &raw("b", 1, "20261104T100000Z")))[0], "The organizer has since cancelled this meeting");
        let cancelled = stored_card(&j, cancel, &version("x", "sync", "CANCEL", 2, "20261104T130000Z", &V1.replace("SUMMARY:Sync", "SUMMARY:Canceled: Sync")));
        assert_eq!(cancelled.changes, Changes::default(), "a cancellation strikes nothing");
        assert_eq!(cancelled.title, "Sync", "Exchange's \"Canceled: \" goes: the label says it");
        assert!(!notes(&cancelled).iter().any(|n| n.contains("changed")), "{:?}", cancelled.notes);
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The calendar's copy is the version before while it hasn't taken the
    /// update yet, and a newer one when it holds a later SEQUENCE at
    /// another time.
    #[test]
    fn the_calendars_copy_as_a_version() {
        let (j, calendar) = jolta("card-copy-version");
        let horizon = Horizon::around(now(), Brussels);
        let store_copy = |seq: i64, lines: &str| {
            let raw = version("copy", "sync", "REQUEST", seq, "20261104T080000Z", lines);
            let mut e = invitation::find(raw.as_bytes(), &[ME.into()]).remove(0).event;
            e.attendees.iter_mut().for_each(|a| a.response = Response::Accepted);
            j.store.write(|tx| store::apply_item(tx, calendar, "sync.ics", None, &[e], None, &horizon)).unwrap();
        };
        store_copy(0, V1);
        let moved = V1.replace("20261112T15", "20261112T13").replace("20261112T16", "20261112T14");
        let raw = version("v2", "sync", "REQUEST", 1, "20261104T090000Z", &moved);
        let id = crate::calendar::invitation::tests::import(&j, raw.as_bytes());
        let c = stored_card(&j, id, &raw);
        assert_eq!(c.changes.when.unwrap().old, "16:00 to 17:00", "the copy is the version before");
        assert_eq!(c.answer, Some(Response::NeedsAction), "accepted at the old time: the move asks again");

        store_copy(1, &moved);
        let synced = stored_card(&j, id, &raw);
        assert_eq!(synced.answer, Some(Response::Accepted), "the server's answer for this version");
        assert_eq!(synced.changes.when, None, "the copy has this version: no other to compare with");

        store_copy(2, V1);
        assert_eq!(notes(&stored_card(&j, id, &raw))[0], "There's a newer version of this invitation", "the copy moved back since");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Graph copies keep no SEQUENCE. Behind a newer mail, a copy at
    /// another time is that newer version, not the one before.
    #[test]
    fn a_graph_copy_behind_a_newer_mail_is_no_earlier_version() {
        let (j, _) = jolta("card-graph-copy");
        let horizon = Horizon::around(now(), Brussels);
        let moved = V1.replace("20261112T15", "20261113T15").replace("20261112T16", "20261113T16");
        let graph = j
            .store
            .write(|tx| {
                let remote = store::RemoteCalendar { remote_id: "AAMk".into(), name: "Calendar".into(), color: None, zone: None, access: crate::calendar::Access::Owner, primary: true, refresh: None };
                let calendar = store::sync_calendar_list(tx, AccountId(1), crate::calendar::CalendarSource::Graph, &[remote])?[0].id;
                let mut copy = invitation::find(version("c", "sync", "REQUEST", 1, "20261104T090000Z", &moved).as_bytes(), &[ME.into()]).remove(0).event;
                copy.sequence = 0;
                store::apply_item(tx, calendar, "AAMkEvent", None, &[copy], None, &horizon)?;
                Ok(calendar)
            })
            .unwrap();
        assert!(graph.0 > 0);
        let first = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        let m1 = crate::calendar::invitation::tests::import(&j, first.as_bytes());
        crate::calendar::invitation::tests::import(&j, version("v2", "sync", "REQUEST", 1, "20261104T090000Z", &moved).as_bytes());
        let old = stored_card(&j, m1, &first);
        assert_eq!((old.label, old.outdated), ("Invitation", true));
        assert!(old.changes.is_empty());
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// One occurrence moved for the first time: the series said where it was.
    #[test]
    fn a_moved_occurrence_strikes_where_the_series_had_it() {
        let (j, _) = jolta("card-occurrence-moved");
        let series = "DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n";
        crate::calendar::invitation::tests::import(&j, version("s", "standup", "REQUEST", 0, "20261104T080000Z", series).as_bytes());
        let moved = "RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\nDTSTART;TZID=Europe/Brussels:20261118T140000\r\n\
                     DTEND;TZID=Europe/Brussels:20261118T141500\r\nSUMMARY:Standup\r\n";
        let raw = version("o", "standup", "REQUEST", 1, "20261104T090000Z", moved);
        let id = crate::calendar::invitation::tests::import(&j, raw.as_bytes());
        let c = stored_card(&j, id, &raw);
        assert_eq!(c.changes.when, edit("Tue 17 Nov · 09:30 to 09:45", "Wed 18 Nov · 14:00 to 14:15"));
        assert_eq!(c.changes.repeat, None, "the occurrence isn't the series");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Remove from calendar shows while the account's calendar holds the
    /// cancelled event, or the cancelled occurrence of a series.
    #[test]
    fn a_cancellation_offers_remove_while_the_calendar_holds_it() {
        let (j, calendar) = jolta("card-remove");
        let horizon = Horizon::around(now(), Brussels);
        let hold = |remote: &str, events: Vec<EventData>| j.store.write(|tx| store::apply_item(tx, calendar, remote, None, &events, None, &horizon)).unwrap();
        let cancel = version("x", "sync", "CANCEL", 1, "20261104T090000Z", V1);
        let id = crate::calendar::invitation::tests::import(&j, cancel.as_bytes());
        let mut copy = invitation::find(version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1).as_bytes(), &[ME.into()]).remove(0).event;
        hold("sync.ics", vec![copy.clone()]);
        let c = stored_card(&j, id, &cancel);
        assert_eq!(c.removing, Some(Removing { message: id, uid: "sync".into(), occurrence: None }));
        assert_eq!(notes(&c), ["Cancelled by the organizer"]);
        copy.status = EventStatus::Cancelled;
        hold("sync.ics", vec![copy]);
        assert_eq!(stored_card(&j, id, &cancel).removing, None, "the server cancelled it already");

        let series = "DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n";
        let mut master = invitation::find(version("s", "standup", "REQUEST", 0, "20261104T080000Z", series).as_bytes(), &[ME.into()]).remove(0).event;
        hold("standup.ics", vec![master.clone()]);
        let one = version("o", "standup", "CANCEL", 1, "20261104T090000Z", &format!("RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\n{series}").replace("RRULE:FREQ=WEEKLY\r\n", ""));
        let id = crate::calendar::invitation::tests::import(&j, one.as_bytes());
        let c = stored_card(&j, id, &one);
        let (rid, day) = c.removing.unwrap().occurrence.unwrap();
        assert_eq!((rid, day.as_str()), (Moment::Time(Local { wall: chrono::NaiveDate::from_ymd_opt(2026, 11, 17).unwrap().and_hms_opt(9, 30, 0).unwrap(), zone: Zone::Tz(Brussels) }), "Tue 17 Nov"));
        master.recurrence.as_mut().unwrap().exdates.push(rid);
        hold("standup.ics", vec![master]);
        assert_eq!(stored_card(&j, id, &one).removing, None, "the series leaves that occurrence out already");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A calendar of `source` on account 1, shown.
    fn calendar_of(j: &crate::Jolta, source: CalendarSource, access: crate::calendar::Access) -> CalendarId {
        j.store
            .write(|tx| {
                let remote = store::RemoteCalendar { remote_id: "remote".into(), name: "Calendar".into(), color: None, zone: None, access, primary: true, refresh: None };
                Ok(store::sync_calendar_list(tx, AccountId(1), source, &[remote])?[0].id)
            })
            .unwrap()
    }

    /// Graph writes no INTERVAL=1 where Exchange's invitation mail does.
    #[test]
    fn a_graph_series_with_the_same_rule_written_otherwise_keeps_its_answer() {
        let (j, _) = jolta("card-graph-rule");
        let graph = calendar_of(&j, CalendarSource::Graph, crate::calendar::Access::Owner);
        let series = "DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Sync\r\n";
        let raw = version("v1", "sync", "REQUEST", 1, "20261104T080000Z", &format!("{series}RRULE:FREQ=WEEKLY;UNTIL=20261217T150000Z;INTERVAL=1;BYDAY=TH;WKST=SU\r\n"));
        let id = import(&j, raw.as_bytes());
        let mut copy = invitation::find(version("c", "sync", "REQUEST", 0, "20261104T080000Z", &format!("{series}RRULE:FREQ=WEEKLY;BYDAY=TH;WKST=SU;UNTIL=20261217T225959Z\r\n")).as_bytes(), &[ME.into()])
            .remove(0)
            .event;
        copy.attendees.iter_mut().for_each(|a| a.response = Response::Accepted);
        j.store.write(|tx| store::apply_item(tx, graph, "AAMk", None, &[copy], None, &Horizon::around(now(), Brussels))).unwrap();
        let c = stored_card(&j, id, &raw);
        assert_eq!((c.label, c.answer), ("Invitation", Some(Response::Accepted)), "the copy is this version, not an earlier one");
        assert!(c.changes.is_empty());
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// `raw` from `from`, naming `organizer` as the ORGANIZER.
    fn sent_as(raw: &str, from: &str, organizer: &str) -> String {
        raw.replace("From: ann@example.com", &format!("From: {from}")).replace("ORGANIZER;CN=Ann Dupont:mailto:ann@example.com", &format!("ORGANIZER:mailto:{organizer}"))
    }

    #[test]
    fn only_the_organizers_mail_is_a_version_of_the_meeting() {
        let (j, calendar) = jolta("card-organizer");
        let real = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        let m1 = import(&j, real.as_bytes());
        let stranger = sent_as(&version("x1", "sync", "REQUEST", 100, "20261104T090000Z", V1), "eve@example.net", "eve@example.net");
        let x1 = import(&j, stranger.as_bytes());
        let posing = sent_as(&version("x2", "sync", "REQUEST", 101, "20261104T090000Z", V1), "eve@example.net", "ann@example.com");
        let x2 = import(&j, posing.as_bytes());
        let c = stored_card(&j, m1, &real);
        assert!(!c.outdated && c.answering.is_some(), "neither a stranger's SEQUENCE 100 nor one naming Ann replaces Ann's invitation");
        for (id, raw) in [(x1, &stranger), (x2, &posing)] {
            let fake = stored_card(&j, id, raw);
            assert_eq!(fake.notes[0], Note { text: "This isn't from the meeting's organizer".into(), warn: true });
            assert_eq!((fake.answering, fake.changes.is_empty()), (None, true));
        }

        let mut copy = invitation::find(real.as_bytes(), &[ME.into()]).remove(0).event;
        copy.attendees.iter_mut().for_each(|a| a.response = Response::Accepted);
        j.store.write(|tx| store::apply_item(tx, calendar, "sync.ics", None, &[copy], None, &Horizon::around(now(), Brussels))).unwrap();
        let cancel = sent_as(&version("x3", "sync", "CANCEL", 102, "20261104T100000Z", V1), "eve@example.net", "ann@example.com");
        let x3 = import(&j, cancel.as_bytes());
        let fake = stored_card(&j, x3, &cancel);
        assert_eq!(fake.removing, None, "a cancellation from someone else removes nothing");
        assert!(notes(&fake).contains(&"This isn't from the meeting's organizer"));
        assert!(!stored_card(&j, m1, &real).outdated, "nor cancels Ann's meeting");

        let google = sent_as(&version("v2", "sync", "REQUEST", 1, "20261104T110000Z", V1), "calendar-notification@google.com", "ann@example.com");
        let v2 = import(&j, google.as_bytes());
        assert!(stored_card(&j, m1, &real).outdated, "Google sends for Ann");
        assert!(!notes(&stored_card(&j, v2, &google)).contains(&"This isn't from the meeting's organizer"));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Sync stores a message's date from the server, IMAP's INTERNALDATE or
    /// Graph's receivedDateTime, so a stranger's backdated Date header
    /// doesn't put a later mail before the request that named the organizer.
    #[test]
    fn a_backdated_request_doesnt_replace_the_organizer() {
        let (j, _) = jolta("card-backdated");
        let dated = |raw: String, date: &str| raw.replace("Subject: s\r\n", &format!("Subject: s\r\nDate: {date}\r\n"));
        let real = dated(version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1), "Wed, 04 Nov 2026 09:00:00 +0100");
        let m1 = import(&j, real.as_bytes());
        let stranger = dated(sent_as(&version("x1", "sync", "REQUEST", 1, "20261101T080000Z", V1), "eve@example.net", "eve@example.net"), "Sun, 01 Nov 2026 09:00:00 +0100");
        let x1 = import(&j, stranger.as_bytes());
        let arrived = |id: MessageId, at: &str| {
            let at = DateTime::parse_from_rfc3339(at).unwrap().timestamp();
            j.store.write(|tx| Ok(tx.execute("UPDATE message SET date = ?2 WHERE id = ?1", rusqlite::params![id, at]).map(|_| ())?)).unwrap();
        };
        arrived(m1, "2026-11-04T08:00:05Z");
        arrived(x1, "2026-11-04T10:00:05Z");
        let c = stored_card(&j, m1, &real);
        assert!(c.answering.is_some() && !c.outdated, "{:?}", c.notes);
        assert!(notes(&stored_card(&j, x1, &stranger)).contains(&"This isn't from the meeting's organizer"));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_cancellation_without_an_organizer_is_nobodys() {
        let (j, calendar) = jolta("card-no-organizer");
        let real = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        let m1 = import(&j, real.as_bytes());
        let copy = invitation::find(real.as_bytes(), &[ME.into()]).remove(0).event;
        j.store.write(|tx| store::apply_item(tx, calendar, "sync.ics", None, &[copy], None, &Horizon::around(now(), Brussels))).unwrap();
        let cancel = version("x", "sync", "CANCEL", 1, "20261104T090000Z", V1).replace("From: ann@example.com", "From: eve@example.net").replace("ORGANIZER;CN=Ann Dupont:mailto:ann@example.com\r\n", "");
        let x = import(&j, cancel.as_bytes());
        let fake = stored_card(&j, x, &cancel);
        assert_eq!(fake.removing, None);
        assert!(notes(&fake).contains(&"This isn't from the meeting's organizer"), "{:?}", fake.notes);
        let c = stored_card(&j, m1, &real);
        assert!(!c.outdated && c.answering.is_some(), "Ann's invitation still stands");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// iCloud mails its invitations from its own address, naming the
    /// organizer by a relay address.
    #[test]
    fn icloud_sends_for_its_organizers() {
        let (j, _) = jolta("card-icloud-sender");
        let relay = "2_abcdefgh@imip.me.com";
        let first = sent_as(&version("i1", "trip", "REQUEST", 0, "20261104T080000Z", V1), "noreply@email.apple.com", relay);
        let m1 = import(&j, first.as_bytes());
        let c = stored_card(&j, m1, &first);
        assert!(c.answering.is_some() && !notes(&c).contains(&"This isn't from the meeting's organizer"), "{:?}", c.notes);
        let moved = sent_as(&version("i2", "trip", "REQUEST", 1, "20261104T090000Z", V1), "noreply@email.apple.com", relay);
        import(&j, moved.as_bytes());
        assert!(stored_card(&j, m1, &first).outdated);
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Deleting the organizer's copy cancels the meeting for its guests, and
    /// a shared calendar's copy is someone else's.
    #[test]
    fn remove_only_the_users_copy_as_a_guest() {
        let (j, calendar) = jolta("card-remove-guest");
        let horizon = Horizon::around(now(), Brussels);
        let hold = |calendar: CalendarId, organizer: &str, edit: &dyn Fn(&mut EventData)| {
            let mut copy = invitation::find(version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1).as_bytes(), &[ME.into()]).remove(0).event;
            copy.organizer = Some(Person { address: organizer.into(), name: String::new() });
            edit(&mut copy);
            j.store.write(|tx| store::apply_item(tx, calendar, "sync.ics", None, &[copy], None, &horizon)).unwrap();
        };
        let removing = |organizer: &str| {
            let cancel = sent_as(&version(&format!("x-{organizer}"), "sync", "CANCEL", 1, "20261104T090000Z", V1), organizer, organizer);
            let id = import(&j, cancel.as_bytes());
            let c = stored_card(&j, id, &cancel);
            j.store.write(|tx| Ok(tx.execute("DELETE FROM message WHERE id = ?1", [id]).map(|_| ())?)).unwrap();
            assert!(!notes(&c).contains(&"This isn't from the meeting's organizer"), "{organizer}");
            c.removing.is_some()
        };
        hold(calendar, "ann@example.com", &|_| {});
        assert!(removing("ann@example.com"), "the user's copy as a guest");

        hold(calendar, ME, &|_| {});
        assert!(!removing(ME), "the user organizes it");
        let organizer = sent_as(&version("x-own", "sync", "CANCEL", 1, "20261104T090000Z", V1), ME, ME);
        let own = import(&j, organizer.as_bytes());
        assert!(!notes(&stored_card(&j, own, &organizer)).contains(&"Not in your calendar"), "the calendar holds the organizer's copy");
        let alias = "me.too@example.com";
        let chair = crate::calendar::Attendee { person: Person { address: alias.into(), name: String::new() }, role: Role::Chair, response: Response::Accepted, comment: String::new(), is_me: true };
        hold(calendar, alias, &|e| e.attendees.push(chair.clone()));
        assert!(!removing(alias), "as an alias the copy marks as the user's");
        hold(calendar, "ann@example.com", &|e| e.attendees.retain(|a| !a.is_me));
        assert!(!removing("ann@example.com"), "the copy doesn't list the user");

        j.store.write(|tx| Ok(tx.execute("DELETE FROM event", []).map(|_| ())?)).unwrap();
        let shared = calendar_of(&j, CalendarSource::Google, crate::calendar::Access::Write);
        hold(shared, "ann@example.com", &|_| {});
        assert!(!removing("ann@example.com"), "a calendar shared with the user");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[tokio::test]
    async fn removing_the_organizers_copy_is_refused_before_any_request() {
        let (j, calendar) = jolta("remove-organizer");
        let cancel = sent_as(&version("x", "sync", "CANCEL", 1, "20261104T090000Z", V1), ME, ME);
        let id = import(&j, cancel.as_bytes());
        let mut copy = invitation::find(version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1).as_bytes(), &[ME.into()]).remove(0).event;
        copy.organizer = Some(Person { address: ME.into(), name: String::new() });
        j.store.write(|tx| store::apply_item(tx, calendar, "sync.ics", None, &[copy], None, &Horizon::around(now(), Brussels))).unwrap();
        let err = crate::calendar::remove::remove(&j, id).await.unwrap_err();
        assert!(err.to_string().contains("You organize this meeting"), "{err}");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The copy's SEQUENCE says it is newer, whatever it changed.
    #[test]
    fn a_newer_or_cancelled_copy_replaces_the_mail() {
        let (j, calendar) = jolta("card-copy-newer");
        let raw = version("v1", "sync", "REQUEST", 1, "20261104T080000Z", V1);
        let id = import(&j, raw.as_bytes());
        let store_copy = |seq: i64, title: &str, status: EventStatus| {
            let mut e = invitation::find(version("c", "sync", "REQUEST", seq, "20261104T090000Z", &V1.replace("SUMMARY:Sync", title)).as_bytes(), &[ME.into()]).remove(0).event;
            e.status = status;
            j.store.write(|tx| store::apply_item(tx, calendar, "sync.ics", None, &[e], None, &Horizon::around(now(), Brussels))).unwrap();
        };
        store_copy(2, "SUMMARY:Weekly sync", EventStatus::Confirmed);
        let c = stored_card(&j, id, &raw);
        assert_eq!((c.outdated, c.answering.is_none()), (true, true), "a new title at SEQUENCE 2");
        store_copy(1, "SUMMARY:Sync", EventStatus::Cancelled);
        assert_eq!(notes(&stored_card(&j, id, &raw))[0], "The organizer has since cancelled this meeting");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_series_cancellation_replaces_an_occurrences_invitation() {
        let (j, _) = jolta("card-series-cancel");
        let one = version("o", "standup", "REQUEST", 1, "20261104T080000Z", "RECURRENCE-ID:20261117T083000Z\r\nDTSTART:20261118T133000Z\r\nDTEND:20261118T134500Z\r\nSUMMARY:Standup\r\n");
        let id = import(&j, one.as_bytes());
        import(&j, version("x", "standup", "CANCEL", 2, "20261104T090000Z", "DTSTART:20261110T083000Z\r\nDTEND:20261110T084500Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n").as_bytes());
        let c = stored_card(&j, id, &one);
        assert_eq!((c.outdated, &c.answering), (true, &None));
        assert_eq!(notes(&c)[0], "The organizer has since cancelled this meeting");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The calendar's copy of the series took the cancellation; it has no
    /// override for the occurrence the mail invites to.
    #[test]
    fn a_cancelled_series_in_the_calendar_replaces_an_occurrences_invitation() {
        let (j, calendar) = jolta("card-series-copy-cancel");
        let one = version("o", "standup", "REQUEST", 1, "20261104T080000Z", "RECURRENCE-ID:20261117T083000Z\r\nDTSTART:20261118T133000Z\r\nDTEND:20261118T134500Z\r\nSUMMARY:Standup\r\n");
        let id = import(&j, one.as_bytes());
        let series = version("s", "standup", "REQUEST", 0, "20261104T070000Z", "DTSTART:20261110T083000Z\r\nDTEND:20261110T084500Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n");
        let mut master = invitation::find(series.as_bytes(), &[ME.into()]).remove(0).event;
        master.status = EventStatus::Cancelled;
        master.sequence = 2;
        let store_master = |m: &EventData| j.store.write(|tx| store::apply_item(tx, calendar, "standup.ics", None, std::slice::from_ref(m), None, &Horizon::around(now(), Brussels))).unwrap();
        store_master(&master);
        let c = stored_card(&j, id, &one);
        assert_eq!((c.outdated, &c.answering), (true, &None));
        assert_eq!(notes(&c)[0], "The organizer has since cancelled this meeting");

        let reinstated = version("o3", "standup", "REQUEST", 3, "20261104T100000Z", "RECURRENCE-ID:20261117T083000Z\r\nDTSTART:20261118T133000Z\r\nDTEND:20261118T134500Z\r\nSUMMARY:Standup\r\n");
        let later = import(&j, reinstated.as_bytes());
        let c = stored_card(&j, later, &reinstated);
        assert!(!c.outdated && c.answering.is_some(), "an occurrence invited after the series was cancelled stays answerable");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn an_occurrence_named_in_another_zone_is_the_same_occurrence() {
        let (j, _) = jolta("card-occurrence-zones");
        let utc = "RECURRENCE-ID:20261117T083000Z\r\nDTSTART:20261117T083000Z\r\nDTEND:20261117T084500Z\r\nSUMMARY:Standup\r\n";
        let first = version("o1", "standup", "REQUEST", 1, "20261104T080000Z", utc);
        let m1 = import(&j, first.as_bytes());
        let brussels = "RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\nDTSTART;TZID=Europe/Brussels:20261118T140000\r\n\
                        DTEND;TZID=Europe/Brussels:20261118T141500\r\nSUMMARY:Standup\r\n";
        import(&j, version("o2", "standup", "REQUEST", 2, "20261104T090000Z", brussels).as_bytes());
        let old = stored_card(&j, m1, &first);
        assert!(old.outdated && old.answering.is_none(), "09:30 Brussels is 08:30 UTC: the later mail replaces it");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The calendar holds the series accepted and no copy of the occurrence
    /// the card shows, which the organizer changed at SEQUENCE 1.
    #[test]
    fn a_series_accepted_in_the_calendar_covers_an_occurrence_only_where_the_series_has_it() {
        let (j, calendar) = jolta("card-copy-series-answer");
        let raw = String::from_utf8(mail!("series.eml").to_vec()).unwrap();
        let mut master = invitation::find(raw.as_bytes(), &[ME.into()]).into_iter().find(|i| i.event.recurrence_id.is_none()).unwrap().event;
        master.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = Response::Accepted);
        j.store.write(|tx| store::apply_item(tx, calendar, "standup.ics", None, &[master], None, &Horizon::around(now(), Brussels))).unwrap();
        let changed = |start: &str, end: &str| {
            let at = raw.rfind("SEQUENCE:0").unwrap();
            let mut r = raw.clone();
            r.replace_range(at..at + 10, "SEQUENCE:1");
            r.replace("DTSTART;TZID=Europe/Brussels:20261105T100000", &format!("DTSTART;TZID=Europe/Brussels:{start}"))
                .replace("DTEND;TZID=Europe/Brussels:20261105T101500", &format!("DTEND;TZID=Europe/Brussels:{end}"))
        };
        assert_eq!(event_card(&j, changed("20261105T100000", "20261105T101500").as_bytes()).answer, Some(Response::NeedsAction), "moved half an hour");
        assert_eq!(event_card(&j, changed("20261105T093000", "20261105T094500").as_bytes()).answer, Some(Response::Accepted), "where the series has it");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_moved_occurrence_shows_its_own_place() {
        let (j, _) = jolta("card-moved");
        let mut raw = String::from_utf8(mail!("series.eml").to_vec()).unwrap();
        for (old, new) in [("LOCATION:https://us02web.zoom.us/j/81234567890?pwd=abc123", "LOCATION:Room B"), ("SUMMARY:Team standup", "SUMMARY:Standup in person")] {
            let at = raw.rfind(old).unwrap();
            raw.replace_range(at..at + old.len(), new);
        }
        let c = event_card(&j, raw.as_bytes());
        assert_eq!(c.when.as_deref(), Some("Thu 5 Nov · 10:00 to 10:15"));
        assert_eq!((c.title.as_str(), c.location.as_deref()), ("Standup in person", Some("Room B")));
        assert_eq!(c.join, None, "the series' Zoom link isn't this occurrence's");
        assert_eq!(c.repeat.as_deref(), Some("Weekly on Tuesday and Thursday, until 17 December 2026"), "still part of the series");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
