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
use crate::calendar::rsvp::Proposal;
use crate::calendar::series::{self, copy_rank};
use crate::calendar::update::{self, holds};
use crate::calendar::{Calendar, CalendarId, CalendarSource, EventData, EventId, Moment, Response, tz};
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

/// What places the event a message sends, as JSON; '' for a message that
/// gives no time.
fn timing(i: &Invitation) -> String {
    if !i.dated {
        return String::new();
    }
    serde_json::to_string(&placed(&i.event)).expect("an event serializes")
}

/// A version of an event with only what `update::holds` compares: its
/// SEQUENCE, its span, and for a series the repeat rule.
fn placed(e: &EventData) -> EventData {
    EventData {
        recurrence_id: e.recurrence_id,
        this_and_future: e.this_and_future,
        recurrence: e.recurrence.clone(),
        status: e.status,
        sequence: e.sequence,
        ..EventData::new(e.uid.clone(), e.span)
    }
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

/// An account's own copy of a meeting is never one the user added with Add
/// to calendar, in whichever account's calendar: that copy is answered by
/// mail and never decides who organizes the meeting.
const NOT_ADDED: &str = "NOT EXISTS (SELECT 1 FROM added_copy a WHERE a.calendar_id = e.calendar_id AND (a.uid = e.uid OR a.remote_id = e.remote_id))";

/// The event with this UID and recurrence id in a shown calendar, of
/// `account` only when one is given, and then never a copy the user added
/// (`NOT_ADDED`). A recurrence id matches the same moment written in
/// another zone, as `Moment::names` decides. A meeting in several calendars
/// gives the copy the views show.
pub fn stored_event(conn: &Connection, uid: &str, recurrence_id: Option<&Moment>, account: Option<AccountId>, here: Tz) -> Result<Option<StoredEvent>> {
    stored_event_in(conn, uid, recurrence_id, account, here, |_| true)
}

/// `stored_event` among the calendars `keep` accepts.
pub fn stored_event_in(
    conn: &Connection,
    uid: &str,
    recurrence_id: Option<&Moment>,
    account: Option<AccountId>,
    here: Tz,
    keep: impl Fn(&Calendar) -> bool,
) -> Result<Option<StoredEvent>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT e.id, e.calendar_id, c.source, c.is_default, e.data, ifnull(json_extract(e.data, '$.organizer_is_me'), 0)
         FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.uid = ?1 AND (e.recurrence_id = '') = ?2 AND c.shown AND (?3 IS NULL OR (c.account_id = ?3 AND {NOT_ADDED}))"
    ))?;
    let rows = stmt.query_map(params![uid, recurrence_id.is_none(), account], |r| {
        let (event_id, calendar_id, source, is_default): (EventId, CalendarId, CalendarSource, bool) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        Ok((copy_rank(uid, r.get(5)?, source, is_default, calendar_id), event_id, calendar_id, source, r.get::<_, String>(4)?))
    })?;
    let mut best: Option<((bool, bool, bool, i64), StoredEvent)> = None;
    for row in rows {
        let (rank, event_id, calendar_id, source, data) = row?;
        if best.as_ref().is_some_and(|(b, _)| *b <= rank) || !keep(&super::calendar(conn, calendar_id)?) {
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
/// only when one is given, and then never a copy the user added.
pub fn stored_series(conn: &Connection, uid: &str, account: Option<AccountId>) -> Result<Vec<EventData>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT e.id, e.calendar_id, c.source, c.is_default, e.data, ifnull(json_extract(e.data, '$.organizer_is_me'), 0)
         FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.uid = ?1 AND c.shown AND (?2 IS NULL OR (c.account_id = ?2 AND {NOT_ADDED}))"
    ))?;
    let rows = stmt
        .query_map(params![uid, account], |r| {
            let (event_id, calendar_id, source, is_default): (EventId, CalendarId, CalendarSource, bool) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            Ok((copy_rank(uid, r.get(5)?, source, is_default, calendar_id), event_id, calendar_id, r.get::<_, String>(4)?))
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
    /// The new time proposed with the answer.
    pub proposal: Option<Proposal>,
}

/// The reply mail an answer queued, while the outbox still means to send
/// it or has sent it. One that failed for good or was taken back is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyMail {
    Waiting(i64),
    Sent(i64),
}

/// The mail with `message_id` in the account's outbox, while it still
/// means to send it or has sent it.
pub fn outbox_mail(conn: &Connection, account: AccountId, message_id: &str) -> Result<Option<ReplyMail>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, state FROM outbox WHERE account_id = ?1 AND message_id = ?2 AND state IN ('queued', 'sent') ORDER BY id DESC LIMIT 1",
            params![account, message_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(id, state)| if state == "queued" { ReplyMail::Waiting(id) } else { ReplyMail::Sent(id) }))
}

/// The calendar mail Jolta sent in answer to a message, such as the no to
/// a proposal, while the outbox still means to send it or has sent it. A
/// reply the user wrote has a draft; this one has none.
pub fn calendar_answer_to(conn: &Connection, message: MessageId) -> Result<Option<ReplyMail>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, state FROM outbox WHERE replying_to = ?1 AND draft_id IS NULL AND state IN ('queued', 'sent') ORDER BY id DESC LIMIT 1",
            [message],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(id, state)| if state == "queued" { ReplyMail::Waiting(id) } else { ReplyMail::Sent(id) }))
}

