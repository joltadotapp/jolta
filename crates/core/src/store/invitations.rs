//! The events that calendar parts in mail name, and what the store knows
//! about them: the copy in the user's calendars and whether the user ever
//! wrote to the sender.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::calendar::{moment_from_text, moment_text};
use crate::calendar::invitation::{Invitation, Method, end_of};
use crate::calendar::recur::Source;
use crate::calendar::series::{self, copy_rank};
use crate::calendar::update::holds;
use crate::calendar::{CalendarId, CalendarSource, EventData, EventId, EventStatus, Moment, Response, tz};
use crate::model::*;
use crate::{Error, Result, compose};

/// Replaces the invitation rows of a message with the events of its body.
pub fn set_invitations(conn: &Connection, id: MessageId, invitations: &[Invitation]) -> Result<()> {
    conn.prepare_cached("DELETE FROM invitation WHERE message_id = ?1")?.execute([id])?;
    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO invitation (message_id, uid, recurrence_id, sequence, method, organizer, ends, timing)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    let here = tz::here();
    for i in invitations {
        let e = &i.event;
        insert.execute(params![
            id,
            e.uid,
            e.recurrence_id.as_ref().map(moment_text).unwrap_or_default(),
            e.sequence,
            i.method,
            e.organizer.as_ref().map(|o| o.address.as_str()).unwrap_or_default(),
            i.ends(here).map_or(0, |t| t.timestamp()),
            timing(i),
        ])?;
    }
    Ok(())
}

/// What places the occurrences of a repeating event or a changed
/// occurrence, as JSON; '' for a single event and for a message that gives
/// no time.
fn timing(i: &Invitation) -> String {
    let e = &i.event;
    if !i.dated || (e.recurrence.is_none() && e.recurrence_id.is_none()) {
        return String::new();
    }
    let placed = EventData {
        recurrence_id: e.recurrence_id,
        this_and_future: e.this_and_future,
        recurrence: e.recurrence.clone(),
        status: e.status,
        sequence: e.sequence,
        ..EventData::new(e.uid.clone(), e.span)
    };
    serde_json::to_string(&placed).expect("an event serializes")
}

pub fn invitation_scan_batch(conn: &Connection, limit: usize) -> Result<Vec<(MessageId, String)>> {
    let mut stmt = conn.prepare(
        "SELECT s.message_id, coalesce(m.blob, '') FROM invitation_scan s LEFT JOIN message m ON m.id = s.message_id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records what the scan found in a message and takes it off the list. A
/// message deleted before its turn is only taken off.
pub fn finish_invitation_scan(conn: &Connection, id: MessageId, invitations: &[Invitation]) -> Result<()> {
    let exists: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM message WHERE id = ?1)", [id], |r| r.get(0))?;
    if exists {
        set_invitations(conn, id, invitations)?;
    }
    conn.execute("DELETE FROM invitation_scan WHERE message_id = ?1", [id])?;
    Ok(())
}

/// The copy of an invitation's event in the user's shown calendars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub event_id: EventId,
    pub calendar_id: CalendarId,
    /// The calendar's source.
    pub source: CalendarSource,
    pub event: EventData,
}

/// The event with this UID and recurrence id in a shown calendar, of
/// `account` only when one is given. A recurrence id matches the same
/// moment written in another zone, as `Moment::names` decides. A meeting in
/// several calendars gives the copy the views show.
pub fn stored_event(conn: &Connection, uid: &str, recurrence_id: Option<&Moment>, account: Option<AccountId>, here: Tz) -> Result<Option<StoredEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.calendar_id, c.source, c.is_default, e.data
         FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.uid = ?1 AND (e.recurrence_id = '') = ?2 AND c.shown AND (?3 IS NULL OR c.account_id = ?3)",
    )?;
    let rows = stmt.query_map(params![uid, recurrence_id.is_none(), account], |r| {
        let (event_id, calendar_id, source, is_default): (EventId, CalendarId, CalendarSource, bool) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        Ok((copy_rank(uid, source, is_default, calendar_id), event_id, calendar_id, source, r.get::<_, String>(4)?))
    })?;
    let mut best: Option<((bool, bool, i64), StoredEvent)> = None;
    for row in rows {
        let (rank, event_id, calendar_id, source, data) = row?;
        if best.as_ref().is_some_and(|(b, _)| *b <= rank) {
            continue;
        }
        let event: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {event_id}: {e}")))?;
        if recurrence_id.is_some_and(|rid| !event.recurrence_id.is_some_and(|stored| rid.names(&stored, here))) {
            continue;
        }
        best = Some((rank, StoredEvent { event_id, calendar_id, source, event }));
    }
    Ok(best.map(|(_, e)| e))
}

