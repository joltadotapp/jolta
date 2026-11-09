//! Holds (C4 U9): tentative events that keep the times a mail offers free
//! until someone books one.
//!
//! Each hold event carries a hidden marker with its set's id, which every
//! source reads back during sync (`EventData::mark`), so any Mac finds the
//! set's events and the user's other clients leave them alone. `decide` is
//! pure: from what a sync round left in the store and the mail, it says
//! what happened to each held time. U10 creates and deletes the events.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::invitation::{self, Method};
use super::recur::{self, Window};
use super::{CalendarId, EventData, EventStatus, Span, freebusy, tz};
use crate::blobs::Blobs;
use crate::{AccountId, Result, compose, store};
use crate::model::text_enum;

pub(crate) const GOOGLE_HOLD: &str = "joltaHold";
pub(crate) const GOOGLE_MESSAGE: &str = "joltaHoldMessage";
/// Graph's ids for the two properties, under a GUID of Jolta's own.
pub(crate) const GRAPH_HOLD: &str = "String {9d3b2d55-5a0e-4a6e-9b4e-6a6f6c746130} Name JoltaHold";
pub(crate) const GRAPH_MESSAGE: &str = "String {9d3b2d55-5a0e-4a6e-9b4e-6a6f6c746130} Name JoltaHoldMessage";
pub(crate) const CALDAV_HOLD: &str = "X-JOLTA-HOLD";
pub(crate) const CALDAV_MESSAGE: &str = "X-JOLTA-HOLD-MESSAGE";
/// The marker's value on an out-of-office event Jolta made (C4 U4).
const AWAY: &str = "away";

/// A hold set's id: a UUID, lowercase, also written on the server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HoldSetId(String);

impl HoldSetId {
    pub fn parse(text: &str) -> Option<HoldSetId> {
        let text = text.trim().to_ascii_lowercase();
        let uuid = text.len() == 36
            && text.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() });
        uuid.then_some(HoldSetId(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HoldSetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Jolta's hidden marker on an event it made for a feature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "is", rename_all = "lowercase")]
pub enum Mark {
    /// One of a hold set's events, with the Message-ID of the mail that
    /// offers its times once that mail left.
    Hold {
        set: HoldSetId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// An out-of-office event (C4 U4).
    Away,
}

impl Mark {
    /// The marker from the two values a source keeps. A value that is
    /// neither a set id nor `away` is no marker of Jolta's.
    fn read(hold: Option<&str>, message: Option<&str>) -> Option<Mark> {
        match hold?.trim() {
            AWAY => Some(Mark::Away),
            value => HoldSetId::parse(value).map(|set| Mark::Hold {
                set,
                message: message.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string),
            }),
        }
    }

    /// From Google's `extendedProperties.private`.
    pub(crate) fn from_google(private: &HashMap<String, String>) -> Option<Mark> {
        Mark::read(private.get(GOOGLE_HOLD).map(String::as_str), private.get(GOOGLE_MESSAGE).map(String::as_str))
    }

    /// From Graph's `singleValueExtendedProperties`, id and value. Graph
    /// may give the id in another case than it was written in.
    pub(crate) fn from_graph<'a>(properties: impl Iterator<Item = (&'a str, &'a str)> + Clone) -> Option<Mark> {
        let value = |id: &str| properties.clone().find(|(i, _)| i.eq_ignore_ascii_case(id)).map(|(_, v)| v);
        Mark::read(value(GRAPH_HOLD), value(GRAPH_MESSAGE))
    }

    /// From the values of a VEVENT's `X-JOLTA-HOLD` and `X-JOLTA-HOLD-MESSAGE`.
    pub(crate) fn from_lines(hold: Option<&str>, message: Option<&str>) -> Option<Mark> {
        Mark::read(hold, message)
    }

    fn values(&self) -> (&str, Option<&str>) {
        match self {
            Mark::Hold { set, message } => (set.as_str(), message.as_deref()),
            Mark::Away => (AWAY, None),
        }
    }

    /// Google's `extendedProperties.private`.
    pub fn google_private(&self) -> Value {
        let (hold, message) = self.values();
        let mut private = json!({ GOOGLE_HOLD: hold });
        if let Some(m) = message {
            private[GOOGLE_MESSAGE] = m.into();
        }
        private
    }

    /// Graph's `singleValueExtendedProperties`.
    pub fn graph_properties(&self) -> Value {
        let (hold, message) = self.values();
        let mut properties = vec![json!({ "id": GRAPH_HOLD, "value": hold })];
        properties.extend(message.map(|m| json!({ "id": GRAPH_MESSAGE, "value": m })));
        Value::Array(properties)
    }

    /// The VEVENT's property lines, values escaped as TEXT.
    pub fn caldav_lines(&self) -> Vec<String> {
        let (hold, message) = self.values();
        let escape = |v: &str| v.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n");
        let mut lines = vec![format!("{CALDAV_HOLD}:{}", escape(hold))];
        lines.extend(message.map(|m| format!("{CALDAV_MESSAGE}:{}", escape(m))));
        lines
    }

    pub fn hold_set(&self) -> Option<&HoldSetId> {
        match self {
            Mark::Hold { set, .. } => Some(set),
            Mark::Away => None,
        }
    }
}

/// `$expand` for Graph's event requests, so they return the two markers.
pub(crate) fn graph_expand() -> String {
    let filter = format!("id eq '{GRAPH_HOLD}' or id eq '{GRAPH_MESSAGE}'");
    let encoded: String = filter
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'\'' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("$expand=singleValueExtendedProperties($filter={encoded})")
}

text_enum!(SlotState {
    Pending => "pending",
    Held => "held",
    Booked => "booked",
    Taken => "taken",
    Released => "released",
    Gone => "gone",
});

/// One time a mail offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldSlot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// The line of the mail that offers it.
    pub line: String,
    pub state: SlotState,
    /// The hold event's remote id, once it was made.
    pub remote_id: Option<String>,
}

