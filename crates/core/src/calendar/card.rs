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
use super::{Bounds, CalendarSource, EventData, EventStatus, MeetingProvider, Moment, Person, Response, Role, Span, Zone, meeting, recur, tz};
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
    pub notes: Vec<Note>,
    /// The user's answer: in their own calendar, else the one they gave
    /// from Jolta for an event no calendar of theirs holds.
    pub answer: Option<Response>,
    pub availability: Option<Availability>,
    /// Set when the user can answer: an invitation to them that they don't
    /// organize.
    pub answering: Option<Answering>,
}

/// What the answer buttons need.
#[derive(Clone, Debug, PartialEq)]
pub struct Answering {
    pub message: MessageId,
    pub uid: String,
    /// For a repeating event, the occurrence the card shows, which "This
    /// occurrence only" answers.
    pub occurrence: Option<ShownOccurrence>,
    /// "Ann", whom the answer tells.
    pub organizer: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShownOccurrence {
    pub recurrence_id: Moment,
    /// "Thu 5 Nov".
    pub day: String,
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
            method => Card::Event(event(conn, method, &events, described, cx)?),
        });
    }
    Ok(out)
}

/// What the calendar says about the event an undated message names: the
/// whole series, so the next occurrence is chosen as for a dated message,
/// or for one named occurrence its override, else the master moved to it.
fn stored_copy(conn: &Connection, sent: &EventData, cx: &Context) -> Result<Vec<EventData>> {
    let series = store::stored_series(conn, &sent.uid)?;
    let Some(rid) = sent.recurrence_id else { return Ok(series) };
    if let Some(o) = series.iter().find(|e| e.recurrence_id.is_some_and(|stored| rid.names(&stored, cx.here))) {
        return Ok(vec![o.clone()]);
    }
    let master = series.iter().find(|e| e.recurrence_id.is_none() && e.recurrence.is_some());
    Ok(master
        .and_then(|m| series::occurrence_span(&m.span, &rid, cx.here).map(|span| EventData { recurrence_id: Some(rid), recurrence: None, span, ..m.clone() }))
        .into_iter()
        .collect())
}

/// `message` holds the events the mail sends, which say who asked and which
/// version it is; `described` the ones that give the time and place: the
/// same events, the calendar's copy, or none.
fn event(conn: &Connection, method: Method, message: &[EventData], described: &[EventData], cx: &Context) -> Result<EventCard> {
    let master = described.iter().find(|e| e.recurrence_id.is_none());
    let occurrence = (!described.is_empty()).then(|| next(master, described, cx));
    let repeats = master.is_some_and(|m| m.recurrence.is_some());
    let sent = message.iter().find(|e| e.recurrence_id.is_none()).unwrap_or(&message[0]);
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
    let copy = |account: Option<AccountId>| -> Result<Option<store::StoredEvent>> {
        Ok(match store::stored_event(conn, &shown.uid, shown.recurrence_id.as_ref(), account, cx.here)? {
            None if shown.recurrence_id.is_some() && master.is_some() => store::stored_event(conn, &shown.uid, None, account, cx.here)?,
            found => found,
        })
    };
    let stored = copy(None)?;

    let mut notes = Vec::new();
    match method {
        Method::Cancel => notes.push(Note { text: "Cancelled by the organizer".into(), warn: true }),
        Method::Counter => notes.push(Note { text: with_comment(format!("{} proposed this new time", first_attendee(said)), said), warn: false }),
        Method::DeclineCounter => notes.push(Note { text: "The organizer kept the original time".into(), warn: false }),
        Method::Refresh => notes.push(Note { text: format!("{} asked for the latest version", first_attendee(said)), warn: false }),
        Method::Add => notes.push(Note { text: "Adds dates to a repeating event".into(), warn: false }),
        Method::Request | Method::Publish | Method::Reply => {}
    }
    if stored.as_ref().is_some_and(|s| s.event.sequence > said.sequence) {
        notes.push(Note { text: "Your calendar has a newer version of this event".into(), warn: false });
    }
    if method != Method::Cancel && stranger(conn, said, cx)? {
        notes.push(Note { text: "You haven't written to this sender before".into(), warn: true });
    }

    let receiver = store::account(conn, cx.account)?.address;
    let answering = answerable(method, sent, &receiver).then(|| Answering {
        message: cx.message,
        uid: sent.uid.clone(),
        occurrence: occurrence.filter(|_| repeats).map(|(e, span)| ShownOccurrence {
            recurrence_id: e.recurrence_id.unwrap_or(span.start()),
            day: match span.bounds(cx.here) {
                Bounds::Utc { start, .. } => date(start.with_timezone(&cx.here).date_naive(), cx),
                Bounds::Days { start, .. } => date(day(start), cx),
            },
        }),
        organizer: sent.organizer.as_ref().map_or_else(String::new, |o| name(o, &[], true)),
    });
    // An invitation the user answers is answered in the calendars of the
    // account it came to; another account's copy may be the organizer's own.
    let (remembered, answered) = match &answering {
        Some(a) => (store::remembered_answer(conn, cx.account, &a.uid, a.occurrence.as_ref().map(|o| &o.recurrence_id))?, copy(Some(cx.account))?),
        None => (None, stored.clone()),
    };

    let wants_time = matches!(method, Method::Request | Method::Add | Method::Publish | Method::Counter);
    let availability = match bounds {
        Some(bounds) if wants_time && ends(bounds, cx) > cx.now => availability(conn, &shown.uid, bounds, &shown.title, cx)?,
        _ => None,
    };

    Ok(EventCard {
        label: label(method, said),
        title: if shown.title.trim().is_empty() { "(no title)".into() } else { shown.title.trim().to_string() },
        cancelled: method == Method::Cancel || shown.status == EventStatus::Cancelled,
        when,
        elsewhere,
        repeat: master
            .and_then(|m| m.recurrence.as_ref().map(|r| recur::describe(r, &m.span).unwrap_or_else(|| "Custom repeat".into()))),
        location: location(shown),
        join: join(shown, bounds, cx.now),
        organizer: shown.organizer.as_ref().map(|o| name(o, cx.me, false)),
        guests: guests(shown, cx.me),
        notes,
        answer: match answered.filter(|_| method != Method::Cancel) {
            // Jolta answers through these, so they hold the answer it gave.
            Some(s) if answered_there(conn, &s)? => s.event.my_response(),
            stored => remembered.or(stored.and_then(|s| s.event.my_response())),
        },
        availability,
        answering,
    })
}