/// Every stored event of the series with this UID, the master and its
/// overrides, from the one calendar the views show it from, of `account`
/// only when one is given.
pub fn stored_series(conn: &Connection, uid: &str, account: Option<AccountId>) -> Result<Vec<EventData>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.calendar_id, c.source, c.is_default, e.data
         FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.uid = ?1 AND c.shown AND (?2 IS NULL OR c.account_id = ?2)",
    )?;
    let rows = stmt
        .query_map(params![uid, account], |r| {
            let (event_id, calendar_id, source, is_default): (EventId, CalendarId, CalendarSource, bool) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            Ok((copy_rank(uid, source, is_default, calendar_id), event_id, calendar_id, r.get::<_, String>(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(best) = rows.iter().map(|(rank, _, calendar, _)| (*rank, *calendar)).min() else { return Ok(Vec::new()) };
    rows.into_iter()
        .filter(|(_, _, calendar, _)| *calendar == best.1)
        .map(|(_, id, _, data)| serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}"))))
        .collect()
}

/// An answer as the user gave it from Jolta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GivenAnswer {
    pub answer: Response,
    /// Empty without one.
    pub note: String,
    /// False for "Don't send a reply".
    pub notify: bool,
}

/// The reply mail an answer queued, while the outbox still means to send
/// it or has sent it. One that failed for good or was taken back is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyMail {
    Waiting(i64),
    Sent(i64),
}

/// Keeps the answer the user gave an event's invitation on an account:
/// the whole event, or with `recurrence_id` one occurrence, with the outbox
/// row of its reply mail.
pub fn remember_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>, given: &GivenAnswer, reply: Option<i64>) -> Result<()> {
    conn.execute(
        "INSERT INTO invitation_answer (account_id, uid, recurrence_id, answer, note, notify, reply) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (account_id, uid, recurrence_id) DO UPDATE SET answer = excluded.answer, note = excluded.note,
           notify = excluded.notify, reply = excluded.reply",
        params![account, uid, recurrence_id.map(moment_text).unwrap_or_default(), given.answer, given.note, given.notify, reply],
    )?;
    // A whole series answered again replaces what was said for single
    // occurrences, and takes back their replies still waiting.
    if recurrence_id.is_none() {
        let mut stmt = conn.prepare(
            "SELECT o.id FROM invitation_answer a JOIN outbox o ON o.id = a.reply
             WHERE a.account_id = ?1 AND a.uid = ?2 AND a.recurrence_id != '' AND o.state = 'queued'",
        )?;
        let waiting = stmt.query_map(params![account, uid], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for id in waiting {
            super::cancel_send(conn, id)?;
        }
        conn.execute("DELETE FROM invitation_answer WHERE account_id = ?1 AND uid = ?2 AND recurrence_id != ''", params![account, uid])?;
    }
    Ok(())
}

/// The answer kept for exactly this event or occurrence, and its reply mail.
pub fn given_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<(GivenAnswer, Option<ReplyMail>)>> {
    Ok(conn
        .query_row(
            "SELECT a.answer, a.note, a.notify, o.id, o.state FROM invitation_answer a LEFT JOIN outbox o ON o.id = a.reply
             WHERE a.account_id = ?1 AND a.uid = ?2 AND a.recurrence_id = ?3",
            params![account, uid, recurrence_id.map(moment_text).unwrap_or_default()],
            |r| {
                let reply = match (r.get::<_, Option<i64>>(3)?, r.get::<_, Option<String>>(4)?.as_deref()) {
                    (Some(id), Some("queued")) => Some(ReplyMail::Waiting(id)),
                    (Some(id), Some("sent")) => Some(ReplyMail::Sent(id)),
                    _ => None,
                };
                Ok((GivenAnswer { answer: r.get(0)?, note: r.get(1)?, notify: r.get(2)? }, reply))
            },
        )
        .optional()?)
}

/// The answer kept for an occurrence, else for its whole event.
pub fn remembered_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<Response>> {
    let rid = recurrence_id.map(moment_text).unwrap_or_default();
    Ok(conn
        .query_row(
            "SELECT answer FROM invitation_answer WHERE account_id = ?1 AND uid = ?2 AND recurrence_id IN (?3, '')
             ORDER BY recurrence_id = '' LIMIT 1",
            params![account, uid, rid],
            |r| r.get(0),
        )
        .optional()?)
}

/// Where the answer an invitation shows comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerFrom<'a> {
    /// A provider's or a CalDAV copy, which Jolta answers through.
    Calendar(&'a StoredEvent),
    /// The answer given from Jolta.
    Jolta,
    /// A feed's or this Mac's copy, which only says what it was imported
    /// with.
    Import,
}

/// The user's answer to an invitation on the account it came to, as its
/// card and the list show it: `copy` is that account's copy of the event
/// and `remembered` the answer given from Jolta. Jolta answers through a
/// provider's or a CalDAV copy, so that one holds the answer it gave.
pub fn shown_answer(copy: Option<&StoredEvent>, remembered: Option<Response>) -> Option<(Response, AnswerFrom<'_>)> {
    match copy {
        Some(s) if !matches!(s.source, CalendarSource::Webcal | CalendarSource::Local) => s.event.my_response().map(|r| (r, AnswerFrom::Calendar(s))),
        copy => remembered.map(|r| (r, AnswerFrom::Jolta)).or_else(|| copy.and_then(|s| s.event.my_response()).map(|r| (r, AnswerFrom::Import))),
    }
}

/// What the message list shows for a conversation with an invitation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvitationState {
    /// A request for an event still to come that the user hasn't answered.
    Unanswered,
    Answered(Response),
    /// A cancellation, or a request whose event was cancelled since.
    Cancelled,
    /// Only the mark: a reply, the user's own meeting, a request for an
    /// event that passed unanswered, and the other methods.
    Plain,
}

/// One `invitation` row with what its state depends on.
struct Named {
    message: MessageId,
    thread: ThreadId,
    account: AccountId,
    receiver: String,
    uid: String,
    recurrence_id: String,
    sequence: i64,
    method: Method,
    organizer: String,
    ends: i64,
    timing: String,
}

const NAMED: &str = "SELECT i.message_id, m.thread_id, m.account_id, a.address, i.uid, i.recurrence_id, i.sequence, i.method, i.organizer, i.ends, i.timing
     FROM invitation i JOIN message m ON m.id = i.message_id JOIN account a ON a.id = m.account_id";

fn named(r: &Row) -> rusqlite::Result<Named> {
    Ok(Named {
        message: r.get(0)?,
        thread: r.get(1)?,
        account: r.get(2)?,
        receiver: r.get(3)?,
        uid: r.get(4)?,
        recurrence_id: r.get(5)?,
        sequence: r.get(6)?,
        method: r.get(7)?,
        organizer: r.get(8)?,
        ends: r.get(9)?,
        timing: r.get(10)?,
    })
}

/// The invitations of a page of conversations, a JSON array of thread ids,
/// in the order that puts the one each conversation shows first: its newest
/// message from the organizer, such as a request, an update or a
/// cancellation, else its newest reply, and a message's whole event or
/// series before its changed occurrences. One statement for the page: a
/// statement per row cost 18 µs each, mostly with nothing to find.
const PAGE: &str = "WHERE m.thread_id IN (SELECT value FROM json_each(?1))
     ORDER BY m.thread_id, i.method IN ('REPLY', 'COUNTER', 'REFRESH'), m.id DESC, i.recurrence_id != ''";

/// The state of the invitation each of these conversations shows, for those
/// that have one.
pub fn thread_invitations(conn: &Connection, threads: &[ThreadId], now: DateTime<Utc>, here: Tz) -> Result<HashMap<ThreadId, InvitationState>> {
    Ok(shown(conn, threads, now, here)?.into_iter().map(|(t, s)| (t, s.state)).collect())
}

/// A conversation's invitation state, and when time alone changes it in
/// unix seconds: the end of the event a "Not answered" request is for, or
/// of the occurrence a series shows, which then moves on to the next.
struct Shown {
    state: InvitationState,
    until: Option<i64>,
}

fn shown(conn: &Connection, threads: &[ThreadId], now: DateTime<Utc>, here: Tz) -> Result<HashMap<ThreadId, Shown>> {
    let ids = serde_json::to_string(&threads.iter().map(|t| t.0).collect::<Vec<_>>()).expect("numbers");
    let rows = conn.prepare_cached(&format!("{NAMED} {PAGE}"))?.query_map([ids], named)?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = HashMap::new();
    for (i, n) in rows.iter().enumerate() {
        if !out.contains_key(&n.thread) {
            // The same message's changed occurrences follow its series.
            let overrides: Vec<&Named> = rows[i + 1..].iter().take_while(|o| o.message == n.message).filter(|o| o.uid == n.uid).collect();
            out.insert(n.thread, state(conn, n, &overrides, now, here)?);
        }
    }
    Ok(out)
}

/// The conversations whose invitation shows "Not answered", for
/// `is:invite`.
pub fn unanswered_threads(conn: &Connection, now: DateTime<Utc>, here: Tz) -> Result<Vec<ThreadId>> {
    let threads = to_come(conn, now)?;
    let shown = shown(conn, &threads, now, here)?;
    Ok(threads.into_iter().filter(|t| shown.get(t).is_some_and(|s| s.state == InvitationState::Unanswered)).collect())
}

/// When the next conversation's "Not answered" comes or goes with time
/// alone, so a list and what `is:invite` finds hold until then.
pub fn next_invitation_change(conn: &Connection, now: DateTime<Utc>, here: Tz) -> Result<Option<DateTime<Utc>>> {
    let threads = to_come(conn, now)?;
    Ok(shown(conn, &threads, now, here)?.values().filter_map(|s| s.until).min().and_then(|t| DateTime::from_timestamp(t, 0)))
}

/// The conversations with a request for an event to come: the only ones
/// that can show "Not answered", or change by time alone.
fn to_come(conn: &Connection, now: DateTime<Utc>) -> Result<Vec<ThreadId>> {
    Ok(conn
        .prepare_cached(
            "SELECT DISTINCT m.thread_id FROM invitation i JOIN message m ON m.id = i.message_id WHERE i.method = 'REQUEST' AND i.ends > ?1",
        )?
        .query_map([now.timestamp()], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<ThreadId>>>()?)
}

/// A request is answered in the calendars of the account it came to, as
/// its card answers it.
fn state(conn: &Connection, n: &Named, overrides: &[&Named], now: DateTime<Utc>, here: Tz) -> Result<Shown> {
    let fixed = |state| Ok(Shown { state, until: None });
    match n.method {
        Method::Cancel => return fixed(InvitationState::Cancelled),
        Method::Request if !n.organizer.trim().is_empty() && !n.organizer.trim().eq_ignore_ascii_case(&n.receiver) => {}
        _ => return fixed(InvitationState::Plain),
    }
    let picked = pick(n, overrides, now, here)?;
    let copy = match stored_event(conn, &n.uid, picked.event.as_ref(), Some(n.account), here)? {
        None if picked.event.is_some() && n.recurrence_id.is_empty() => stored_event(conn, &n.uid, None, Some(n.account), here)?,
        found => found,
    };
    let cancelled: bool = conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM invitation WHERE uid = ?1 AND method = 'CANCEL' AND recurrence_id IN (?2, '') AND sequence >= ?3)",
        )?
        .query_row(params![n.uid, n.recurrence_id, n.sequence], |r| r.get(0))?;
    if cancelled || copy.as_ref().is_some_and(|c| c.event.status == EventStatus::Cancelled) {
        return fixed(InvitationState::Cancelled);
    }
    let remembered = remembered_answer(conn, n.account, &n.uid, picked.occurrence.as_ref())?;
    // A calendar's copy holds only for the version and the occurrence the
    // message sends, as on the card. An answer given from Jolta is judged
    // there by its reply mail, which the list doesn't read.
    let answer = shown_answer(copy.as_ref(), remembered).map(|(r, from)| match from {
        AnswerFrom::Calendar(s) if picked.sent.as_ref().is_some_and(|(events, said)| !holds(&s.event, Some(s.source), events, &events[*said], here)) => Response::NeedsAction,
        _ => r,
    });
    let state = match answer {
        Some(r) if r != Response::NeedsAction => InvitationState::Answered(r),
        _ if n.ends > now.timestamp() => InvitationState::Unanswered,
        _ => InvitationState::Plain,
    };
    let unanswered = (state == InvitationState::Unanswered).then_some(n.ends);
    let until = picked.ends.into_iter().chain(unanswered).filter(|&t| t > now.timestamp()).min();
    Ok(Shown { state, until })
}