/// The holds for one mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldSet {
    pub id: HoldSetId,
    /// The account that sends the mail.
    pub account: AccountId,
    /// The sent mail's Message-ID, once it left.
    pub message_id: Option<String>,
    /// The draft, while the mail isn't sent.
    pub draft: Option<i64>,
    pub recipients: Vec<String>,
    pub subject: String,
    pub calendar: CalendarId,
    /// When the mail left.
    pub sent: Option<DateTime<Utc>>,
    pub slots: Vec<HeldSlot>,
}

impl HoldSet {
    /// The set ends when no slot is held.
    pub fn is_open(&self) -> bool {
        self.slots.iter().any(|s| s.state == SlotState::Held)
    }
}

/// What Jolta knows after a sync round, as `decide` reads it.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// The sending account's address and its aliases.
    pub me: Vec<String>,
    /// Occurrences that block the user's time, from every shown calendar
    /// that counts as busy, on the offered days and the 7 after them. An
    /// occurrence of a series counts on its own.
    pub busy: Vec<Busy>,
    /// Requests that came to the sending account, one per occurrence.
    pub invitations: Vec<Invited>,
    /// The conversation's messages from other people.
    pub replies: Vec<Reply>,
    /// The set's own hold events as stored.
    pub holds: Vec<HoldEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// The organizer and the guests.
    pub people: Vec<String>,
    /// Any set's hold event, which books nothing and takes nothing.
    pub hold: bool,
    /// When the provider says the event was made.
    pub created: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invited {
    /// The message's sender.
    pub from: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub received: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub from: String,
    pub date: DateTime<Utc>,
}

/// A hold event of the set, as the last sync stored it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldEvent {
    pub remote_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Someone besides the user is on it.
    pub guests: bool,
}

/// What happened, for one slot or for the set. Slots are named by their
/// start as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Rule 9: the user moved the hold in another client; it stays held there.
    Moved { slot: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc> },
    /// A held slot leaves `Held`.
    Slot { slot: DateTime<Utc>, outcome: Outcome },
    /// Something the conversation line says; nothing changes by itself.
    Notice(Notice),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Booked(Booking),
    /// Rule 3: something without a recipient is there now.
    Taken,
    Released(Release),
    /// Rule 8: the hold was deleted elsewhere.
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Booking {
    /// Rule 1: an event with a recipient on it.
    Event { by: String },
    /// Rule 2: a recipient's invitation in mail.
    Invitation { by: String },
    /// Rule 5: the user added people to the hold.
    Meeting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Another slot was booked.
    OtherBooked,
    /// Rule 7: its start passed unbooked.
    Passed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Rule 4: "Ann booked Wed 15:00. Release the holds?"
    BookedElsewhere { by: String, start: DateTime<Utc>, end: DateTime<Utc> },
    /// Rule 6: "Ann replied".
    Replied { by: String },
}

impl Outcome {
    pub fn state(&self) -> SlotState {
        match self {
            Outcome::Booked(_) => SlotState::Booked,
            Outcome::Taken => SlotState::Taken,
            Outcome::Released(_) => SlotState::Released,
            Outcome::Gone => SlotState::Gone,
        }
    }

    /// Whether the hold event is to be deleted: not when it is gone
    /// already, or is the meeting now.
    pub fn deletes_hold(&self) -> bool {
        !matches!(self, Outcome::Gone | Outcome::Booked(Booking::Meeting))
    }
}

