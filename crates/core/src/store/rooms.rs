//! Rooms for meetings (`calendar::rooms`): the answer to the room lists
//! consent, the organization's rooms as Microsoft 365 last listed them, and
//! the stored events that name rooms.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::calendar::rooms::{Access, Room, RoomSource};
use crate::calendar::{EventData, EventId};
use crate::model::{AccountId, AccountKind};
use crate::{Error, Result};

const GRANTED: &str = "granted";

/// What the account answered to the room lists consent. Only Microsoft 365
/// has room lists Jolta reads.
pub fn rooms_access(conn: &Connection, account: AccountId) -> Result<Access> {
    let (kind, answer): (AccountKind, Option<String>) =
        conn.query_row("SELECT kind, rooms FROM account WHERE id = ?1", [account], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(match (kind, answer) {
        (AccountKind::Graph, None) => Access::NotAsked,
        (AccountKind::Graph, Some(a)) if a == GRANTED => Access::Granted,
        (AccountKind::Graph, Some(reason)) => Access::Refused(reason),
        (AccountKind::Gmail | AccountKind::Imap, _) => Access::NotOffered,
    })
}

/// Records the answer. Anything but granted drops the listed rooms, which
/// Jolta may no longer read.
pub fn set_rooms_access(conn: &Connection, account: AccountId, access: &Access) -> Result<()> {
    let answer = match access {
        Access::Granted => Some(GRANTED),
        Access::Refused(reason) => Some(reason.as_str()),
        Access::NotAsked | Access::NotOffered => None,
    };
    conn.execute("UPDATE account SET rooms = ?2 WHERE id = ?1", params![account, answer])?;
    if *access != Access::Granted {
        conn.execute("DELETE FROM room WHERE account_id = ?1", [account])?;
        conn.execute("UPDATE account SET rooms_fetched = NULL WHERE id = ?1", [account])?;
    }
    Ok(())
}

/// Replaces the account's listed rooms with what the room lists said at
/// `fetched`, which is kept even when they listed none.
pub fn save_rooms(conn: &Connection, account: AccountId, rooms: &[Room], fetched: DateTime<Utc>) -> Result<()> {
    conn.execute("DELETE FROM room WHERE account_id = ?1", [account])?;
    conn.execute("UPDATE account SET rooms_fetched = ?2 WHERE id = ?1", params![account, fetched.timestamp()])?;
    let mut insert = conn.prepare_cached(
        "INSERT OR REPLACE INTO room (account_id, address, name, capacity, building, floor, list, accessible, video, display)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for r in rooms {
        insert.execute(params![
            account,
            r.address.to_ascii_lowercase(),
            r.name,
            r.capacity,
            r.building,
            r.floor,
            r.list,
            r.accessible,
            r.video,
            r.display
        ])?;
    }
    Ok(())
}

/// The listed rooms and when they were listed, None when no list is stored.
pub fn listed_rooms(conn: &Connection, account: AccountId) -> Result<(Vec<Room>, Option<DateTime<Utc>>)> {
    let mut stmt = conn.prepare_cached(
        "SELECT address, name, capacity, building, floor, list, accessible, video, display FROM room
         WHERE account_id = ?1 ORDER BY coalesce(list, '') = '', list, name",
    )?;
    let rooms = stmt
        .query_map([account], |r| {
            Ok(Room {
                address: r.get(0)?,
                name: r.get(1)?,
                capacity: r.get(2)?,
                building: r.get(3)?,
                floor: r.get(4)?,
                list: r.get(5)?,
                accessible: r.get(6)?,
                video: r.get(7)?,
                display: r.get(8)?,
                source: RoomSource::Directory,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let fetched: Option<i64> =
        conn.query_row("SELECT rooms_fetched FROM account WHERE id = ?1", [account], |r| r.get(0)).optional()?.flatten();
    Ok((rooms, fetched.and_then(|t| DateTime::from_timestamp(t, 0))))
}

/// The account's own events that name a room and have occurrences
/// starting from `from` to `to`, each with those starts, for the rooms the
/// user booked. All-day ones start at their first day's midnight in UTC,
/// close enough to order by.
pub fn room_uses(conn: &Connection, account: AccountId, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<(EventData, Vec<DateTime<Utc>>)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.data, group_concat(coalesce(o.start_utc, o.start_day * 86400)) FROM occurrence o
           JOIN calendar c ON c.id = o.calendar_id JOIN event e ON e.id = o.event_id
         WHERE c.account_id = ?1 AND coalesce(o.start_utc, o.start_day * 86400) >= ?2 AND coalesce(o.start_utc, o.start_day * 86400) < ?3
           AND (e.data LIKE '%\"role\":\"room\"%' OR e.data LIKE '%\"role\":\"resource\"%')
         GROUP BY e.id",
    )?;
    let rows = stmt.query_map(params![account, from.timestamp(), to.timestamp()], |r| {
        Ok((r.get::<_, EventId>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, data, starts) = row?;
        let event: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))?;
        let starts = starts.split(',').filter_map(|t| t.parse().ok()).filter_map(|t| DateTime::from_timestamp(t, 0)).collect();
        out.push((event, starts));
    }
    Ok(out)
}
