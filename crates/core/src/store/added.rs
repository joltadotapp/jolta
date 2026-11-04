//! Copies of invitations the user put in a calendar with Add to calendar,
//! for an account whose own calendars hold none.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::StoredEvent;
use crate::calendar::{CalendarId, CalendarSource, EventData, EventId, Moment};
use crate::model::AccountId;
use crate::{Error, Result};

/// One version of the invitation written into an added copy: the series or
/// single event (no recurrence id), or one occurrence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrittenVersion {
    pub recurrence_id: Option<Moment>,
    pub sequence: i64,
    pub stamp: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddedCopy {
    pub calendar_id: CalendarId,
    /// The object Jolta created: Google's event id, Graph's, or the CalDAV
    /// resource's path.
    pub remote_id: String,
    /// The address of the meeting's organizer the copy was added from.
    pub organizer: String,
    pub written: Vec<WrittenVersion>,
}

/// The copy the user added of the invitation `uid` that came to `account`.
pub fn added_copy(conn: &Connection, account: AccountId, uid: &str) -> Result<Option<AddedCopy>> {
    let row: Option<(CalendarId, String, String, String)> = conn
        .query_row(
            "SELECT calendar_id, remote_id, organizer, written FROM added_copy WHERE account_id = ?1 AND uid = ?2",
            params![account, uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((calendar_id, remote_id, organizer, written)) = row else { return Ok(None) };
    let written = serde_json::from_str(&written).map_err(|e| Error::Other(format!("added copy of {uid}: {e}")))?;
    Ok(Some(AddedCopy { calendar_id, remote_id, organizer, written }))
}

pub fn remember_added(conn: &Connection, account: AccountId, uid: &str, copy: &AddedCopy) -> Result<()> {
    conn.execute(
        "INSERT INTO added_copy (account_id, uid, calendar_id, remote_id, organizer, written) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (account_id, uid) DO UPDATE SET calendar_id = excluded.calendar_id, remote_id = excluded.remote_id, organizer = excluded.organizer, written = excluded.written",
        params![account, uid, copy.calendar_id, copy.remote_id, copy.organizer, serde_json::to_string(&copy.written).expect("versions serialize")],
    )?;
    Ok(())
}

pub fn forget_added(conn: &Connection, account: AccountId, uid: &str) -> Result<()> {
    conn.execute("DELETE FROM added_copy WHERE account_id = ?1 AND uid = ?2", params![account, uid])?;
    Ok(())
}

/// The event an added copy holds in its calendar, while that calendar is
/// shown: its series or single event, or with `recurrence_id` that changed
/// occurrence, by the moment it names. The copy's UID is the one the
/// object Jolta created has, which every provider keeps as the invitation's.
pub fn added_event(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>, here: Tz) -> Result<Option<StoredEvent>> {
    let Some(copy) = added_copy(conn, account, uid)? else { return Ok(None) };
    let own_uid: Option<String> = conn
        .query_row("SELECT uid FROM event WHERE calendar_id = ?1 AND remote_id = ?2 LIMIT 1", params![copy.calendar_id, copy.remote_id], |r| r.get(0))
        .optional()?;
    let own_uid = own_uid.unwrap_or_else(|| uid.to_string());
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, c.source, e.data FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.calendar_id = ?1 AND e.uid = ?2 AND (e.recurrence_id = '') = ?3 AND c.shown",
    )?;
    let rows = stmt.query_map(params![copy.calendar_id, own_uid, recurrence_id.is_none()], |r| {
        Ok((r.get::<_, EventId>(0)?, r.get::<_, CalendarSource>(1)?, r.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (event_id, source, data) = row?;
        let event: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {event_id}: {e}")))?;
        if recurrence_id.is_none_or(|rid| event.recurrence_id.is_some_and(|stored| rid.names(&stored, here))) {
            return Ok(Some(StoredEvent { event_id, calendar_id: copy.calendar_id, source, event }));
        }
    }
    Ok(None)
}

/// The copy of an invitation to `account` that is the user's: the one in
/// the account's own calendars, else the one the user added, which may be
/// in those calendars too.
pub fn own_or_added(conn: &Connection, uid: &str, recurrence_id: Option<&Moment>, account: AccountId, here: Tz) -> Result<Option<StoredEvent>> {
    match super::stored_event(conn, uid, recurrence_id, Some(account), here)? {
        Some(own) => Ok(Some(own)),
        None => added_event(conn, account, uid, recurrence_id, here),
    }
}

/// Whether the event `uid` in `calendar` is a copy the user added for
/// another account, which is answered from its invitation's mail.
pub fn is_added_copy(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM added_copy WHERE calendar_id = ?1 AND uid = ?2", params![calendar, uid], |_| Ok(())).optional()?.is_some())
}

/// The calendar the user last added an invitation to.
pub fn last_added_calendar(conn: &Connection) -> Result<Option<CalendarId>> {
    let id: Option<String> = conn.query_row("SELECT value FROM setting WHERE key = 'add_to_calendar'", [], |r| r.get(0)).optional()?;
    Ok(id.and_then(|id| id.parse::<i64>().ok()).map(CalendarId))
}

pub fn set_last_added_calendar(conn: &Connection, calendar: CalendarId) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('add_to_calendar', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [calendar.0.to_string()],
    )?;
    Ok(())
}
