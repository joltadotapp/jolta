use rusqlite::{Connection, params};

use crate::Result;
use crate::model::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub id: i64,
    pub name: String,
    pub query: String,
}

pub fn sections(conn: &Connection) -> Result<Vec<Section>> {
    let mut stmt = conn.prepare("SELECT id, name, query FROM section ORDER BY position, id")?;
    let rows = stmt.query_map([], |r| Ok(Section { id: r.get(0)?, name: r.get(1)?, query: r.get(2)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Replaces all sections with `list`, in that order.
pub fn set_sections(conn: &Connection, list: &[(String, String)]) -> Result<()> {
    conn.execute("DELETE FROM section", [])?;
    for (i, (name, query)) in list.iter().enumerate() {
        conn.execute("INSERT INTO section (name, position, query) VALUES (?1, ?2, ?3)", params![name, i as i64 + 1, query])?;
    }
    Ok(())
}

pub fn set_vip(conn: &Connection, address: &str, vip: bool) -> Result<()> {
    if vip {
        conn.execute("INSERT OR IGNORE INTO vip (address) VALUES (?1)", [address.trim()])?;
    } else {
        conn.execute("DELETE FROM vip WHERE address = ?1", [address.trim()])?;
    }
    Ok(())
}

/// Remote images from a trusted sender load without asking.
pub fn set_sender_trusted(conn: &Connection, address: &str, trusted: bool) -> Result<()> {
    if trusted {
        conn.execute("INSERT OR IGNORE INTO trusted_sender (address) VALUES (?1)", [address.trim()])?;
    } else {
        conn.execute("DELETE FROM trusted_sender WHERE address = ?1", [address.trim()])?;
    }
    Ok(())
}

pub fn is_sender_trusted(conn: &Connection, address: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM trusted_sender WHERE address = ?1)", [address.trim()], |r| r.get(0))?)
}

pub fn is_vip(conn: &Connection, address: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM vip WHERE address = ?1)", [address.trim()], |r| r.get(0))?)
}

/// Every VIP address, A to Z.
pub fn vips(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT address FROM vip ORDER BY address")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Clone, Debug)]
pub struct Snoozed {
    pub thread: ThreadId,
    pub account: AccountId,
    pub wake_at: i64,
    pub subject: String,
}

/// Sleeping threads, soonest first.
pub fn snoozed(conn: &Connection) -> Result<Vec<Snoozed>> {
    let mut stmt = conn.prepare(
        "SELECT s.thread_id, s.account_id, s.wake_at,
                (SELECT subject FROM message WHERE thread_id = s.thread_id ORDER BY id DESC LIMIT 1)
         FROM snooze s WHERE s.state = 'sleeping' ORDER BY s.wake_at",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Snoozed { thread: r.get(0)?, account: r.get(1)?, wake_at: r.get(2)?, subject: r.get::<_, Option<String>>(3)?.unwrap_or_default() })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn due_snoozes(conn: &Connection, account: AccountId, now: i64) -> Result<Vec<(i64, ThreadId)>> {
    let mut stmt =
        conn.prepare("SELECT id, thread_id FROM snooze WHERE account_id = ?1 AND state = 'sleeping' AND wake_at <= ?2")?;
    let rows = stmt.query_map(params![account, now], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The next snooze wake-up or "remind me" check for this account. Overdue
/// reminders whose message hasn't synced yet are retried with each sync,
/// not counted here, so they can't make the engine spin.
pub fn next_wake(conn: &Connection, account: AccountId) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT min(t) FROM (
             SELECT min(wake_at) AS t FROM snooze WHERE account_id = ?1 AND state = 'sleeping'
             UNION ALL
             SELECT min(remind_at) FROM reminder WHERE account_id = ?1 AND fired = 0 AND remind_at > strftime('%s', 'now'))",
        [account],
        |r| r.get(0),
    )?)
}

pub fn snooze_woken(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE snooze SET state = 'woken' WHERE id = ?1", [id])?;
    Ok(())
}