/// Where a request's answer is read from, as its card picks it.
struct Picked {
    /// The recurrence id of the event whose calendar copy holds the answer.
    event: Option<Moment>,
    /// The occurrence an answer given from Jolta names.
    occurrence: Option<Moment>,
    /// For a series, when the occurrence it shows ends, in unix seconds.
    ends: Option<i64>,
    /// The events the message sends, as `timing` places them, and which of
    /// them places the occurrence shown. None for a single event and for a
    /// message that gives no time.
    sent: Option<(Vec<EventData>, usize)>,
}

/// For a series, the occurrence `series::shown_occurrence` shows, either
/// one of the message's changed occurrences or one the rule makes.
fn pick(n: &Named, overrides: &[&Named], now: DateTime<Utc>, here: Tz) -> Result<Picked> {
    let rid = moment_from_text(&n.recurrence_id);
    let parse = |n: &Named| serde_json::from_str::<EventData>(&n.timing).map_err(|e| Error::Other(format!("invitation timing of {}: {e}", n.uid)));
    if n.timing.is_empty() {
        return Ok(Picked { event: rid, occurrence: rid, ends: None, sent: None });
    }
    let this = parse(n)?;
    if rid.is_some() {
        return Ok(Picked { event: rid, occurrence: rid, ends: None, sent: Some((vec![this], 0)) });
    }
    let overrides = overrides.iter().filter(|o| !o.timing.is_empty()).map(|o| parse(o)).collect::<Result<Vec<_>>>()?;
    let shown = series::shown_occurrence(&this, &overrides, now, here);
    let ends = shown.map(|o| end_of(&o.span, here).timestamp());
    let (event, occurrence, said) = match shown.map(|o| (o.source, o.span.start())) {
        None => (None, Some(this.span.start()), 0),
        Some((Source::Override(i), _)) => (overrides[i].recurrence_id, overrides[i].recurrence_id, 1 + i),
        // The calendar may hold this occurrence as an override the mail
        // doesn't have; the series' copy answers when it doesn't.
        Some((Source::Master, start)) => (Some(start), Some(start), 0),
    };
    Ok(Picked { event, occurrence, ends, sent: Some(([this].into_iter().chain(overrides).collect(), said)) })
}