/// Keeps the answer the user gave an event's invitation on an account:
/// the whole event, or with `recurrence_id` one occurrence, with the
/// version answered (the event or occurrence, or a series with its changed
/// occurrences) and the outbox row of its reply mail.
pub fn remember_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>, given: &GivenAnswer, version: &[EventData], reply: Option<i64>) -> Result<()> {
    conn.execute(
        "INSERT INTO invitation_answer (account_id, uid, recurrence_id, answer, note, notify, reply, version, proposed_start, proposed_end)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (account_id, uid, recurrence_id) DO UPDATE SET answer = excluded.answer, note = excluded.note,
           notify = excluded.notify, reply = excluded.reply, version = excluded.version,
           proposed_start = excluded.proposed_start, proposed_end = excluded.proposed_end",
        params![
            account,
            uid,
            recurrence_id.map(moment_text).unwrap_or_default(),
            given.answer,
            given.note,
            given.notify,
            reply,
            serde_json::to_string(&version.iter().map(placed).collect::<Vec<_>>()).expect("events serialize"),
            given.proposal.map(|p| p.start.timestamp()),
            given.proposal.map(|p| p.end.timestamp()),
        ],
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
            "SELECT a.answer, a.note, a.notify, o.id, o.state, a.proposed_start, a.proposed_end FROM invitation_answer a LEFT JOIN outbox o ON o.id = a.reply
             WHERE a.account_id = ?1 AND a.uid = ?2 AND a.recurrence_id = ?3",
            params![account, uid, recurrence_id.map(moment_text).unwrap_or_default()],
            |r| {
                let reply = match (r.get::<_, Option<i64>>(3)?, r.get::<_, Option<String>>(4)?.as_deref()) {
                    (Some(id), Some("queued")) => Some(ReplyMail::Waiting(id)),
                    (Some(id), Some("sent")) => Some(ReplyMail::Sent(id)),
                    _ => None,
                };
                let at = |i| r.get::<_, Option<i64>>(i).map(|t| t.and_then(|t| DateTime::from_timestamp(t, 0)));
                let proposal = match (at(5)?, at(6)?) {
                    (Some(start), Some(end)) => Some(Proposal { start, end }),
                    _ => None,
                };
                Ok((GivenAnswer { answer: r.get(0)?, note: r.get(1)?, notify: r.get(2)?, proposal }, reply))
            },
        )
        .optional()?)
}

/// An answer given from Jolta and the version it went to, as
/// `remember_answer` keeps it. Answers kept before Jolta recorded the
/// version have none.
#[derive(Clone, Debug, PartialEq)]
pub struct Remembered {
    pub answer: Response,
    pub version: Option<Vec<EventData>>,
}

