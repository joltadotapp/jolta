//! Hold sets (`calendar::holds`) with their slots, the stored events that
//! carry a hold marker, and the mail `holds::seen` reads.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::calendar::holds::{HeldSlot, HoldSet, HoldSetId};
use crate::calendar::{CalendarId, EventData};
use crate::model::AccountId;
use crate::{Error, Result};

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap_or_default()
}

/// Every set with its slots, oldest first.
pub fn hold_sets(conn: &Connection) -> Result<Vec<HoldSet>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, account_id, calendar_id, draft_id, message_id, recipients, subject, sent FROM hold_set ORDER BY created, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, AccountId>(1)?,
            r.get::<_, CalendarId>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<i64>>(7)?,
        ))
    })?;
    let mut sets = Vec::new();
    for row in rows {
        let (id, account, calendar, draft, message_id, recipients, subject, sent) = row?;
        let Some(id) = HoldSetId::parse(&id) else { continue };
        let slots = slots(conn, &id)?;
        let recipients = recipients.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        sets.push(HoldSet { id, account, message_id, draft, recipients, subject, calendar, sent: sent.map(at), slots });
    }
    Ok(sets)
}

fn slots(conn: &Connection, set: &HoldSetId) -> Result<Vec<HeldSlot>> {
    let mut stmt = conn.prepare_cached("SELECT start, end, line, state, remote_id FROM hold WHERE set_id = ?1 ORDER BY start")?;
    let rows = stmt.query_map([set.as_str()], |r| {
        Ok(HeldSlot { start: at(r.get(0)?), end: at(r.get(1)?), line: r.get(2)?, state: r.get(3)?, remote_id: r.get(4)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Stores a new set with its slots. A set id that is there already is an
/// error, so two Macs building the same set keep one.
pub fn insert_hold_set(conn: &Connection, set: &HoldSet, created: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "INSERT INTO hold_set (id, account_id, calendar_id, draft_id, message_id, recipients, subject, created, sent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            set.id.as_str(),
            set.account,
            set.calendar,
            set.draft,
            set.message_id,
            set.recipients.join("\n"),
            set.subject,
            created.timestamp(),
            set.sent.map(|s| s.timestamp())
        ],
    )?;
    let mut stmt = conn.prepare_cached("INSERT INTO hold (set_id, start, end, line, state, remote_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?;
    for s in &set.slots {
        stmt.execute(params![set.id.as_str(), s.start.timestamp(), s.end.timestamp(), s.line, s.state, s.remote_id])?;
    }
    Ok(())
}

/// A stored event with a hold marker.
#[derive(Clone, Debug)]
pub struct MarkedEvent {
    pub calendar: CalendarId,
    pub remote_id: String,
    pub event: EventData,
}

/// The stored hold events of `set`, or of every set.
pub fn hold_events(conn: &Connection, set: Option<&HoldSetId>) -> Result<Vec<MarkedEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT calendar_id, remote_id, data FROM event
         WHERE json_extract(data, '$.mark.is') = 'hold' AND (?1 IS NULL OR json_extract(data, '$.mark.set') = ?1)
         ORDER BY calendar_id, id",
    )?;
    let rows = stmt.query_map([set.map(HoldSetId::as_str)], |r| Ok((r.get::<_, CalendarId>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (calendar, remote_id, data) = row?;
        let event: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("event data: {e}")))?;
        out.push(MarkedEvent { calendar, remote_id, event });
    }
    Ok(out)
}

/// One stored event's data.
pub fn event_data(conn: &Connection, id: crate::calendar::EventId) -> Result<Option<EventData>> {
    let data: Option<String> = conn.query_row("SELECT data FROM event WHERE id = ?1", [id], |r| r.get(0)).optional()?;
    Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
}

/// A mail of the user's as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredMail {
    pub account: AccountId,
    pub thread: crate::ThreadId,
    pub date: DateTime<Utc>,
    pub to: String,
    pub cc: String,
    pub subject: String,
}

/// The mail with this Message-ID, with or without its angle brackets: the
/// copy from the account's own address first, so the one the user sent
/// wins over the copy another of their accounts received.
pub fn sent_mail(conn: &Connection, message_id: &str) -> Result<Option<StoredMail>> {
    let bare = message_id.trim().trim_start_matches('<').trim_end_matches('>');
    Ok(conn
        .query_row(
            "SELECT m.account_id, m.thread_id, m.date, m.to_addrs, m.cc_addrs, m.subject FROM message m JOIN account a ON a.id = m.account_id
             WHERE m.message_id IN (?1, '<' || ?1 || '>')
             ORDER BY m.from_addr = a.address COLLATE NOCASE DESC, m.id LIMIT 1",
            [bare],
            |r| Ok(StoredMail { account: r.get(0)?, thread: r.get(1)?, date: at(r.get(2)?), to: r.get(3)?, cc: r.get(4)?, subject: r.get(5)? }),
        )
        .optional()?)
}

/// The messages on `account` since `since` that carry a request, with their
/// sender, date and body blob.
pub fn requests_since(conn: &Connection, account: AccountId, since: DateTime<Utc>) -> Result<Vec<(String, DateTime<Utc>, Option<String>)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT m.id, m.from_addr, m.date, m.blob FROM invitation i JOIN message m ON m.id = i.message_id
         WHERE i.method = 'REQUEST' AND m.account_id = ?1 AND m.date >= ?2 ORDER BY m.id",
    )?;
    let rows = stmt.query_map(params![account, since.timestamp()], |r| Ok((r.get::<_, String>(1)?, at(r.get(2)?), r.get::<_, Option<String>>(3)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The senders and dates of the conversation's messages since `since`.
pub fn thread_mail_since(conn: &Connection, thread: crate::ThreadId, since: DateTime<Utc>) -> Result<Vec<(String, DateTime<Utc>)>> {
    let mut stmt = conn.prepare_cached("SELECT from_addr, date FROM message WHERE thread_id = ?1 AND date > ?2 ORDER BY id")?;
    let rows = stmt.query_map(params![thread, since.timestamp()], |r| Ok((r.get::<_, String>(0)?, at(r.get(1)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