fn overlaps(a: (DateTime<Utc>, DateTime<Utc>), b: (DateTime<Utc>, DateTime<Utc>)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// What happened to `set` by `now`, from what a sync round left in `seen`.
/// Only held slots change, so a decision applied once isn't made again.
pub fn decide(set: &HoldSet, seen: &Seen, now: DateTime<Utc>) -> Vec<Decision> {
    let Some(sent) = set.sent else { return Vec::new() };
    let is_me = |a: &str| seen.me.iter().any(|m| m.eq_ignore_ascii_case(a));
    let recipient = |a: &str| !is_me(a) && set.recipients.iter().any(|r| r.eq_ignore_ascii_case(a));
    let by = |people: &[String]| people.iter().find(|p| recipient(p)).cloned();
    let blocking = || seen.busy.iter().filter(|b| !b.hold);

    let mut moves = Vec::new();
    let mut outcomes: Vec<(DateTime<Utc>, Option<Outcome>, (DateTime<Utc>, DateTime<Utc>))> = Vec::new();
    for slot in set.slots.iter().filter(|s| s.state == SlotState::Held) {
        let event = slot.remote_id.as_deref().map(|id| seen.holds.iter().find(|h| h.remote_id == id));
        let Some(event) = event else {
            outcomes.push((slot.start, None, (slot.start, slot.end)));
            continue;
        };
        let Some(event) = event else {
            outcomes.push((slot.start, Some(Outcome::Gone), (slot.start, slot.end)));
            continue;
        };
        let at = (event.start, event.end);
        if at != (slot.start, slot.end) {
            moves.push(Decision::Moved { slot: slot.start, start: event.start, end: event.end });
        }
        let booking = if event.guests {
            Some(Booking::Meeting)
        } else if let Some(by) = blocking().filter(|b| overlaps((b.start, b.end), at)).find_map(|b| by(&b.people)) {
            Some(Booking::Event { by })
        } else {
            seen.invitations
                .iter()
                .find(|i| i.received >= sent && recipient(&i.from) && overlaps((i.start, i.end), at))
                .map(|i| Booking::Invitation { by: i.from.clone() })
        };
        outcomes.push((slot.start, booking.map(Outcome::Booked), at));
    }

    let booked = set.slots.iter().any(|s| s.state == SlotState::Booked) || outcomes.iter().any(|(_, o, _)| matches!(o, Some(Outcome::Booked(_))));
    for (_, outcome, at) in outcomes.iter_mut() {
        if outcome.is_some() {
            continue;
        }
        *outcome = if booked {
            Some(Outcome::Released(Release::OtherBooked))
        } else if blocking().any(|b| overlaps((b.start, b.end), *at)) {
            Some(Outcome::Taken)
        } else if at.0 <= now {
            Some(Outcome::Released(Release::Passed))
        } else {
            None
        };
    }

    let still_held: Vec<(DateTime<Utc>, DateTime<Utc>)> = outcomes.iter().filter(|(_, o, _)| o.is_none()).map(|(_, _, at)| *at).collect();
    let mut notices = Vec::new();
    if !still_held.is_empty() {
        for b in blocking().filter(|b| b.created.is_some_and(|c| c > sent)) {
            let Some(by) = by(&b.people) else { continue };
            let notice = Notice::BookedElsewhere { by, start: b.start, end: b.end };
            if !still_held.iter().any(|at| overlaps((b.start, b.end), *at)) && !notices.contains(&notice) {
                notices.push(notice);
            }
        }
        let mut replied: Vec<&Reply> = seen.replies.iter().filter(|r| r.date > sent && recipient(&r.from)).collect();
        replied.sort_by_key(|r| r.date);
        for r in replied {
            let notice = Notice::Replied { by: r.from.clone() };
            if !notices.iter().any(|n| matches!(n, Notice::Replied { by } if by.eq_ignore_ascii_case(&r.from))) {
                notices.push(notice);
            }
        }
    }

    moves
        .into_iter()
        .chain(outcomes.into_iter().filter_map(|(slot, o, _)| o.map(|outcome| Decision::Slot { slot, outcome })))
        .chain(notices.into_iter().map(Decision::Notice))
        .collect()
}

/// When a span starts and ends, an all-day one from midnight on `here`.
fn instants(span: &Span, here: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    let midnight = |d: NaiveDate| tz::instant(d.and_time(NaiveTime::MIN), here);
    match span {
        Span::Timed { start, end } => (start.instant(here), end.instant(here)),
        Span::AllDay { start, end } => (midnight(*start), midnight(*end)),
    }
}

/// Everyone on an event, and the addresses the source marks as its own
/// account's: the organizer when it is that account, and its attendee.
fn people(e: &EventData) -> (Vec<String>, Vec<String>) {
    let organizer = e.organizer.iter().map(|o| (o.address.clone(), e.organizer_is_me));
    let attendees = e.attendees.iter().map(|a| (a.person.address.clone(), a.is_me));
    let everyone: Vec<(String, bool)> = organizer.chain(attendees).collect();
    let mine = everyone.iter().filter(|(_, me)| *me).map(|(a, _)| a.clone()).collect();
    (everyone.into_iter().map(|(a, _)| a).collect(), mine)
}

/// What `decide` reads for `set`: the occurrences on the offered days and
/// the 7 after, a moved hold's day among them, the requests and the conversation's mail since the mail
/// left, and the set's hold events in its calendar. "Me" is the sending
/// account's address and the ones the copies in its own calendars mark as
/// its own; another of the user's accounts is someone else here.
pub fn seen(conn: &Connection, blobs: &Blobs, set: &HoldSet, here: Tz) -> Result<Seen> {
    let address = store::account(conn, set.account)?.address;
    let mut seen = Seen { me: vec![address.clone()], ..Seen::default() };
    let own: Vec<CalendarId> = store::account_calendars(conn, set.account)?.into_iter().filter(|c| c.is_own(&address)).map(|c| c.id).collect();
    seen.holds = store::hold_events(conn, Some(&set.id))?
        .into_iter()
        .filter(|m| m.calendar == set.calendar && m.event.status != EventStatus::Cancelled)
        .map(|m| {
            let (start, end) = instants(&m.event.span, here);
            HoldEvent { remote_id: m.remote_id, start, end, guests: m.event.attendees.iter().any(|a| !a.is_me) }
        })
        .collect();
    let times = set.slots.iter().map(|s| (s.start, s.end)).chain(seen.holds.iter().map(|h| (h.start, h.end)));
    let (Some(first), Some(last)) = (times.clone().map(|t| t.0).min(), times.map(|t| t.1).max()) else { return Ok(seen) };
    let (from, to) = (first.with_timezone(&here).date_naive(), last.with_timezone(&here).date_naive() + Duration::days(8));
    let (start, end) = instants(&Span::AllDay { start: from, end: to }, here);
    let window = Window { start, end };
    for o in store::occurrences(conn, from, to, here)? {
        let (Some(b), Some(e)) = (freebusy::stored_busy(&o, here), store::event_data(conn, o.event_id)?) else { continue };
        let (people, mine) = people(&e);
        if own.contains(&o.calendar_id) {
            seen.me.extend(mine);
        }
        seen.busy.push(Busy { start: b.start, end: b.end, people, hold: e.mark.as_ref().is_some_and(|m| m.hold_set().is_some()), created: e.created });
    }
    let Some(sent) = set.sent else { return Ok(seen) };
    for (from, received, blob) in store::requests_since(conn, set.account, sent)? {
        let Some(raw) = blob.and_then(|b| blobs.get(&b).ok()) else { continue };
        let events: Vec<EventData> = invitation::find(&raw, &seen.me).into_iter().filter(|i| i.method == Method::Request && i.dated).map(|i| i.event).collect();
        // A series' moved occurrences come in the same mail as overrides of its master.
        for e in &events {
            let has_master = events.iter().any(|m| m.uid == e.uid && m.recurrence_id.is_none());
            if e.recurrence_id.is_some() && has_master {
                continue;
            }
            let overrides: Vec<EventData> = events.iter().filter(|o| o.uid == e.uid && o.recurrence_id.is_some() && e.recurrence_id.is_none()).cloned().collect();
            for o in recur::expand(e, &overrides, window, here) {
                let (start, end) = instants(&o.span, here);
                seen.invitations.push(Invited { from: from.clone(), start, end, received });
            }
        }
    }
    if let Some(mail) = set.message_id.as_deref().map(|m| store::sent_mail(conn, m)).transpose()?.flatten() {
        seen.replies = store::thread_mail_since(conn, mail.thread, sent)?.into_iter().map(|(from, date)| Reply { from, date }).collect();
    }
    Ok(seen)
}

/// Builds the sets this Mac doesn't know from the hold events sync found,
/// each from the sent mail its marker names, so either of the user's Macs
/// can release them. A set whose mail isn't stored here waits.
pub fn adopt(conn: &Connection, now: DateTime<Utc>, here: Tz) -> Result<Vec<HoldSetId>> {
    let known: HashSet<HoldSetId> = store::hold_sets(conn)?.into_iter().map(|s| s.id).collect();
    let mut found: BTreeMap<HoldSetId, Vec<store::MarkedEvent>> = BTreeMap::new();
    for m in store::hold_events(conn, None)? {
        if let Some(set) = m.event.mark.as_ref().and_then(Mark::hold_set).filter(|s| !known.contains(*s)) {
            found.entry(set.clone()).or_default().push(m);
        }
    }
    let mut adopted = Vec::new();
    for (id, events) in found {
        let message = events.iter().find_map(|e| match &e.event.mark {
            Some(Mark::Hold { message: Some(m), .. }) => Some(m.clone()),
            _ => None,
        });
        let Some(mail) = message.as_deref().map(|m| store::sent_mail(conn, m)).transpose()?.flatten() else { continue };
        let calendar = events[0].calendar;
        let mut slots: Vec<HeldSlot> = Vec::new();
        for e in events.iter().filter(|e| e.calendar == calendar && e.event.status != EventStatus::Cancelled) {
            let (start, end) = instants(&e.event.span, here);
            if !slots.iter().any(|s| s.start == start) {
                slots.push(HeldSlot { start, end, line: String::new(), state: SlotState::Held, remote_id: Some(e.remote_id.clone()) });
            }
        }
        slots.sort_by_key(|s| s.start);
        let recipients = compose::parse_addresses(&mail.to).into_iter().chain(compose::parse_addresses(&mail.cc)).map(|(_, a)| a).collect();
        let set = HoldSet { id: id.clone(), account: mail.account, message_id: message, draft: None, recipients, subject: mail.subject, calendar, sent: Some(mail.date), slots };
        store::insert_hold_set(conn, &set, now)?;
        adopted.push(id);
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;

    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc()
    }

    const ANN: &str = "ann@example.com";
    const ME: &str = "me@example.com";
    /// Tue, Wed and Thu at the times the mail offers.
    const SLOTS: [(&str, &str, &str); 3] = [("h1", "2026-11-10 15:00", "2026-11-10 15:30"), ("h2", "2026-11-11 10:00", "2026-11-11 10:30"), ("h3", "2026-11-12 14:00", "2026-11-12 14:30")];

    fn set() -> HoldSet {
        HoldSet {
            id: HoldSetId::parse("5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10").unwrap(),
            account: AccountId(1),
            message_id: Some("<m1@example.com>".into()),
            draft: None,
            recipients: vec![ANN.into()],
            subject: "Planning".into(),
            calendar: CalendarId(1),
            sent: Some(t("2026-11-09 09:00")),
            slots: SLOTS
                .iter()
                .map(|(id, s, e)| HeldSlot { start: t(s), end: t(e), line: format!("{s} to {e}"), state: SlotState::Held, remote_id: Some(id.to_string()) })
                .collect(),
        }
    }

    fn seen() -> Seen {
        Seen {
            me: vec![ME.into()],
            holds: SLOTS.iter().map(|(id, s, e)| HoldEvent { remote_id: id.to_string(), start: t(s), end: t(e), guests: false }).collect(),
            ..Seen::default()
        }
    }

    fn busy(from: &str, to: &str, people: &[&str]) -> Busy {
        Busy { start: t(from), end: t(to), people: people.iter().map(|p| p.to_string()).collect(), hold: false, created: Some(t("2026-11-09 10:00")) }
    }

    fn now() -> DateTime<Utc> {
        t("2026-11-09 12:00")
    }

    /// Each slot's outcome by its hold's remote id.
    fn outcomes(decisions: &[Decision]) -> Vec<(&'static str, Outcome)> {
        decisions
            .iter()
            .filter_map(|d| match d {
                Decision::Slot { slot, outcome } => SLOTS.iter().find(|(_, s, _)| t(s) == *slot).map(|(id, _, _)| (*id, outcome.clone())),
                _ => None,
            })
            .collect()
    }

    fn notices(decisions: &[Decision]) -> Vec<Notice> {
        decisions.iter().filter_map(|d| if let Decision::Notice(n) = d { Some(n.clone()) } else { None }).collect()
    }

    /// The set after the decisions, as the store applies them.
    fn applied(mut set: HoldSet, decisions: &[Decision]) -> HoldSet {
        for d in decisions {
            match d {
                Decision::Slot { slot, outcome } => set.slots.iter_mut().filter(|s| s.start == *slot).for_each(|s| s.state = outcome.state()),
                Decision::Moved { slot, start, end } => set.slots.iter_mut().filter(|s| s.start == *slot).for_each(|s| (s.start, s.end) = (*start, *end)),
                Decision::Notice(_) => {}
            }
        }
        set
    }

    const OTHERS_RELEASED: Outcome = Outcome::Released(Release::OtherBooked);

    #[test]
    fn rule_1_a_meeting_with_a_recipient_on_a_held_time_books_it_and_releases_the_rest() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-11 10:00", "2026-11-11 10:30", &["bo@example.com", "Ann@Example.COM"]));
        let booked = Outcome::Booked(Booking::Event { by: "Ann@Example.COM".into() });
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h1", OTHERS_RELEASED), ("h2", booked.clone()), ("h3", OTHERS_RELEASED)]);
        assert!(booked.deletes_hold(), "the meeting holds the time now");
    }

    #[test]
    fn rule_2_a_recipients_invitation_in_mail_books_and_a_strangers_is_ignored() {
        let mut seen = seen();
        let invited = |from: &str, received: &str| Invited { from: from.into(), start: t("2026-11-10 15:00"), end: t("2026-11-10 16:00"), received: t(received) };
        seen.invitations = vec![invited("eve@example.com", "2026-11-09 11:00"), invited(ANN, "2026-11-08 11:00")];
        assert_eq!(decide(&set(), &seen, now()), [], "a stranger reusing the subject, and Ann's invitation from before the mail");
        seen.invitations.push(invited(ANN, "2026-11-09 11:00"));
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h1", Outcome::Booked(Booking::Invitation { by: ANN.into() })), ("h2", OTHERS_RELEASED), ("h3", OTHERS_RELEASED)]);
    }

    #[test]
    fn rule_3_something_without_a_recipient_takes_only_its_slot() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-12 13:00", "2026-11-12 14:15", &["bo@example.com"]));
        seen.busy.push(busy("2026-11-11 09:00", "2026-11-11 10:00", &[]));
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h3", Outcome::Taken)], "an event that ends as a slot starts takes nothing");
    }

    #[test]
    fn an_all_day_event_over_a_held_slot_takes_it() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-10 23:00", "2026-11-11 23:00", &[]));
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h2", Outcome::Taken)]);
    }

    #[test]
    fn rule_4_a_new_meeting_with_a_recipient_at_another_time_only_asks() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-13 15:00", "2026-11-13 16:00", &[ANN]));
        let before = Busy { created: Some(t("2026-11-02 08:00")), ..busy("2026-11-16 15:00", "2026-11-16 16:00", &[ANN]) };
        let unknown = Busy { created: None, ..busy("2026-11-17 15:00", "2026-11-17 16:00", &[ANN]) };
        seen.busy.extend([before, unknown]);
        let decisions = decide(&set(), &seen, now());
        assert_eq!(outcomes(&decisions), []);
        assert_eq!(notices(&decisions), [Notice::BookedElsewhere { by: ANN.into(), start: t("2026-11-13 15:00"), end: t("2026-11-13 16:00") }]);
    }

    #[test]
    fn rule_5_a_hold_with_guests_is_the_meeting() {
        let mut seen = seen();
        seen.holds[1].guests = true;
        let decisions = decide(&set(), &seen, now());
        assert_eq!(outcomes(&decisions), [("h1", OTHERS_RELEASED), ("h2", Outcome::Booked(Booking::Meeting)), ("h3", OTHERS_RELEASED)]);
        assert!(!Outcome::Booked(Booking::Meeting).deletes_hold(), "the hold is the meeting now");
    }

    #[test]
    fn rule_6_a_reply_from_a_recipient_changes_nothing_and_says_so() {
        let mut seen = seen();
        seen.replies = vec![
            Reply { from: "eve@example.com".into(), date: t("2026-11-09 10:00") },
            Reply { from: ANN.into(), date: t("2026-11-08 10:00") },
            Reply { from: ME.into(), date: t("2026-11-09 10:30") },
        ];
        assert_eq!(decide(&set(), &seen, now()), [], "a stranger, Ann before the mail, the user");
        seen.replies.push(Reply { from: ANN.into(), date: t("2026-11-09 11:00") });
        seen.replies.push(Reply { from: "ANN@example.com".into(), date: t("2026-11-09 11:30") });
        assert_eq!(decide(&set(), &seen, now()), [Decision::Notice(Notice::Replied { by: ANN.into() })]);
    }

    #[test]
    fn rule_7_a_slot_whose_start_passed_is_released_and_the_set_ends_with_the_last() {
        let decisions = decide(&set(), &seen(), t("2026-11-10 15:00"));
        assert_eq!(outcomes(&decisions), [("h1", Outcome::Released(Release::Passed))]);
        let later = applied(set(), &decisions);
        assert!(later.is_open());
        let all = decide(&later, &seen(), t("2026-11-12 14:00"));
        assert_eq!(outcomes(&all), [("h2", Outcome::Released(Release::Passed)), ("h3", Outcome::Released(Release::Passed))]);
        assert!(!applied(later, &all).is_open());
    }

    #[test]
    fn rule_8_a_hold_deleted_elsewhere_is_gone_and_nothing_else_changes() {
        let mut seen = seen();
        seen.holds.remove(1);
        let decisions = decide(&set(), &seen, now());
        assert_eq!(outcomes(&decisions), [("h2", Outcome::Gone)]);
        assert!(!Outcome::Gone.deletes_hold());
    }

    #[test]
    fn rule_9_a_moved_hold_is_held_at_its_new_time() {
        let mut seen = seen();
        (seen.holds[2].start, seen.holds[2].end) = (t("2026-11-13 09:00"), t("2026-11-13 09:30"));
        seen.busy.push(busy("2026-11-12 14:00", "2026-11-12 14:30", &[]));
        let decisions = decide(&set(), &seen, now());
        assert_eq!(decisions, [Decision::Moved { slot: t("2026-11-12 14:00"), start: t("2026-11-13 09:00"), end: t("2026-11-13 09:30") }], "the old time is free to take");
        seen.busy.push(busy("2026-11-13 09:00", "2026-11-13 10:00", &[ANN]));
        let booked = Decision::Slot { slot: t("2026-11-13 09:00"), outcome: Outcome::Booked(Booking::Event { by: ANN.into() }) };
        assert!(decide(&applied(set(), &decisions), &seen, now()).contains(&booked));
    }

    #[test]
    fn the_users_alias_among_the_recipients_books_nothing() {
        let mut set = set();
        set.recipients.push("Me.Alias@example.com".into());
        let mut seen = seen();
        seen.me.push("me.alias@example.com".into());
        seen.busy.push(busy("2026-11-10 15:00", "2026-11-10 15:30", &["me.alias@example.com"]));
        assert_eq!(outcomes(&decide(&set, &seen, now())), [("h1", Outcome::Taken)]);
    }

    #[test]
    fn the_same_booking_seen_in_two_rounds_is_applied_once() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-11 10:00", "2026-11-11 10:30", &[ANN]));
        let first = decide(&set(), &seen, now());
        assert_eq!(decide(&applied(set(), &first), &seen, now()), []);
        let booked_only: Vec<Decision> = first.into_iter().filter(|d| matches!(d, Decision::Slot { outcome: Outcome::Booked(_), .. })).collect();
        let after_a_failure = applied(set(), &booked_only);
        assert_eq!(outcomes(&decide(&after_a_failure, &seen, now())), [("h1", OTHERS_RELEASED), ("h3", OTHERS_RELEASED)], "releases that failed come again");
    }

    #[test]
    fn holds_take_nothing_and_a_set_not_sent_decides_nothing() {
        let mut seen = seen();
        seen.busy.push(Busy { hold: true, ..busy("2026-11-10 15:00", "2026-11-10 15:30", &[]) });
        assert_eq!(decide(&set(), &seen, now()), [], "another set's hold at the same time");
        seen.busy.push(busy("2026-11-10 15:00", "2026-11-10 15:30", &[ANN]));
        assert_eq!(decide(&HoldSet { sent: None, ..set() }, &seen, now()), []);
    }

    #[test]
    fn markers_carry_the_set_and_the_message() {
        let mark = Mark::Hold { set: set().id, message: Some("<m1@example.com>".into()) };
        assert_eq!(mark.google_private(), json!({ "joltaHold": "5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10", "joltaHoldMessage": "<m1@example.com>" }));
        assert_eq!(mark.graph_properties()[1], json!({ "id": GRAPH_MESSAGE, "value": "<m1@example.com>" }));
        assert_eq!(Mark::Away.caldav_lines(), ["X-JOLTA-HOLD:away"]);
        assert_eq!(HoldSetId::parse("5b2f8c1e0d4a4e8b9a613c7e2d9f4a10"), None);
        assert_eq!(serde_json::to_value(&mark).unwrap(), json!({ "is": "hold", "set": "5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10", "message": "<m1@example.com>" }));
    }

    mod stored {
        use chrono::TimeZone;
        use chrono_tz::Europe::Brussels;

        use super::*;
        use crate::calendar::invitation::tests::{import, jolta};
        use crate::calendar::series::Horizon;
        use crate::calendar::{Access, CalendarSource, ical};

        const SET: &str = "5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10";
        const SENT: &str = "From: Me <me@example.com>\r\nTo: Ann <ann@example.com>\r\nCc: Bo <bo@example.com>\r\nSubject: Planning\r\n\
            Date: Mon, 09 Nov 2026 09:00:00 +0000\r\nMessage-ID: <m1@example.com>\r\n\r\nTue 16:00 to 16:30\r\n";

        fn put(j: &crate::Jolta, calendar: CalendarId, remote_id: &str, vevent: &str) {
            let ics = format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{remote_id}@example.com\r\nDTSTAMP:20261101T080000Z\r\n{vevent}END:VEVENT\r\nEND:VCALENDAR\r\n");
            let events = ical::read(&ics, &["me@example.com".into()]).unwrap();
            let horizon = Horizon::around(Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(), Brussels);
            j.store.write(|tx| store::apply_item(tx, calendar, remote_id, None, &events, None, &horizon)).unwrap();
        }

        fn holds(j: &crate::Jolta, calendar: CalendarId, message: &str) {
            for (id, s, e) in SLOTS {
                let ics = |at: &str| t(at).format("%Y%m%dT%H%M00Z").to_string();
                let lines = Mark::Hold { set: HoldSetId::parse(SET).unwrap(), message: Some(message.into()) }.caldav_lines().join("\r\n");
                put(j, calendar, id, &format!("DTSTART:{}\r\nDTEND:{}\r\nSUMMARY:Hold: Planning\r\nSTATUS:TENTATIVE\r\n{lines}\r\n", ics(s), ics(e)));
            }
        }

        #[test]
        fn the_store_gives_decide_what_a_round_left() {
            let (j, work) = jolta("holds-seen");
            let other = |remote_id: &str| store::RemoteCalendar {
                remote_id: remote_id.into(),
                name: remote_id.into(),
                color: None,
                zone: None,
                access: Access::Owner,
                primary: remote_id == "/cal/work/",
                refresh: None,
                video: None,
            };
            let free = j
                .store
                .write(|tx| {
                    let listed = store::sync_calendar_list(tx, AccountId(1), CalendarSource::CalDav, &[other("/cal/work/"), other("/cal/free/")])?;
                    let free = listed.iter().find(|c| c.remote_id == "/cal/free/").unwrap().id;
                    store::set_calendar_shown(tx, free, true)?;
                    store::set_calendar_busy(tx, free, false)?;
                    Ok(free)
                })
                .unwrap();
            holds(&j, work, "<m1@example.com>");
            let ann = "ORGANIZER:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:me@example.com\r\n";
            put(&j, free, "not-busy", &format!("DTSTART:20261110T150000Z\r\nDTEND:20261110T153000Z\r\n{ann}"));
            put(&j, work, "declined", "DTSTART:20261110T150000Z\r\nDTEND:20261110T153000Z\r\nORGANIZER:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=DECLINED:mailto:me@example.com\r\n");
            put(&j, work, "offsite", "DTSTART;VALUE=DATE:20261111\r\nDTEND;VALUE=DATE:20261112\r\nSUMMARY:Offsite\r\n");
            let mut set = set();
            set.slots[0].line = "Tue 10 Nov 16:00 to 16:30".into();
            j.store.write(|tx| store::insert_hold_set(tx, &set, t("2026-11-09 09:00"))).unwrap();
            assert_eq!(j.store.read(store::hold_sets).unwrap(), [set.clone()], "a set reads back as stored");

            let seen_now = || j.store.read(|c| super::super::seen(c, &j.blobs, &set, Brussels)).unwrap();
            let seen = seen_now();
            assert_eq!(seen.holds, super::seen().holds, "the hold events by remote id");
            let not_holds: Vec<(DateTime<Utc>, DateTime<Utc>)> = seen.busy.iter().filter(|b| !b.hold).map(|b| (b.start, b.end)).collect();
            assert_eq!(not_holds, [(t("2026-11-10 23:00"), t("2026-11-11 23:00"))], "the offsite from midnight here; not the declined meeting or a calendar that isn't busy");
            assert_eq!(decide(&set, &seen, now()), [Decision::Slot { slot: t("2026-11-11 10:00"), outcome: Outcome::Taken }]);

            put(&j, work, "weekly", &format!("DTSTART:20261105T140000Z\r\nDTEND:20261105T143000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\n{ann}"));
            import(&j, SENT.as_bytes());
            import(&j, b"From: Ann <ann@example.com>\r\nTo: me@example.com\r\nSubject: Re: Planning\r\nDate: Mon, 09 Nov 2026 10:00:00 +0000\r\n\
                Message-ID: <r1@example.com>\r\nIn-Reply-To: <m1@example.com>\r\nReferences: <m1@example.com>\r\n\r\nWednesday works.\r\n");
            import(&j, b"From: Ann <ann@example.com>\r\nTo: me@example.com\r\nSubject: Invitation: Planning\r\nDate: Mon, 09 Nov 2026 11:00:00 +0000\r\n\
                Message-ID: <i1@example.com>\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\n\
                BEGIN:VEVENT\r\nUID:i1@example.com\r\nDTSTAMP:20261109T110000Z\r\nDTSTART:20261110T150000Z\r\nDTEND:20261110T153000Z\r\nSUMMARY:Planning\r\n\
                ORGANIZER:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
            let seen = seen_now();
            let weekly: Vec<(DateTime<Utc>, Vec<String>)> = seen.busy.iter().filter(|b| !b.people.is_empty()).map(|b| (b.start, b.people.clone())).collect();
            let people = vec![ANN.to_string(), ME.to_string()];
            assert_eq!(weekly, [(t("2026-11-12 14:00"), people.clone()), (t("2026-11-19 14:00"), people)], "each occurrence on its own");
            assert_eq!(seen.invitations, [Invited { from: ANN.into(), start: t("2026-11-10 15:00"), end: t("2026-11-10 15:30"), received: t("2026-11-09 11:00") }]);
            assert_eq!(seen.replies.iter().map(|r| (r.from.as_str(), r.date)).collect::<Vec<_>>(), [(ANN, t("2026-11-09 10:00"))]);
            let booked = |by: &str| Outcome::Booked(Booking::Event { by: by.into() });
            assert_eq!(outcomes(&decide(&set, &seen, now())), [("h1", Outcome::Booked(Booking::Invitation { by: ANN.into() })), ("h2", OTHERS_RELEASED), ("h3", booked(ANN))]);
            let _ = std::fs::remove_dir_all(&j.data_dir);
        }

        /// The same meeting in two of the user's accounts' calendars is
        /// stored once, as the organizer's copy, whose own address is that
        /// account's and not the sending account's (check 1).
        #[test]
        fn another_of_the_users_accounts_is_someone_else() {
            let (j, work) = jolta("holds-accounts");
            let google = j
                .store
                .write(|tx| {
                    tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (2, 'gmail', 'me2@example.net', 'me2', 'a2', 0, 1);")?;
                    let primary = store::RemoteCalendar {
                        remote_id: "me2@example.net".into(),
                        name: "me2@example.net".into(),
                        color: None,
                        zone: None,
                        access: Access::Owner,
                        primary: true,
                        refresh: None,
                        video: None,
                    };
                    Ok(store::sync_calendar_list(tx, AccountId(2), CalendarSource::Google, &[primary])?[0].id)
                })
                .unwrap();
            holds(&j, work, "<m1@example.com>");
            let meeting = "DTSTART:20261110T150000Z\r\nDTEND:20261110T153000Z\r\nORGANIZER:mailto:me2@example.net\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:me2@example.net\r\nATTENDEE:mailto:me@example.com\r\n";
            let ics = format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:shared@example.net\r\n{meeting}END:VEVENT\r\nEND:VCALENDAR\r\n");
            let horizon = Horizon::around(Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(), Brussels);
            j.store
                .write(|tx| {
                    store::apply_item(tx, google, "shared-g", None, &ical::read(&ics, &["me2@example.net".into()])?, None, &horizon)?;
                    store::apply_item(tx, work, "shared-w", None, &ical::read(&ics, &["me@example.com".into()])?, None, &horizon)
                })
                .unwrap();
            let set = HoldSet { recipients: vec!["me2@example.net".into()], ..set() };
            let seen = j.store.read(|c| super::super::seen(c, &j.blobs, &set, Brussels)).unwrap();
            assert_eq!(seen.me, [ME]);
            let booked = Outcome::Booked(Booking::Event { by: "me2@example.net".into() });
            assert_eq!(outcomes(&decide(&set, &seen, now())), [("h1", booked), ("h2", OTHERS_RELEASED), ("h3", OTHERS_RELEASED)]);
            let _ = std::fs::remove_dir_all(&j.data_dir);
        }

        /// Google marks a shared calendar's owner as the copy's `self`, so
        /// Ann's own meeting in her calendar shared with the user names her
        /// as the user there; she stays a recipient.
        #[test]
        fn a_shared_calendars_owner_is_no_alias() {
            let (j, work) = jolta("holds-shared");
            let shared = j
                .store
                .write(|tx| {
                    let cal = |remote_id: &str, access| store::RemoteCalendar {
                        remote_id: remote_id.into(),
                        name: remote_id.into(),
                        color: None,
                        zone: None,
                        access,
                        primary: remote_id == "/cal/work/",
                        refresh: None,
                        video: None,
                    };
                    let listed = store::sync_calendar_list(tx, AccountId(1), CalendarSource::CalDav, &[cal("/cal/work/", Access::Owner), cal("/cal/ann/", Access::Write)])?;
                    let shared = listed.iter().find(|c| c.remote_id == "/cal/ann/").unwrap().id;
                    store::set_calendar_shown(tx, shared, true)?;
                    Ok(shared)
                })
                .unwrap();
            holds(&j, work, "<m1@example.com>");
            let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ann-own@example.com\r\nDTSTART:20261110T150000Z\r\nDTEND:20261110T153000Z\r\n\
                ORGANIZER:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
            let horizon = Horizon::around(Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap(), Brussels);
            j.store.write(|tx| store::apply_item(tx, shared, "ann-own", None, &ical::read(ics, &[ANN.into()])?, None, &horizon)).unwrap();
            let seen = j.store.read(|c| super::super::seen(c, &j.blobs, &set(), Brussels)).unwrap();
            assert_eq!(seen.me, [ME]);
            assert_eq!(outcomes(&decide(&set(), &seen, now()))[0], ("h1", Outcome::Booked(Booking::Event { by: ANN.into() })));
            let _ = std::fs::remove_dir_all(&j.data_dir);
        }

        /// A series' moved occurrence comes in the same request as an
        /// override, and counts at its new time only.
        #[test]
        fn a_moved_occurrence_in_a_request_counts_at_its_new_time() {
            let (j, work) = jolta("holds-moved-request");
            holds(&j, work, "<m1@example.com>");
            import(&j, b"From: Ann <ann@example.com>\r\nTo: me@example.com\r\nSubject: Invitation: Weekly\r\nDate: Mon, 09 Nov 2026 11:00:00 +0000\r\n\
                Message-ID: <i2@example.com>\r\nMIME-Version: 1.0\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\n\
                BEGIN:VEVENT\r\nUID:w1@example.com\r\nDTSTAMP:20261109T110000Z\r\nDTSTART:20261103T150000Z\r\nDTEND:20261103T153000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\n\
                SUMMARY:Weekly\r\nORGANIZER:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nEND:VEVENT\r\n\
                BEGIN:VEVENT\r\nUID:w1@example.com\r\nRECURRENCE-ID:20261110T150000Z\r\nDTSTAMP:20261109T110000Z\r\nDTSTART:20261113T090000Z\r\nDTEND:20261113T093000Z\r\n\
                SUMMARY:Weekly\r\nORGANIZER:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
            let seen = j.store.read(|c| super::super::seen(c, &j.blobs, &set(), Brussels)).unwrap();
            let times: Vec<DateTime<Utc>> = seen.invitations.iter().map(|i| i.start).collect();
            assert_eq!(times, [t("2026-11-13 09:00"), t("2026-11-17 15:00")]);
            assert_eq!(decide(&set(), &seen, now()), []);
            let _ = std::fs::remove_dir_all(&j.data_dir);
        }

        #[test]
        fn another_macs_holds_are_built_from_the_sent_mail_their_marker_names() {
            let (j, work) = jolta("holds-adopt");
            holds(&j, work, "<m1@example.com>");
            let adopt = || j.store.write(|tx| adopt(tx, t("2026-11-09 12:00"), Brussels)).unwrap();
            assert_eq!(adopt(), [], "the sent mail isn't here yet");
            j.store
                .write(|tx| {
                    tx.execute_batch(
                        "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (2, 'imap', 'me2@example.net', 'me2', 'a2', 0);
                         INSERT INTO mailbox (id, account_id, remote_id, name, role) VALUES (2, 2, 'INBOX', 'Inbox', 'inbox');",
                    )?;
                    Ok(())
                })
                .unwrap();
            crate::import::import_raw(&j, AccountId(2), crate::MailboxId(2), SENT.as_bytes()).unwrap();
            assert_eq!(adopt(), [], "a copy another of the user's accounts received");
            import(&j, SENT.as_bytes());
            assert_eq!(adopt(), [HoldSetId::parse(SET).unwrap()]);
            let sets = j.store.read(store::hold_sets).unwrap();
            let lineless = |mut s: HoldSet| {
                s.slots.iter_mut().for_each(|slot| slot.line.clear());
                s.recipients.push("bo@example.com".into());
                s
            };
            assert_eq!(sets, [lineless(set())]);
            assert_eq!(adopt(), [], "a set this Mac knows");
            let _ = std::fs::remove_dir_all(&j.data_dir);
        }
    }
}