/// The answer kept for an occurrence, else for its whole event.
pub fn remembered_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<Remembered>> {
    let rid = recurrence_id.map(moment_text).unwrap_or_default();
    let row: Option<(Response, Option<String>)> = conn
        .query_row(
            "SELECT answer, version FROM invitation_answer WHERE account_id = ?1 AND uid = ?2 AND recurrence_id IN (?3, '')
             ORDER BY recurrence_id = '' LIMIT 1",
            params![account, uid, rid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((answer, version)) = row else { return Ok(None) };
    let version = version.map(|v| serde_json::from_str(&v).map_err(|e| Error::Other(format!("answered version of {uid}: {e}")))).transpose()?;
    Ok(Some(Remembered { answer, version }))
}

/// The user's answer to an invitation on the account it came to, as its
/// card and the list show it: `copy` is that account's copy of the event,
/// `remembered` the answer given from Jolta, and `sent` the events the
/// message sends with the recurrence id of the occurrence shown, None for a
/// message that gives no time. Jolta answers through a provider's or a
/// CalDAV copy, so that one holds the answer it gave. Its answer, or the one
/// given from Jolta, holds only for the version and the occurrence it went
/// to, and reads Not answered after that. A feed's or this Mac's copy only
/// says what it was imported with.
pub fn shown_answer(copy: Option<&StoredEvent>, remembered: Option<&Remembered>, sent: Option<(&[EventData], Option<&Moment>)>, here: Tz) -> Option<Response> {
    let judged = |answer: Response, answered: &[EventData], source: Option<CalendarSource>| match sent {
        Some((events, at)) if !holds(answered, source, events, at, here) => Response::NeedsAction,
        _ => answer,
    };
    match copy {
        Some(s) if !matches!(s.source, CalendarSource::Webcal | CalendarSource::Local) => s.event.my_response().map(|r| judged(r, std::slice::from_ref(&s.event), Some(s.source))),
        copy => match remembered {
            Some(Remembered { answer, version: Some(answered) }) => Some(judged(*answer, answered, None)),
            Some(Remembered { answer, version: None }) => Some(*answer),
            None => copy.and_then(|s| s.event.my_response()),
        },
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
    /// The mail's From address.
    from: String,
    uid: String,
    recurrence_id: String,
    sequence: i64,
    method: Method,
    organizer: String,
    ends: i64,
    timing: String,
}

const NAMED: &str = "SELECT i.message_id, m.thread_id, m.account_id, a.address, m.from_addr, i.uid, i.recurrence_id, i.sequence, i.method, i.organizer, i.ends, i.timing
     FROM invitation i JOIN message m ON m.id = i.message_id JOIN account a ON a.id = m.account_id";

fn named(r: &Row) -> rusqlite::Result<Named> {
    Ok(Named {
        message: r.get(0)?,
        thread: r.get(1)?,
        account: r.get(2)?,
        receiver: r.get(3)?,
        from: r.get(4)?,
        uid: r.get(5)?,
        recurrence_id: r.get(6)?,
        sequence: r.get(7)?,
        method: r.get(8)?,
        organizer: r.get(9)?,
        ends: r.get(10)?,
        timing: r.get(11)?,
    })
}

/// The invitations of a page of conversations, a JSON array of thread ids,
/// in the order that puts the one each conversation shows first: its newest
/// message from the organizer, such as a request, an update or a
/// cancellation, else its newest reply, and a message's whole event or
/// series before its changed occurrences. `shown` passes over the messages
/// that aren't the organizer's. One statement for the page: a
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
    if rows.is_empty() {
        return Ok(HashMap::new());
    }
    let me = conn.prepare_cached("SELECT address FROM account")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
    let mut out = HashMap::new();
    for (i, n) in rows.iter().enumerate() {
        if !out.contains_key(&n.thread) {
            // The same message's changed occurrences follow its series.
            let overrides: Vec<&Named> = rows[i + 1..].iter().take_while(|o| o.message == n.message).filter(|o| o.uid == n.uid).collect();
            if let Some(shown) = state(conn, n, &overrides, &me, now, here)? {
                out.insert(n.thread, shown);
            }
        }
    }
    // A conversation with nothing from the organizer has the mark alone.
    for n in &rows {
        out.entry(n.thread).or_insert(Shown { state: InvitationState::Plain, until: None });
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
/// its card answers it. None for a request or a cancellation that isn't
/// from the meeting's organizer, as `update::sent_by` decides for the card:
/// it is no version of the meeting.
fn state(conn: &Connection, n: &Named, overrides: &[&Named], me: &[String], now: DateTime<Utc>, here: Tz) -> Result<Option<Shown>> {
    let fixed = |state| Ok(Some(Shown { state, until: None }));
    // The user's own meeting has the mark alone, without asking who sent it.
    match n.method {
        Method::Request if n.organizer.trim().eq_ignore_ascii_case(&n.receiver) => return fixed(InvitationState::Plain),
        Method::Request | Method::Cancel => {}
        _ => return fixed(InvitationState::Plain),
    }
    let organizer = update::organizer(conn, n.account, me, &n.uid)?;
    let organizers = |named: &str, from: &str| update::sent_by(me, organizer.as_deref(), named, from);
    if !organizers(&n.organizer, &n.from) {
        return Ok(None);
    }
    if n.method == Method::Cancel {
        return fixed(InvitationState::Cancelled);
    }
    let picked = pick(n, overrides, now, here)?;
    // The account's copy of the event or series, and of the occurrence shown
    // when the calendar holds one, in its own calendars only, as the card
    // finds it. A series' copy answers for an occurrence without its own
    // copy only when the message is the series'.
    let owned = |c: &Calendar| c.is_own(&n.receiver);
    let whole = stored_event_in(conn, &n.uid, None, Some(n.account), here, owned)?;
    let own = match &picked.event {
        Some(at) => stored_event_in(conn, &n.uid, Some(at), Some(n.account), here, owned)?,
        None => None,
    };
    let copy_cancelled = own.iter().chain(&whole).any(|c| update::cancels(c, n.sequence));
    let copy = own.or(whole.filter(|_| n.recurrence_id.is_empty()));
    // The organizer's cancellation of this event, or of the series this
    // occurrence is of, at the same SEQUENCE or later.
    let rid = moment_from_text(&n.recurrence_id);
    let cancelled = conn
        .prepare_cached(
            "SELECT i.recurrence_id, i.organizer, m.from_addr FROM invitation i JOIN message m ON m.id = i.message_id
             WHERE i.uid = ?1 AND i.method = 'CANCEL' AND i.sequence >= ?2 AND m.account_id = ?3 AND (m.blob IS NULL OR m.blob NOT LIKE 'synth:%')",
        )?
        .query_map(params![n.uid, n.sequence, n.account], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .any(|(cancels, named, from)| {
            let event = match (moment_from_text(&cancels), &rid) {
                (None, _) => true,
                (Some(c), Some(rid)) => c.names(rid, here),
                (Some(_), None) => false,
            };
            event && organizers(&named, &from)
        });
    if cancelled || copy_cancelled {
        return fixed(InvitationState::Cancelled);
    }
    let remembered = remembered_answer(conn, n.account, &n.uid, picked.occurrence.as_ref())?;
    let sent = picked.sent.as_deref().map(|events| (events, picked.occurrence.as_ref()));
    let answer = shown_answer(copy.as_ref(), remembered.as_ref(), sent, here);
    let state = match answer {
        Some(r) if r != Response::NeedsAction => InvitationState::Answered(r),
        _ if n.ends > now.timestamp() => InvitationState::Unanswered,
        _ => InvitationState::Plain,
    };
    let unanswered = (state == InvitationState::Unanswered).then_some(n.ends);
    let until = picked.ends.into_iter().chain(unanswered).filter(|&t| t > now.timestamp()).min();
    Ok(Some(Shown { state, until }))
}

/// Where a request's answer is read from, as its card picks it.
struct Picked {
    /// The recurrence id of the event whose calendar copy holds the answer.
    event: Option<Moment>,
    /// The occurrence an answer given from Jolta names.
    occurrence: Option<Moment>,
    /// For a series, when the occurrence it shows ends, in unix seconds.
    ends: Option<i64>,
    /// The events the message sends, as `timing` places them. None for a
    /// message that gives no time.
    sent: Option<Vec<EventData>>,
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
    if rid.is_some() || this.recurrence.is_none() {
        return Ok(Picked { event: rid, occurrence: rid, ends: None, sent: Some(vec![this]) });
    }
    let overrides = overrides.iter().filter(|o| !o.timing.is_empty()).map(|o| parse(o)).collect::<Result<Vec<_>>>()?;
    let shown = series::shown_occurrence(&this, &overrides, now, here);
    let ends = shown.map(|o| end_of(&o.span, here).timestamp());
    let (event, occurrence) = match shown.map(|o| (o.source, o.span.start())) {
        None => (None, Some(this.span.start())),
        Some((Source::Override(i), _)) => (overrides[i].recurrence_id, overrides[i].recurrence_id),
        // The calendar may hold this occurrence as an override the mail
        // doesn't have; the series' copy answers when it doesn't.
        Some((Source::Master, start)) => (Some(start), Some(start)),
    };
    Ok(Picked { event, occurrence, ends, sent: Some([this].into_iter().chain(overrides).collect()) })
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

        let given = GivenAnswer { answer: Response::Tentative, note: String::new(), notify: true, proposal: None };
        let version = crate::calendar::invitation::find(mail!("google-request.eml"), &[]).remove(0).event;
        jolta.store.write(|tx| remember_answer(tx, AccountId(1), &version.uid, None, &given, std::slice::from_ref(&version), None)).unwrap();
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
    /// A message that isn't from the meeting's organizer has the mark alone.
    fn card_says(jolta: &crate::Jolta, message: MessageId, raw: &[u8], now: DateTime<Utc>) -> InvitationState {
        let e = event_card(jolta, message, raw, now);
        let says = |text: &str| e.notes.iter().any(|n| n.text == text);
        if says("This isn't from the meeting's organizer") {
            assert!(e.answering.is_none() && e.answer.is_none(), "no buttons and no answer on a stranger's card");
            return InvitationState::Plain;
        }
        if e.cancelled || says("The organizer has since cancelled this meeting") {
            return InvitationState::Cancelled;
        }
        match e.answer {
            Some(r) if r != Response::NeedsAction => InvitationState::Answered(r),
            _ => InvitationState::Unanswered,
        }
    }

    fn event_card(jolta: &crate::Jolta, message: MessageId, raw: &[u8], now: DateTime<Utc>) -> crate::calendar::card::EventCard {
        use crate::calendar::card;
        let me = ["me@example.com".to_string()];
        let found = crate::calendar::invitation::find(raw, &me);
        let sender: String = jolta.store.read(|c| Ok(c.query_row("SELECT from_addr FROM message WHERE id = ?1", [message], |r| r.get(0))?)).unwrap();
        let cx = card::Context { message, account: AccountId(1), me: &me, sender: &sender, now, here: chrono_tz::Europe::Brussels, blobs: &jolta.blobs };
        match jolta.store.read(|c| card::cards(c, &found, &cx)).unwrap().remove(0) {
            card::Card::Event(e) => e,
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
        let sent: Vec<EventData> = crate::calendar::invitation::find(mail!("series.eml"), &[]).into_iter().map(|i| i.event).collect();
        for (occurrence, answer) in [("2026-11-05 09:30", Response::Accepted), ("2026-11-10 09:30", Response::Declined)] {
            let given = GivenAnswer { answer, note: String::new(), notify: true, proposal: None };
            let version = series::occurrence(&sent, &at(occurrence), chrono_tz::Europe::Brussels).unwrap();
            jolta.store.write(|tx| remember_answer(tx, AccountId(1), uid, Some(&at(occurrence)), &given, std::slice::from_ref(&version), None)).unwrap();
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

        // Tuesday's recurrence id written in UTC names the same occurrence.
        let Moment::Time(local) = rid else { unreachable!() };
        let utc = Moment::Time(crate::calendar::Local { wall: local.instant(chrono_tz::UTC).naive_utc(), zone: crate::calendar::Zone::Tz(chrono_tz::UTC) });
        events.iter_mut().filter(|e| e.recurrence_id.is_some()).for_each(|e| e.recurrence_id = Some(utc));
        jolta.store.write(|tx| crate::store::apply_item(tx, calendar, "s1", Some("3"), &events, None, &horizon).map(|_| ())).unwrap();
        assert_eq!(card_says(&jolta, series, mail!("series.eml"), after_thursday), accepted, "the card: Tuesday's copy, written in UTC");
        assert_eq!(shown(&jolta, series, after_thursday), Some(accepted), "the list: Tuesday's copy, written in UTC");

        // The same copies in another account's calendar aren't the user's
        // answer on the account the invitation came to.
        let other = jolta
            .store
            .write(|tx| {
                tx.execute("INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (2, 'imap', 'other@example.com', 'o', 'a2', 0, 1)", [])?;
                let remote = crate::store::RemoteCalendar { remote_id: "/cal/other/".into(), name: "Other".into(), color: None, zone: None, access: crate::calendar::Access::Owner, primary: true, refresh: None, video: None };
                let other = crate::store::sync_calendar_list(tx, AccountId(2), CalendarSource::CalDav, &[remote])?[0].id;
                tx.execute("DELETE FROM event", [])?;
                crate::store::apply_item(tx, other, "s1", Some("1"), &events, None, &horizon)?;
                Ok(other)
            })
            .unwrap();
        assert_ne!(other, calendar);
        assert_eq!(card_says(&jolta, series, mail!("series.eml"), after_thursday), InvitationState::Unanswered, "the card: another account's copy");
        assert_eq!(shown(&jolta, series, after_thursday), Some(InvitationState::Unanswered), "the list: another account's copy");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    /// `raw` with each line `edit` returns a new one for changed. A fixture
    /// may end a line in a stray CR, which `edit` doesn't see.
    fn edited(raw: &[u8], edit: impl Fn(&str) -> Option<String>) -> Vec<u8> {
        let text = String::from_utf8_lossy(raw);
        let line = |l: &str| {
            let bare = l.trim_end_matches('\r');
            edit(bare).map_or_else(|| l.to_string(), |new| new + &l[bare.len()..])
        };
        text.split("\r\n").map(line).collect::<Vec<_>>().join("\r\n").into_bytes()
    }

    /// The organizer's update of `raw` at SEQUENCE 1, the next morning: two
    /// hours later when `moved`, else only retitled.
    fn update(raw: &[u8], moved: bool) -> Vec<u8> {
        edited(raw, |l| {
            let later = |l: &str| {
                let hour = l.rfind(':').unwrap() + 10;
                format!("{}{:02}{}", &l[..hour], l[hour..hour + 2].parse::<u32>().unwrap() + 2, &l[hour + 2..])
            };
            match l {
                _ if l.starts_with("Message-ID: <") => Some(l.replace('<', "<update-")),
                _ if l.starts_with("Date:") => Some("Date: Wed, 04 Nov 2026 07:00:00 +0000".into()),
                "SEQUENCE:0" => Some("SEQUENCE:1".into()),
                _ if moved && (l.starts_with("DTSTART") || l.starts_with("DTEND")) => Some(later(l)),
                _ if !moved && l.starts_with("SUMMARY:") => Some(format!("{l}, renamed")),
                _ => None,
            }
        })
    }

    /// Mallory's `method` for the event of `raw`, at SEQUENCE 100, in a
    /// conversation of its own, naming Ann as the organizer or Mallory.
    fn stranger(raw: &[u8], method: &str, names_self: bool) -> Vec<u8> {
        edited(raw, |l| match l {
            _ if l.starts_with("From:") => Some("From: Mallory <mallory@example.com>".into()),
            _ if l.starts_with("Subject:") => Some("Subject: Urgent: meeting changed".into()),
            _ if l.starts_with("Message-ID: <") => Some(l.replace('<', "<stranger-")),
            _ if l.starts_with("Date:") => Some("Date: Wed, 04 Nov 2026 07:00:00 +0000".into()),
            "SEQUENCE:0" => Some("SEQUENCE:100".into()),
            "METHOD:REQUEST" => Some(format!("METHOD:{method}")),
            _ if l.contains("method=REQUEST") => Some(l.replace("method=REQUEST", &format!("method={method}"))),
            _ if names_self && l.starts_with("ORGANIZER") => Some(l.replace("ann@example.com", "mallory@example.com")),
            _ => None,
        })
    }

    /// The organizer's cancellation of the events of `raw`, at SEQUENCE 1.
    fn cancelled(raw: &[u8]) -> Vec<u8> {
        edited(raw, |l| match l {
            _ if l.starts_with("Subject:") => Some("Subject: Cancelled".into()),
            _ if l.starts_with("Message-ID: <") => Some(l.replace('<', "<cancel-")),
            "SEQUENCE:0" => Some("SEQUENCE:1".into()),
            "METHOD:REQUEST" => Some("METHOD:CANCEL".into()),
            _ if l.contains("method=REQUEST") => Some(l.replace("method=REQUEST", "method=CANCEL")),
            _ => None,
        })
    }

    /// Every mix of a calendar copy, of the event or of the occurrence shown
    /// or both, or the occurrence's in a series the calendar holds cancelled,
    /// on CalDAV or Graph, and an answer given from Jolta to the
    /// whole event or to that occurrence, with or without a reply mail, for a
    /// single event, an unchanged and a moved occurrence of a series and an
    /// occurrence invited to alone; then again after the organizer's update
    /// that moves it, only retitles it or leaves out the moved occurrence, the organizer's cancellation of the
    /// whole series or of Thursday's occurrence named in UTC, and a
    /// stranger's CANCEL or REQUEST for the same UID: the card and the list
    /// say the same.
    #[test]
    fn the_card_and_the_list_show_the_same_answer() {
        use crate::calendar::rsvp::{self, Answer, ImipReply};
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
        let thursday_in_utc = String::from_utf8_lossy(&alone).replace("RECURRENCE-ID;TZID=Europe/Brussels:20261105T093000", "RECURRENCE-ID:20261105T083000Z");
        let moved_part = &text[first + master_part.len()..];
        let moved_part = &moved_part[..moved_part.find("END:VEVENT").unwrap() + "END:VEVENT\r\n".len()];
        let without_move = text.replace(moved_part, "").into_bytes();
        let single = request("single@example.com", "", "single-1", "20261112T150000Z", "20261112T154500Z", 0);
        let after_thursday = Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0).unwrap();
        let me = ["me@example.com".to_string()];
        // (what, mail, when, the occurrence the card shows)
        let shapes: [(&str, Vec<u8>, DateTime<Utc>, Option<Moment>); 4] = [
            ("a single event", single, now(), None),
            ("an unchanged occurrence", series.clone(), after_thursday, Some(at("2026-11-10 09:30"))),
            ("a moved occurrence", series.clone(), now(), Some(at("2026-11-05 09:30"))),
            ("an occurrence invited to alone", alone, now(), Some(at("2026-11-05 09:30"))),
        ];
        let whole_series: Vec<EventData> = crate::calendar::invitation::find(&series, &me).into_iter().map(|i| i.event).collect();
        let series_master = whole_series.iter().find(|e| e.recurrence_id.is_none()).unwrap().clone();
        let answer = |e: &mut EventData, r: Response| e.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = r);
        let mut seen = std::collections::BTreeSet::new();
        let mut before = HashMap::new();
        for (what, raw, now, occurrence) in shapes {
            let whole_event = if occurrence.is_some() { series.clone() } else { raw.clone() };
            let arrivals: [(&str, Option<Vec<u8>>); 10] = [
                ("nothing since", None),
                ("an update that moved it", Some(update(&raw, true))),
                ("an update that kept its time", Some(update(&raw, false))),
                ("an update that puts the moved occurrence back", (what == "a moved occurrence").then(|| update(&without_move, false))),
                ("the organizer's CANCEL of the whole event", Some(cancelled(&whole_event))),
                ("the organizer's CANCEL of Thursday's occurrence", occurrence.map(|_| cancelled(thursday_in_utc.as_bytes()))),
                ("a stranger's CANCEL naming the organizer", Some(stranger(&raw, "CANCEL", false))),
                ("a stranger's CANCEL naming themselves", Some(stranger(&raw, "CANCEL", true))),
                ("a stranger's REQUEST naming the organizer", Some(stranger(&raw, "REQUEST", false))),
                ("a stranger's REQUEST naming themselves", Some(stranger(&raw, "REQUEST", true))),
            ];
            for (arrival, later) in arrivals.into_iter().filter(|(a, later)| later.is_some() || a.starts_with("nothing")) {
                let (jolta, caldav) = jolta(&format!("same-answer-{what}-{arrival}").replace([' ', '\''], "-"));
                let graph = jolta
                    .store
                    .write(|tx| {
                        let remote = crate::store::RemoteCalendar { remote_id: "graph".into(), name: "Calendar".into(), color: None, zone: None, access: crate::calendar::Access::Owner, primary: true, refresh: None, video: None };
                        Ok(crate::store::sync_calendar_list(tx, AccountId(1), CalendarSource::Graph, &[remote])?[0].id)
                    })
                    .unwrap();
                let message = import(&jolta, &raw);
                let later = later.map(|r| (import(&jolta, &r), r));
                // An update's card stands for the request's; the list doesn't
                // know of updates in other conversations. A cancellation's own
                // card is Cancelled in both by its method alone.
                let compared: Vec<(&str, MessageId, &[u8])> = match &later {
                    Some((m, r)) if arrival.contains("update") => vec![("the update", *m, r.as_slice())],
                    Some((m, r)) if arrival.contains("stranger") => vec![("the request", message, raw.as_slice()), ("the stranger's mail", *m, r.as_slice())],
                    _ => vec![("the request", message, raw.as_slice())],
                };
                let sent: Vec<EventData> = crate::calendar::invitation::find(&raw, &me).into_iter().map(|i| i.event).collect();
                let uid = sent[0].uid.clone();
                let base = sent.iter().find(|e| e.recurrence_id.is_none()).cloned().unwrap_or_else(|| series_master.clone());
                // The whole event answered from Jolta, from this message or
                // for an occurrence invited to alone from the series'.
                let whole_version = if sent.iter().any(|e| e.recurrence_id.is_none()) { sent.clone() } else { whole_series.clone() };
                let mut whole = base.clone();
                answer(&mut whole, Response::Declined);
                let whole_cancelled = EventData { status: crate::calendar::EventStatus::Cancelled, ..whole.clone() };
                let mut own = occurrence.map(|rid| crate::calendar::series::occurrence(&sent, &rid, brussels).or_else(|| crate::calendar::series::occurrence(std::slice::from_ref(&base), &rid, brussels)).unwrap());
                let own_version = own.clone();
                if let Some(own) = &mut own {
                    answer(own, Response::Accepted);
                }
                let own_cancelled = own.clone().map(|o| EventData { status: crate::calendar::EventStatus::Cancelled, ..o });
                let copies: Vec<(&str, Vec<EventData>)> = [
                    ("no copy", vec![]),
                    ("the event's copy", vec![whole.clone()]),
                    ("the occurrence's copy", own.iter().cloned().collect()),
                    ("both copies", [whole.clone()].into_iter().chain(own.clone()).collect()),
                    ("the event's cancelled copy and the occurrence's", [whole_cancelled.clone()].into_iter().chain(own.clone()).collect()),
                    ("the event's copy and the occurrence's cancelled copy", [whole.clone()].into_iter().chain(own_cancelled.clone()).collect()),
                ]
                .into_iter()
                .filter(|(name, c)| occurrence.is_some() || (c.len() < 2 && !name.contains("occurrence's cancelled")))
                .collect();
                let text = crate::calendar::invitation::calendar_text(&raw, &uid).unwrap();
                for (calendar, source) in [(caldav, "CalDAV"), (graph, "Graph")] {
                    for (copy, events) in &copies {
                        let answers = [("no answer from Jolta", None), ("the event answered from Jolta", Some(None)), ("the occurrence answered from Jolta", occurrence.map(Some))];
                        for (remembered, rid) in answers.into_iter().filter(|(w, rid)| rid.is_some() || w.starts_with("no")) {
                            for reply in [false, true].into_iter().filter(|r| !r || rid.is_some()) {
                                let cell = format!("{what}, {copy} on {source}, {remembered}{}", if reply { " with a reply" } else { "" });
                                jolta
                                    .store
                                    .write(|tx| {
                                        tx.execute_batch("DELETE FROM event; DELETE FROM invitation_answer; DELETE FROM outbox;")?;
                                        if !events.is_empty() {
                                            let horizon = Horizon::around(now, brussels);
                                            crate::store::apply_item(tx, calendar, "c1", Some("1"), events, None, &horizon)?;
                                        }
                                        let Some(rid) = rid else { return Ok(()) };
                                        let (said, version) = match rid {
                                            None => ((Answer::Maybe, Response::Tentative), whole_version.clone()),
                                            Some(_) => ((Answer::Accept, Response::Accepted), own_version.clone().into_iter().collect()),
                                        };
                                        let given = GivenAnswer { answer: said.1, note: String::new(), notify: reply, proposal: None };
                                        let reply = match reply {
                                            true => {
                                                let built = rsvp::imip_reply(&ImipReply { invitation: &text, from: "me@example.com", me: &me, answer: said.0, note: None, occurrence: rid }, brussels)?;
                                                let blob = jolta.blobs.put(&built.raw)?;
                                                Some(crate::store::queue_outbox(tx, AccountId(1), None, &blob, &built.message_id, &built.from, &built.recipients, None, 0)?)
                                            }
                                            false => None,
                                        };
                                        remember_answer(tx, AccountId(1), &uid, rid.as_ref(), &given, &version, reply)
                                    })
                                    .unwrap();
                                for (which, m, r) in &compared {
                                    let card = card_says(&jolta, *m, r, now);
                                    assert_eq!(Some(card), shown(&jolta, *m, now), "{cell}, after {arrival}: {which}");
                                    seen.insert(format!("{card:?}"));
                                }
                                let card = |i: usize| card_says(&jolta, compared[i].1, compared[i].2, now);
                                // Graph keeps no SEQUENCE: its cancelled series covers an update too.
                                let in_cancelled = copy.starts_with("the event's cancelled copy");
                                let unanswered = match in_cancelled && source == "Graph" {
                                    true => InvitationState::Cancelled,
                                    false => InvitationState::Unanswered,
                                };
                                // Which occurrence an update shows decides whether the cancelled
                                // one is on screen; the card and the list agreeing is the check.
                                let only_agreement = copy.contains("occurrence's cancelled copy") && arrival != "nothing since";
                                match arrival {
                                    _ if only_agreement => {}
                                    "nothing since" => {
                                        if in_cancelled {
                                            assert_eq!(card(0), InvitationState::Cancelled, "{cell}: the series cancelled");
                                        }
                                        if copy.contains("occurrence's cancelled copy") {
                                            assert_eq!(card(0), InvitationState::Cancelled, "{cell}: the occurrence shown cancelled");
                                        }
                                        before.insert(cell, card(0));
                                    }
                                    // Only the series' copy places Thursday where the update does.
                                    "an update that puts the moved occurrence back" if *copy == "the event's copy" => {
                                        assert_eq!(card(0), InvitationState::Answered(Response::Declined), "{cell}: {arrival}")
                                    }
                                    "an update that puts the moved occurrence back" => assert_eq!(card(0), unanswered, "{cell}: answered at 10:00, now at 09:30"),
                                    "an update that moved it" => {
                                        assert_eq!(card(0), unanswered, "{cell}: answered before the move");
                                        if *copy == "no copy" && let (Some(rid), Some((m, r))) = (rid, &later) {
                                            // Answered again, for the version that moved it.
                                            let moved: Vec<EventData> = crate::calendar::invitation::find(r, &me).into_iter().map(|i| i.event).collect();
                                            let shown_at = rid.map(|_| event_card(&jolta, *m, r, now).answering.unwrap().occurrence.unwrap().recurrence_id);
                                            let version = match &shown_at {
                                                Some(at) => vec![crate::calendar::series::occurrence(&moved, at, brussels).unwrap()],
                                                None => moved.clone(),
                                            };
                                            let given = GivenAnswer { answer: Response::Declined, note: String::new(), notify: false, proposal: None };
                                            jolta.store.write(|tx| remember_answer(tx, AccountId(1), &uid, shown_at.as_ref(), &given, &version, None)).unwrap();
                                            let card = card_says(&jolta, *m, r, now);
                                            assert_eq!(Some(card), shown(&jolta, *m, now), "{cell}, answered again after the move");
                                            assert_eq!(card, InvitationState::Answered(Response::Declined), "{cell}, answered again after the move");
                                        }
                                    }
                                    "the organizer's CANCEL of the whole event" => assert_eq!(card(0), InvitationState::Cancelled, "{cell}: {arrival}"),
                                    "the organizer's CANCEL of Thursday's occurrence" if what.ends_with("alone") => assert_eq!(card(0), InvitationState::Cancelled, "{cell}: {arrival}"),
                                    "the organizer's CANCEL of Thursday's occurrence" => {}
                                    // The copies here leave out the series' moved occurrence,
                                    // so only an answer from Jolta places it as the mail does.
                                    "an update that kept its time" if *copy == "no copy" => assert_eq!(card(0), before[&cell], "{cell}: answered before the new title"),
                                    "an update that kept its time" => {}
                                    _ => {
                                        assert_eq!(card(0), before[&cell], "{cell}: the request after {arrival}");
                                        assert_eq!(card(1), InvitationState::Plain, "{cell}: {arrival}");
                                    }
                                }
                            }
                        }
                    }
                }
                std::fs::remove_dir_all(&jolta.data_dir).unwrap();
            }
        }
        assert_eq!(seen.len(), 6, "Not answered, Accepted, Maybe, Declined, Cancelled and the mark alone all turn up: {seen:?}");
    }

    /// A copy in a calendar shared with the account is its owner's, whom
    /// Google marks as the copy's self, even with the owner role Google
    /// gives "Make changes and manage sharing". For a single event and for
    /// the moved occurrence of a series the card shows, the card and the
    /// list show the answer in the account's own calendar, else the one
    /// given from Jolta, and never the owner's.
    #[test]
    fn a_shared_calendars_copy_never_shows_the_users_answer() {
        use crate::calendar::Access::{Owner, Write};
        use crate::calendar::{Attendee, Person, Role};
        let brussels = chrono_tz::Europe::Brussels;
        let me = ["me@example.com".to_string()];
        let dee = Attendee { person: Person { address: "dee@example.com".into(), name: String::new() }, role: Role::Required, response: Response::Declined, comment: String::new(), is_me: true };
        let wall = chrono::NaiveDateTime::parse_from_str("2026-11-05 09:30", "%Y-%m-%d %H:%M").unwrap();
        let thursday = Moment::Time(crate::calendar::Local { wall, zone: crate::calendar::Zone::Tz(brussels) });
        let horizon = Horizon::around(now(), brussels);
        for (what, raw, occurrence) in [("a single event", &mail!("google-request.eml")[..], None), ("a moved occurrence", &mail!("series.eml")[..], Some(thursday))] {
            let sent: Vec<EventData> = crate::calendar::invitation::find(raw, &me).into_iter().map(|i| i.event).collect();
            let uid = sent[0].uid.clone();
            // A calendar's copy lists the guests on every occurrence, which
            // the mail leaves out of the moved one.
            let guests = sent.iter().find(|e| e.recurrence_id.is_none()).unwrap().attendees.clone();
            let mut copy = sent.clone();
            copy.iter_mut().filter(|e| e.attendees.is_empty()).for_each(|e| e.attendees = guests.clone());
            let mut mine = copy.clone();
            mine.iter_mut().flat_map(|e| &mut e.attendees).filter(|a| a.is_me).for_each(|a| a.response = Response::Accepted);
            let mut theirs = copy.clone();
            for e in &mut theirs {
                e.attendees.iter_mut().for_each(|a| a.is_me = false);
                e.attendees.push(dee.clone());
            }
            let version = match &occurrence {
                Some(rid) => vec![series::occurrence(&sent, rid, brussels).unwrap()],
                None => sent.clone(),
            };
            for (source, access, remote_id) in [
                (CalendarSource::Google, Owner, "dee@example.com"),
                (CalendarSource::Google, Write, "dee@example.com"),
                (CalendarSource::Graph, Write, "AAMkDee"),
                (CalendarSource::CalDav, Write, "/cal/dee/"),
            ] {
                let (jolta, own) = jolta(&format!("shared-answer-{what}-{source:?}-{access:?}").replace(' ', "-"));
                let message = import(&jolta, raw);
                // A CalDAV list replaces the account's CalDAV calendars, the own one too.
                let shared = jolta
                    .store
                    .write(|tx| {
                        let remote = |remote_id: &str, access| crate::store::RemoteCalendar { remote_id: remote_id.into(), name: "Calendar".into(), color: None, zone: None, access, primary: true, refresh: None, video: None };
                        let list = match source {
                            CalendarSource::CalDav => vec![remote("/cal/work/", Owner), remote(remote_id, access)],
                            _ => vec![remote(remote_id, access)],
                        };
                        let synced = crate::store::sync_calendar_list(tx, AccountId(1), source, &list)?;
                        Ok(synced.iter().find(|c| c.remote_id == remote_id).unwrap().id)
                    })
                    .unwrap();
                for (in_own, remembered, expected) in [
                    (false, false, InvitationState::Unanswered),
                    (false, true, InvitationState::Answered(Response::Tentative)),
                    (true, false, InvitationState::Answered(Response::Accepted)),
                    (true, true, InvitationState::Answered(Response::Accepted)),
                ] {
                    jolta
                        .store
                        .write(|tx| {
                            tx.execute_batch("DELETE FROM event; DELETE FROM invitation_answer;")?;
                            crate::store::apply_item(tx, shared, "theirs", Some("1"), &theirs, None, &horizon)?;
                            if in_own {
                                crate::store::apply_item(tx, own, "mine", Some("1"), &mine, None, &horizon)?;
                            }
                            if remembered {
                                let given = GivenAnswer { answer: Response::Tentative, note: String::new(), notify: true, proposal: None };
                                remember_answer(tx, AccountId(1), &uid, occurrence.as_ref(), &given, &version, None)?;
                            }
                            Ok(())
                        })
                        .unwrap();
                    let case = format!("{what} on {source:?} calendar {remote_id} with {access:?}, in the own calendar {in_own}, answered from Jolta {remembered}");
                    assert_eq!(card_says(&jolta, message, raw, now()), expected, "the card: {case}");
                    assert_eq!(shown(&jolta, message, now()), Some(expected), "the list: {case}");
                }
                // The owner's copy cancelled, as when the organizer took only
                // the owner off: the user's invitation still stands.
                let cancelled: Vec<EventData> = theirs.iter().map(|e| EventData { status: crate::calendar::EventStatus::Cancelled, sequence: e.sequence + 1, ..e.clone() }).collect();
                jolta
                    .store
                    .write(|tx| {
                        tx.execute_batch("DELETE FROM event; DELETE FROM invitation_answer;")?;
                        crate::store::apply_item(tx, shared, "theirs", Some("2"), &cancelled, None, &horizon).map(|_| ())
                    })
                    .unwrap();
                let case = format!("{what} cancelled on {source:?} calendar {remote_id} with {access:?}");
                assert_eq!(card_says(&jolta, message, raw, now()), InvitationState::Unanswered, "the card: {case}");
                assert_eq!(shown(&jolta, message, now()), Some(InvitationState::Unanswered), "the list: {case}");
                std::fs::remove_dir_all(&jolta.data_dir).unwrap();
            }
        }
    }

    /// The organizer is the one the account the mail came to knows: another
    /// account's copy naming a stranger as organizer lets none of the
    /// stranger's mail count.
    #[test]
    fn another_accounts_copy_makes_nobody_the_organizer() {
        let (jolta, _) = jolta("organizer-account");
        let raw = request("q9@example.com", "", "sync-9", "20991112T150000Z", "20991112T160000Z", 0);
        let mut theirs = crate::calendar::invitation::find(&raw, &[]).remove(0).event;
        theirs.organizer.as_mut().unwrap().address = "mallory@example.com".into();
        jolta
            .store
            .write(|tx| {
                tx.execute("INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (2, 'imap', 'other@example.com', 'o', 'a2', 0, 1)", [])?;
                let remote = crate::store::RemoteCalendar { remote_id: "/cal/other/".into(), name: "Other".into(), color: None, zone: None, access: crate::calendar::Access::Owner, primary: true, refresh: None, video: None };
                let other = crate::store::sync_calendar_list(tx, AccountId(2), CalendarSource::CalDav, &[remote])?[0].id;
                crate::store::apply_item(tx, other, "s9", Some("1"), &[theirs], None, &Horizon::around(now(), chrono_tz::Europe::Brussels)).map(|_| ())
            })
            .unwrap();
        let mine = import(&jolta, &raw);
        let cancel = stranger(&raw, "CANCEL", true);
        let cancel = (import(&jolta, &cancel), cancel);
        for (which, m, r, expected) in [("the request", mine, raw.as_slice(), InvitationState::Unanswered), ("the stranger's CANCEL", cancel.0, cancel.1.as_slice(), InvitationState::Plain)] {
            assert_eq!(card_says(&jolta, m, r, now()), expected, "the card of {which}");
            assert_eq!(shown(&jolta, m, now()), Some(expected), "the list of {which}");
        }
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
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