/// The reply mail Jolta sent with the answer `remembered_answer` finds:
/// its blob, which names the version answered. None for an answer given
/// without a reply, or a reply that failed for good.
pub fn remembered_reply(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<String>> {
    let rid = recurrence_id.map(moment_text).unwrap_or_default();
    Ok(conn
        .query_row(
            "SELECT o.blob FROM invitation_answer a JOIN outbox o ON o.id = a.reply AND o.state IN ('queued', 'sent')
             WHERE a.account_id = ?1 AND a.uid = ?2 AND a.recurrence_id IN (?3, '')
             ORDER BY a.recurrence_id = '' LIMIT 1",
            params![account, uid, rid],
            |r| r.get(0),
        )
        .optional()?)
}

/// One message on an account that carries a version of an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRow {
    pub message: MessageId,
    pub method: Method,
    pub sequence: i64,
    /// The ORGANIZER's address, empty without one.
    pub organizer: String,
    /// The mail's From address.
    pub from: String,
    /// The stored body.
    pub blob: Option<String>,
}

/// The requests, additions and cancellations for this UID and recurrence
/// id in the account's messages other than `skip`, highest SEQUENCE first.
/// A recurrence id matches the same moment written in another zone.
pub fn invitation_versions(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>, skip: MessageId, here: Tz) -> Result<Vec<VersionRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.message_id, i.method, i.sequence, i.organizer, m.from_addr, m.blob, i.recurrence_id FROM invitation i JOIN message m ON m.id = i.message_id
         WHERE i.uid = ?1 AND (i.recurrence_id = '') = ?2 AND m.account_id = ?3 AND i.message_id != ?4
           AND i.method IN ('REQUEST', 'ADD', 'CANCEL') AND (m.blob IS NULL OR m.blob NOT LIKE 'synth:%')
         ORDER BY i.sequence DESC, i.message_id DESC",
    )?;
    let rows = stmt.query_map(params![uid, recurrence_id.is_none(), account, skip], |r| {
        let row = VersionRow { message: r.get(0)?, method: r.get(1)?, sequence: r.get(2)?, organizer: r.get(3)?, from: r.get(4)?, blob: r.get(5)? };
        Ok((row, r.get::<_, String>(6)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (row, rid) = row?;
        if recurrence_id.is_none_or(|want| moment_from_text(&rid).is_some_and(|m| m.names(want, here))) {
            out.push(row);
        }
    }
    Ok(out)
}

/// The ORGANIZER and From of the account's requests for this UID, the
/// whole event's and its occurrences', in the order they arrived: by the
/// date sync took from the server (IMAP's INTERNALDATE, Graph's
/// receivedDateTime), which a sender can't set. Ids don't keep that order,
/// since sync stores each batch newest first.
pub fn request_senders(conn: &Connection, account: AccountId, uid: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.organizer, m.from_addr FROM invitation i JOIN message m ON m.id = i.message_id
         WHERE i.uid = ?1 AND m.account_id = ?2 AND i.method = 'REQUEST' AND (m.blob IS NULL OR m.blob NOT LIKE 'synth:%')
         ORDER BY m.date, m.id",
    )?;
    let rows = stmt.query_map(params![uid, account], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Mail from the address that the user answered, through the partial
/// index `message_answered`, which needs this exact `flags & 4` term.
const REPLIED: &str = "SELECT EXISTS (SELECT 1 FROM message WHERE from_addr = ?1 COLLATE NOCASE AND (flags & 4) != 0)";
const _: () = assert!(Flags::ANSWERED == 4);
/// The recipients of the user's own mail that the search index finds the
/// address in, newest first: a contact the user writes to is found in the
/// first rows, and only a stranger's own messages are read to the end.
const SENT: &str = "SELECT m.to_addrs || ', ' || m.cc_addrs FROM message_fts f
     JOIN message m ON m.id = f.rowid JOIN account a ON a.id = m.account_id
     WHERE message_fts MATCH ?1 AND m.from_addr = a.address COLLATE NOCASE ORDER BY f.rowid DESC";

/// Whether the user ever sent mail to `address` from one of their
/// accounts, or replied to mail from it. A search phrase also matches
/// longer addresses ("jan@x.com" in "jan@x.com.au"), so each hit is
/// checked against the parsed recipients.
pub fn has_written_to(conn: &Connection, address: &str) -> Result<bool> {
    let address = address.trim().to_lowercase();
    if !address.contains(char::is_alphanumeric) {
        return Ok(false);
    }
    let replied: bool = conn.query_row(REPLIED, [&address], |r| r.get(0))?;
    if replied {
        return Ok(true);
    }
    let mut stmt = conn.prepare_cached(SENT)?;
    let mut rows = stmt.query([format!("recipients : \"{}\"", address.replace('"', "\"\""))])?;
    while let Some(r) = rows.next()? {
        let list: String = r.get(0)?;
        if compose::parse_addresses(&list).iter().any(|(_, a)| a.trim().eq_ignore_ascii_case(&address)) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::calendar::Response;
    use crate::calendar::invitation::tests::{import, jolta, mail};
    use crate::calendar::series::Horizon;

    fn rows(jolta: &crate::Jolta) -> Vec<(String, String, i64, String, String)> {
        jolta
            .store
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT uid, recurrence_id, sequence, method, organizer FROM invitation ORDER BY uid, recurrence_id",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap()
    }

    #[test]
    fn a_stored_body_records_its_invitations() {
        let (jolta, _) = jolta("stored");
        import(&jolta, mail!("cancel.eml"));
        import(&jolta, mail!("series.eml"));
        import(&jolta, b"Subject: plain\r\n\r\nNo calendar.\r\n");
        let uid = "7kq2m4p9r1s3t5v7x9z0b2d4f6@google.com";
        assert_eq!(
            rows(&jolta),
            [
                (uid.into(), "".into(), 1, "CANCEL".into(), "ann@example.com".into()),
                ("standup-series@example.com".into(), "".into(), 0, "REQUEST".into(), "ann@example.com".into()),
                ("standup-series@example.com".into(), "20261105T093000 Europe/Brussels".into(), 0, "REQUEST".into(), "ann@example.com".into()),
            ]
        );
        jolta.store.write(|tx| Ok(tx.execute("DELETE FROM message", [])?)).unwrap();
        assert!(rows(&jolta).is_empty(), "rows go with their message");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn the_scan_fills_in_bodies_stored_before_and_runs_once() {
        let (jolta, _) = jolta("scan");
        let request = import(&jolta, mail!("google-request.eml"));
        let reply = import(&jolta, mail!("reply.eml"));
        let gone = import(&jolta, mail!("outlook-request.eml"));
        jolta
            .store
            .write(|tx| {
                tx.execute("DELETE FROM invitation", [])?;
                tx.execute("INSERT INTO invitation_scan SELECT id FROM message", [])?;
                tx.execute("DELETE FROM message WHERE id = ?1", [gone])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(crate::engine::scan_invitations(&jolta).unwrap(), 2);
        let found: Vec<(String, String)> = rows(&jolta).into_iter().map(|r| (r.0, r.3)).collect();
        assert_eq!(
            found,
            [("7kq2m4p9r1s3t5v7x9z0b2d4f6@google.com".into(), "REQUEST".into()), ("design-crit-1113@example.com".into(), "REPLY".into())]
        );
        assert_eq!(crate::engine::scan_invitations(&jolta).unwrap(), 0, "the queue is empty");
        assert_eq!(rows(&jolta).len(), 2);
        let _ = (request, reply);
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn an_invitation_finds_its_event_in_the_calendars() {
        let (jolta, calendar) = jolta("match");
        let found = crate::calendar::invitation::find(mail!("outlook-request.eml"), &["me@example.com".into()]);
        let mut event: EventData = found[0].event.clone();
        event.attendees[0].response = Response::Accepted;
        event.sequence = 2;
        let horizon = Horizon::around(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(), chrono_tz::Europe::Brussels);
        jolta
            .store
            .write(|tx| crate::store::apply_item(tx, calendar, "remote-1", None, &[event.clone()], None, &horizon))
            .unwrap();
        let stored = jolta.store.read(|c| stored_event(c, &event.uid, None, None, chrono_tz::Europe::Brussels)).unwrap().unwrap();
        assert_eq!((stored.calendar_id, stored.event.my_response(), stored.event.sequence), (calendar, Some(Response::Accepted), 2));
        assert_eq!(jolta.store.read(|c| stored_event(c, "someone-else", None, None, chrono_tz::Europe::Brussels)).unwrap(), None);
        jolta.store.write(|tx| crate::store::set_calendar_shown(tx, calendar, false)).unwrap();
        assert_eq!(jolta.store.read(|c| stored_event(c, &event.uid, None, None, chrono_tz::Europe::Brussels)).unwrap(), None, "hidden calendars don't count");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn an_occurrence_matches_its_moment_in_any_zone() {
        let (jolta, calendar) = jolta("match-moment");
        let mut series = crate::calendar::invitation::find(mail!("series.eml"), &["me@example.com".into()]);
        let mut moved = series.remove(1).event;
        moved.sequence = 3;
        let horizon = Horizon::around(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(), chrono_tz::Europe::Brussels);
        jolta.store.write(|tx| crate::store::apply_item(tx, calendar, "remote-1", None, &[moved.clone()], None, &horizon)).unwrap();
        let brussels = moved.recurrence_id.unwrap();
        let Moment::Time(local) = brussels else { panic!("a time") };
        let utc = Moment::Time(crate::calendar::Local {
            wall: local.instant(chrono_tz::UTC).naive_utc(),
            zone: crate::calendar::Zone::Tz(chrono_tz::UTC),
        });
        let found = |rid: &Moment| jolta.store.read(|c| stored_event(c, &moved.uid, Some(rid), None, chrono_tz::Europe::Brussels)).unwrap();
        assert_eq!(found(&brussels).map(|s| s.event.sequence), Some(3));
        assert_eq!(found(&utc).map(|s| s.event.sequence), Some(3), "09:30 Brussels is 08:30 UTC in November");
        let later = Moment::Time(crate::calendar::Local { wall: local.wall + chrono::Duration::hours(1), ..local });
        assert_eq!(found(&later), None);
        assert_eq!(jolta.store.read(|c| stored_event(c, &moved.uid, None, None, chrono_tz::Europe::Brussels)).unwrap(), None, "no master stored");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    fn shown(jolta: &crate::Jolta, message: MessageId, now: DateTime<Utc>) -> Option<InvitationState> {
        jolta
            .store
            .read(|c| {
                let thread = c.query_row("SELECT thread_id FROM message WHERE id = ?1", [message], |r| r.get(0))?;
                Ok(thread_invitations(c, &[thread], now, chrono_tz::Europe::Brussels)?.get(&thread).copied())
            })
            .unwrap()
    }

    /// The messages of the conversations `is:invite` finds.
    fn open(jolta: &crate::Jolta, now: DateTime<Utc>) -> Vec<MessageId> {
        jolta
            .store
            .read(|c| {
                let threads = unanswered_threads(c, now, chrono_tz::Europe::Brussels)?;
                let ids = serde_json::to_string(&threads.iter().map(|t| t.0).collect::<Vec<_>>()).expect("numbers");
                let mut stmt = c.prepare("SELECT id FROM message WHERE thread_id IN (SELECT value FROM json_each(?1)) ORDER BY id")?;
                Ok(stmt.query_map([ids], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap()
    }

    fn copy_answered(jolta: &crate::Jolta, calendar: CalendarId, response: Response) {
        let mut copy = crate::calendar::invitation::find(mail!("google-request.eml"), &["me@example.com".into()]).remove(0).event;
        copy.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = response);
        let horizon = Horizon::around(now(), chrono_tz::Europe::Brussels);
        jolta.store.write(|tx| crate::store::apply_item(tx, calendar, "g1", None, &[copy], None, &horizon)).unwrap();
    }

    #[test]
    fn a_request_shows_the_answer_its_card_shows() {
        let (jolta, calendar) = jolta("state-answer");
        let request = import(&jolta, mail!("google-request.eml"));
        assert_eq!(shown(&jolta, request, now()), Some(InvitationState::Unanswered));
        assert_eq!(open(&jolta, now()), [request]);

        let given = GivenAnswer { answer: Response::Tentative, note: String::new(), notify: true };
        jolta.store.write(|tx| remember_answer(tx, AccountId(1), "7kq2m4p9r1s3t5v7x9z0b2d4f6@google.com", None, &given, None)).unwrap();
        assert_eq!(shown(&jolta, request, now()), Some(InvitationState::Answered(Response::Tentative)), "the answer given from Jolta");
        assert!(open(&jolta, now()).is_empty());

        copy_answered(&jolta, calendar, Response::Declined);
        assert_eq!(shown(&jolta, request, now()), Some(InvitationState::Answered(Response::Declined)), "the CalDAV copy Jolta answers through wins");
        copy_answered(&jolta, calendar, Response::NeedsAction);
        assert_eq!(shown(&jolta, request, now()), Some(InvitationState::Unanswered), "answered elsewhere and taken back");

        let later = Utc.with_ymd_and_hms(2026, 11, 12, 16, 0, 0).unwrap();
        assert_eq!(shown(&jolta, request, later), Some(InvitationState::Plain), "16:45 Brussels has passed, unanswered");
        assert!(open(&jolta, later).is_empty());
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn a_cancellation_cancels_the_request_it_follows() {
        let (jolta, _) = jolta("state-cancel");
        let request = import(&jolta, mail!("google-request.eml"));
        let cancel = import(&jolta, mail!("cancel.eml"));
        assert_eq!(shown(&jolta, cancel, now()), Some(InvitationState::Cancelled));
        assert_eq!(shown(&jolta, request, now()), Some(InvitationState::Cancelled), "a later SEQUENCE cancels it");
        assert!(open(&jolta, now()).is_empty());
        jolta.store.write(|tx| Ok(tx.execute("UPDATE invitation SET sequence = 2 WHERE message_id = ?1", [request])?)).unwrap();
        assert_eq!(shown(&jolta, request, now()), Some(InvitationState::Unanswered), "sent again after the cancellation");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn replies_and_the_users_own_meetings_get_the_mark_alone() {
        let (jolta, _) = jolta("state-plain");
        let reply = import(&jolta, mail!("reply.eml"));
        assert_eq!(shown(&jolta, reply, now()), Some(InvitationState::Plain));
        let mine = import(&jolta, &String::from_utf8_lossy(mail!("series.eml")).replace("mailto:ann@example.com", "mailto:ME@example.com").into_bytes());
        assert_eq!(shown(&jolta, mine, now()), Some(InvitationState::Plain), "organized by the account it came to");
        let plain = import(&jolta, b"Subject: plain\r\n\r\nNo calendar.\r\n");
        assert_eq!(shown(&jolta, plain, now()), None);
        assert!(open(&jolta, now()).is_empty());
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn a_series_is_to_come_until_its_last_occurrence() {
        let (jolta, _) = jolta("state-series");
        let series = import(&jolta, mail!("series.eml"));
        let ends: i64 = jolta.store.read(|c| Ok(c.query_row("SELECT ends FROM invitation WHERE recurrence_id = ''", [], |r| r.get(0))?)).unwrap();
        assert_eq!(DateTime::from_timestamp(ends, 0), Utc.with_ymd_and_hms(2026, 12, 17, 8, 45, 0).single(), "Thursday 17 December, 09:45 Brussels");
        assert_eq!(open(&jolta, now()), [series]);
        assert_eq!(open(&jolta, Utc.with_ymd_and_hms(2026, 12, 18, 0, 0, 0).unwrap()), []);

        let endless = String::from_utf8_lossy(mail!("series.eml")).replace(";UNTIL=20261217T225959Z", "").replace("standup-series@", "endless@");
        let endless = crate::calendar::invitation::find(endless.as_bytes(), &[]).remove(0);
        assert_eq!(endless.ends(chrono_tz::Europe::Brussels), Some(DateTime::<Utc>::MAX_UTC));
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    /// The answer the card of this message shows at `now`, as the list would.
    fn card_says(jolta: &crate::Jolta, message: MessageId, raw: &[u8], now: DateTime<Utc>) -> InvitationState {
        use crate::calendar::card;
        let me = ["me@example.com".to_string()];
        let found = crate::calendar::invitation::find(raw, &me);
        let cx = card::Context { message, account: AccountId(1), me: &me, sender: "ann@example.com", now, here: chrono_tz::Europe::Brussels, blobs: &jolta.blobs };
        match jolta.store.read(|c| card::cards(c, &found, &cx)).unwrap().remove(0) {
            card::Card::Event(e) => match e.answer {
                Some(r) if r != Response::NeedsAction => InvitationState::Answered(r),
                _ => InvitationState::Unanswered,
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_series_shows_the_answer_to_the_occurrence_its_card_shows() {
        let (jolta, _) = jolta("state-occurrence");
        let series = import(&jolta, mail!("series.eml"));
        let at = |s: &str| {
            let wall = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
            Moment::Time(crate::calendar::Local { wall, zone: crate::calendar::Zone::Tz(chrono_tz::Europe::Brussels) })
        };
        let uid = "standup-series@example.com";
        for (occurrence, answer) in [("2026-11-05 09:30", Response::Accepted), ("2026-11-10 09:30", Response::Declined)] {
            let given = GivenAnswer { answer, note: String::new(), notify: true };
            jolta.store.write(|tx| remember_answer(tx, AccountId(1), uid, Some(&at(occurrence)), &given, None)).unwrap();
        }
        for (now, expected, why) in [
            (now(), InvitationState::Answered(Response::Accepted), "Thursday's, moved to 10:00"),
            (Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap(), InvitationState::Answered(Response::Declined), "next Tuesday's"),
            (Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap(), InvitationState::Unanswered, "Thursday 12 November's, not answered"),
        ] {
            assert_eq!(card_says(&jolta, series, mail!("series.eml"), now), expected, "the card: {why}");
            assert_eq!(shown(&jolta, series, now), Some(expected), "the list: {why}");
        }
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    /// The calendar answered one occurrence of a series the mail sent whole:
    /// its copy is an override the mail doesn't have.
    #[test]
    fn a_series_shows_the_calendars_answer_for_the_occurrence_its_card_shows() {
        let (jolta, calendar) = jolta("state-occurrence-copy");
        let series = import(&jolta, mail!("series.eml"));
        let me = ["me@example.com".to_string()];
        let mut events: Vec<EventData> = crate::calendar::invitation::find(mail!("series.eml"), &me).into_iter().map(|i| i.event).collect();
        let master = events.iter().find(|e| e.recurrence_id.is_none()).unwrap().clone();
        let wall = chrono::NaiveDateTime::parse_from_str("2026-11-10 09:30", "%Y-%m-%d %H:%M").unwrap();
        let rid = Moment::Time(crate::calendar::Local { wall, zone: crate::calendar::Zone::Tz(chrono_tz::Europe::Brussels) });
        let mut tuesday = EventData {
            recurrence_id: Some(rid),
            recurrence: None,
            span: crate::calendar::series::occurrence_span(&master.span, &rid, chrono_tz::Europe::Brussels).unwrap(),
            ..master
        };
        tuesday.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = Response::Accepted);
        assert!(tuesday.my_response() == Some(Response::Accepted), "the user is on the guest list");
        events.push(tuesday);
        let horizon = crate::calendar::series::Horizon::around(now(), chrono_tz::Europe::Brussels);
        jolta.store.write(|tx| crate::store::apply_item(tx, calendar, "s1", Some("1"), &events, None, &horizon).map(|_| ())).unwrap();

        let after_thursday = Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap();
        let accepted = InvitationState::Answered(Response::Accepted);
        assert_eq!(card_says(&jolta, series, mail!("series.eml"), after_thursday), accepted, "the card: next Tuesday's copy");
        assert_eq!(shown(&jolta, series, after_thursday), Some(accepted), "the list: next Tuesday's copy");

        // The series answered Declined doesn't reach an occurrence the
        // calendar holds its own answer for.
        for e in events.iter_mut().filter(|e| e.recurrence_id.is_none()) {
            e.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = Response::Declined);
        }
        jolta.store.write(|tx| crate::store::apply_item(tx, calendar, "s1", Some("2"), &events, None, &horizon).map(|_| ())).unwrap();
        assert_eq!(card_says(&jolta, series, mail!("series.eml"), after_thursday), accepted, "the card: Tuesday's own copy");
        assert_eq!(shown(&jolta, series, after_thursday), Some(accepted), "the list: Tuesday's own copy");
        let after_tuesday = Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap();
        let declined = InvitationState::Answered(Response::Declined);
        assert_eq!(card_says(&jolta, series, mail!("series.eml"), after_tuesday), declined, "the card: the series' answer for Thursday 12 November");
        assert_eq!(shown(&jolta, series, after_tuesday), Some(declined), "the list: the series' answer for Thursday 12 November");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    /// Every mix of a calendar copy, of the event or of the occurrence shown
    /// or both, on CalDAV or Graph, and an answer given from Jolta to the
    /// whole event or to that occurrence, for a single event, an unchanged
    /// and a moved occurrence of a series and an occurrence invited to
    /// alone: the card and the list say the same.
    #[test]
    fn the_card_and_the_list_show_the_same_answer() {
        let brussels = chrono_tz::Europe::Brussels;
        let at = |s: &str| {
            let wall = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
            Moment::Time(crate::calendar::Local { wall, zone: crate::calendar::Zone::Tz(brussels) })
        };
        let series = mail!("series.eml").to_vec();
        let text = String::from_utf8_lossy(&series);
        let first = text.find("BEGIN:VEVENT").unwrap();
        let master_part = &text[first..first + 1 + text[first + 1..].find("BEGIN:VEVENT").unwrap()];
        let alone = text.replace(master_part, "").into_bytes();
        let after_thursday = Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap();
        let me = ["me@example.com".to_string()];
        // (what, mail, when, the occurrence the card shows)
        let shapes: [(&str, Vec<u8>, DateTime<Utc>, Option<Moment>); 4] = [
            ("a single event", mail!("google-request.eml").to_vec(), now(), None),
            ("an unchanged occurrence", series.clone(), after_thursday, Some(at("2026-11-10 09:30"))),
            ("a moved occurrence", series.clone(), now(), Some(at("2026-11-05 09:30"))),
            ("an occurrence invited to alone", alone, now(), Some(at("2026-11-05 09:30"))),
        ];
        let series_master = crate::calendar::invitation::find(&series, &me).into_iter().map(|i| i.event).find(|e| e.recurrence_id.is_none()).unwrap();
        let answer = |e: &mut EventData, r: Response| e.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = r);
        let mut seen = std::collections::BTreeSet::new();
        for (what, raw, now, occurrence) in shapes {
            let (jolta, caldav) = jolta(&format!("same-answer-{}", what.replace(' ', "-")));
            let graph = jolta
                .store
                .write(|tx| {
                    let remote = crate::store::RemoteCalendar { remote_id: "graph".into(), name: "Calendar".into(), color: None, zone: None, access: crate::calendar::Access::Owner, primary: true, refresh: None };
                    Ok(crate::store::sync_calendar_list(tx, AccountId(1), CalendarSource::Graph, &[remote])?[0].id)
                })
                .unwrap();
            let message = import(&jolta, &raw);
            let sent: Vec<EventData> = crate::calendar::invitation::find(&raw, &me).into_iter().map(|i| i.event).collect();
            let uid = sent[0].uid.clone();
            let base = sent.iter().find(|e| e.recurrence_id.is_none()).cloned().unwrap_or_else(|| series_master.clone());
            let mut whole = base.clone();
            answer(&mut whole, Response::Declined);
            let mut own = occurrence.map(|rid| {
                let span = sent
                    .iter()
                    .find(|e| e.recurrence_id.is_some_and(|r| r.names(&rid, brussels)))
                    .map(|e| e.span)
                    .unwrap_or_else(|| crate::calendar::series::occurrence_span(&base.span, &rid, brussels).unwrap());
                EventData { recurrence_id: Some(rid), recurrence: None, span, ..base.clone() }
            });
            if let Some(own) = &mut own {
                answer(own, Response::Accepted);
            }
            let copies: Vec<(&str, Vec<EventData>)> = [
                ("no copy", vec![]),
                ("the event's copy", vec![whole.clone()]),
                ("the occurrence's copy", own.iter().cloned().collect()),
                ("both copies", [whole.clone()].into_iter().chain(own.clone()).collect()),
            ]
            .into_iter()
            .filter(|(_, c)| occurrence.is_some() || c.len() < 2)
            .collect();
            let given = |r| GivenAnswer { answer: r, note: String::new(), notify: false };
            for (calendar, source) in [(caldav, "CalDAV"), (graph, "Graph")] {
                for (copy, events) in &copies {
                    let answers = [("no answer from Jolta", None), ("the event answered from Jolta", Some(None)), ("the occurrence answered from Jolta", occurrence.map(Some))];
                    for (remembered, rid) in answers.into_iter().filter(|(w, rid)| rid.is_some() || w.starts_with("no")) {
                        let cell = format!("{what}, {copy} on {source}, {remembered}");
                        jolta
                            .store
                            .write(|tx| {
                                tx.execute_batch("DELETE FROM event; DELETE FROM invitation_answer;")?;
                                if !events.is_empty() {
                                    let horizon = Horizon::around(now, brussels);
                                    crate::store::apply_item(tx, calendar, "c1", Some("1"), events, None, &horizon)?;
                                }
                                match rid {
                                    Some(None) => remember_answer(tx, AccountId(1), &uid, None, &given(Response::Tentative), None),
                                    Some(Some(rid)) => remember_answer(tx, AccountId(1), &uid, Some(&rid), &given(Response::Accepted), None),
                                    None => Ok(()),
                                }
                            })
                            .unwrap();
                        let card = card_says(&jolta, message, &raw, now);
                        assert_eq!(Some(card), shown(&jolta, message, now), "{cell}");
                        seen.insert(format!("{card:?}"));
                    }
                }
            }
            std::fs::remove_dir_all(&jolta.data_dir).unwrap();
        }
        assert_eq!(seen.len(), 4, "Not answered, Accepted, Maybe and Declined all turn up: {seen:?}");
    }

    #[test]
    fn states_hold_until_an_unanswered_event_ends_or_a_series_moves_on() {
        let (jolta, _) = jolta("state-change");
        let next = |now| jolta.store.read(|c| next_invitation_change(c, now, chrono_tz::Europe::Brussels)).unwrap();
        assert_eq!(next(now()), None);
        import(&jolta, mail!("google-request.eml"));
        assert_eq!(next(now()), Utc.with_ymd_and_hms(2026, 11, 12, 15, 45, 0).single(), "16:45 Brussels, when Not answered goes");
        import(&jolta, mail!("series.eml"));
        assert_eq!(next(now()), Utc.with_ymd_and_hms(2026, 11, 5, 9, 15, 0).single(), "Thursday's standup ends at 10:15 Brussels");
        assert_eq!(next(Utc.with_ymd_and_hms(2026, 12, 18, 0, 0, 0).unwrap()), None, "everything has passed");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    /// A request from Ann for `uid`, from `start` to `end` in UTC, as
    /// message `id` answering `parent`.
    fn request(id: &str, parent: &str, uid: &str, start: &str, end: &str, sequence: i64) -> Vec<u8> {
        format!(
            "From: ann@example.com\r\nTo: me@example.com\r\nSubject: Sync\r\nMessage-ID: <{id}>\r\nIn-Reply-To: <{parent}>\r\n\
             Content-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             SUMMARY:Sync\r\nDTSTART:{start}\r\nDTEND:{end}\r\nSEQUENCE:{sequence}\r\nORGANIZER:mailto:ann@example.com\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
        .into_bytes()
    }

    fn count(jolta: &crate::Jolta, query: &str) -> u32 {
        jolta.store.read(|c| crate::search::query_count(c, &crate::search::parse(query)?)).unwrap()
    }

    #[test]
    fn is_invite_and_its_negation_split_conversations() {
        let (jolta, _) = jolta("is-invite-thread");
        import(&jolta, &request("q1@example.com", "", "sync-1", "20991112T150000Z", "20991112T160000Z", 0));
        import(&jolta, b"From: me@example.com\r\nTo: ann@example.com\r\nSubject: Re: Sync\r\nMessage-ID: <r1@example.com>\r\nIn-Reply-To: <q1@example.com>\r\n\r\nSure.\r\n");
        assert_eq!(count(&jolta, "is:invite"), 1);
        assert_eq!(count(&jolta, "-is:invite"), 0, "the reply is in a conversation that is:invite finds");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn is_invite_reads_the_invitation_the_list_shows() {
        let (jolta, _) = jolta("is-invite-shown");
        import(&jolta, &request("q2@example.com", "", "sync-2", "20991112T150000Z", "20991112T160000Z", 0));
        let update = import(&jolta, &request("q3@example.com", "q2@example.com", "sync-2", "20011112T150000Z", "20011112T160000Z", 1));
        assert_eq!(shown(&jolta, update, Utc::now()), Some(InvitationState::Plain), "the update moved it into the past");
        assert_eq!(count(&jolta, "is:invite"), 0);
        assert_eq!(count(&jolta, "-is:invite"), 1);
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn a_page_finds_its_invitations_through_indexes() {
        let (jolta, _) = jolta("state-plan");
        let plans: Vec<String> = jolta
            .store
            .read(|c| {
                let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {NAMED} {PAGE}"))?;
                let rows = stmt.query_map(["[1, 2]"], |r| r.get::<_, String>(3))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        assert!(!plans.iter().any(|d| d.starts_with("SCAN") && !d.starts_with("SCAN json_each")), "only the page's ids are read through: {plans:#?}");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn written_to_reads_no_message_it_doesnt_need() {
        let (jolta, _) = jolta("written-plan");
        let plans: Vec<String> = jolta
            .store
            .read(|c| {
                let mut details = Vec::new();
                for (sql, n) in [(REPLIED, 1), (SENT, 1)] {
                    let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let args = vec![rusqlite::types::Value::Text("x".into()); n];
                    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
                    while let Some(r) = rows.next()? {
                        details.push(r.get::<_, String>(3)?);
                    }
                }
                Ok(details)
            })
            .unwrap();
        assert!(!plans.iter().any(|d| d == "SCAN m" || d == "SCAN message" || d.starts_with("SCAN m ") || d.starts_with("SCAN message ")), "{plans:#?}");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn written_to_means_sent_to_or_replied_to() {
        let (jolta, _) = jolta("written");
        jolta
            .store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, cc_addrs, date, flags)
                     VALUES (1, 1, 'a', 1, 'ME@example.com', 'Jan Peeters <jan@example.com>', 'Cleo <cleo@example.com>', 0, 0),
                            (2, 1, 'b', 2, 'bart@example.com', 'me@example.com', '', 0, 4),
                            (3, 1, 'c', 3, 'news@example.com', 'me@example.com', '', 0, 0);
                     INSERT INTO message_fts (rowid, subject, sender, recipients, body)
                     VALUES (1, '', ' ME@example.com', 'Jan Peeters <jan@example.com> Cleo <cleo@example.com>', ''),
                            (2, '', ' bart@example.com', 'me@example.com ', ''),
                            (3, '', ' news@example.com', 'me@example.com ', '');",
                )?;
                Ok(())
            })
            .unwrap();
        let written = |a: &str| jolta.store.read(|c| has_written_to(c, a)).unwrap();
        assert!(written("jan@example.com") && written("Cleo@Example.com"), "sent to, in To or Cc");
        assert!(written("bart@example.com"), "replied to");
        assert!(!written("an@example.com") && !written("example.com"), "only whole addresses");
        assert!(!written("news@example.com") && !written(""));
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }
}