fn answered_there(conn: &Connection, copy: &store::StoredEvent) -> Result<bool> {
    let source = store::calendar(conn, copy.calendar_id)?.source;
    Ok(!matches!(source, CalendarSource::Webcal | CalendarSource::Local))
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

fn label(method: Method, e: &EventData) -> &'static str {
    match method {
        Method::Request if e.sequence > 0 => "Updated invitation",
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
/// event itself, or for a series the next occurrence that hasn't ended, or
/// its last one when all have. A changed occurrence brings its own place,
/// link and guests.
fn next<'a>(master: Option<&'a EventData>, events: &'a [EventData], cx: &Context) -> (&'a EventData, Span) {
    let Some(m) = master.filter(|m| m.recurrence.is_some()) else {
        let e = master.unwrap_or(&events[0]);
        return (e, e.span);
    };
    let overrides: Vec<&EventData> = events.iter().filter(|e| e.recurrence_id.is_some()).collect();
    let owned: Vec<EventData> = overrides.iter().map(|&e| e.clone()).collect();
    let placed = series::place(Some(m), &owned, &Horizon::around(cx.now, cx.here));
    let ended = |s: &Span| match s.bounds(cx.here) {
        Bounds::Utc { end, .. } => end <= cx.now,
        Bounds::Days { end, .. } => day(end) <= cx.now.with_timezone(&cx.here).date_naive(),
    };
    let Some(o) = placed.iter().find(|o| !ended(&o.span)).or(placed.last()) else { return (m, m.span) };
    match o.source {
        Source::Master => (m, o.span),
        Source::Override(i) => (overrides[i], o.span),
    }
}

/// "Thu 5 Nov · 16:00 to 16:45" here, and the event's own clock when it
/// differs: "10:00 to 10:45 New York".
fn when(span: &Span, e: &EventData, cx: &Context) -> (String, Option<String>) {
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
            (text, None)
        }
        Bounds::Utc { start, end } => {
            let wall = |t: DateTime<Utc>, tz: Tz| t.with_timezone(&tz).naive_local();
            let here = (wall(start, cx.here), wall(end, cx.here));
            let text = if crosses_days(here.0, here.1) { range(here.0, here.1) } else { format!("{} · {}", date(here.0.date(), cx), range(here.0, here.1)) };
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
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::invitation::tests::{ME, jolta, mail};
    use crate::calendar::{CalendarId, Local, invitation};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    fn cards_of(j: &crate::Jolta, raw: &[u8]) -> Vec<Card> {
        let me = [ME.to_string()];
        let found = invitation::find(raw, &me);
        let cx = Context { message: MessageId(1), account: AccountId(1), me: &me, sender: "sender@example.com", now: now(), here: Brussels };
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
        assert_eq!(notes(&c), ["Cancelled by the organizer"]);
        assert_eq!(c.availability, None);

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
