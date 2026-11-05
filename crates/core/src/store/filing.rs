//! File responses automatically: the setting, the replies that arrived
//! since it was turned on, and the ones already settled.

use rusqlite::{Connection, OptionalExtension, params};

use super::date_to_id_floor;
use crate::Result;
use crate::model::*;

/// When File responses automatically was turned on, in unix seconds; None
/// while it is off.
pub fn file_responses_since(conn: &Connection) -> Result<Option<i64>> {
    let since: Option<String> = conn.query_row("SELECT value FROM setting WHERE key = 'file_responses'", [], |r| r.get(0)).optional()?;
    Ok(since.and_then(|s| s.parse().ok()))
}

/// Turns File responses automatically on from `now`, or off. Turning it on
/// while it is on keeps the time it was first turned on. Mail already
/// there that the time alone would let through, dated `now` or later, is
/// settled at once, so turning it on never files it.
pub fn set_file_responses(conn: &Connection, on: bool, now: i64) -> Result<()> {
    if !on {
        conn.execute("DELETE FROM setting WHERE key = 'file_responses'", [])?;
    } else if conn.execute("INSERT OR IGNORE INTO setting (key, value) VALUES ('file_responses', ?1)", [now.to_string()])? == 1 {
        conn.execute(
            "INSERT OR IGNORE INTO settled_reply (account_id, message_id)
             SELECT account_id, message_id FROM message WHERE date >= ?1 AND coalesce(message_id, '') != ''",
            [now],
        )?;
    }
    Ok(())
}

/// A reply in an account's inbox that File responses automatically hasn't
/// settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsettledReply {
    pub id: MessageId,
    /// Its Message-ID header, which settles it.
    pub message_id: String,
    pub from_addr: String,
    pub from_name: String,
    pub date: i64,
    pub blob: String,
}

/// The account's inbox mail carrying an iTIP REPLY that arrived at `since`
/// or later, after the account was added, with a body and a Message-ID,
/// and not settled.
pub fn unsettled_replies(conn: &Connection, account: AccountId, since: i64) -> Result<Vec<UnsettledReply>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.message_id, m.from_addr, m.from_name, m.date, m.blob FROM message m
         JOIN account a ON a.id = m.account_id
         WHERE m.account_id = ?1 AND m.id >= ?2 AND m.date >= ?3 AND m.date >= a.created
           AND coalesce(m.message_id, '') != '' AND coalesce(m.blob, '') != '' AND m.blob NOT LIKE 'synth:%'
           AND EXISTS (SELECT 1 FROM invitation i WHERE i.message_id = m.id AND i.method = 'REPLY')
           AND EXISTS (SELECT 1 FROM message_mailbox mm JOIN mailbox b ON b.id = mm.mailbox_id WHERE mm.message_id = m.id AND b.role = 'inbox')
           AND NOT EXISTS (SELECT 1 FROM settled_reply s WHERE s.account_id = m.account_id AND s.message_id = m.message_id)
         ORDER BY m.id",
    )?;
    let rows = stmt.query_map(params![account, date_to_id_floor(since), since], |r| {
        Ok(UnsettledReply { id: r.get(0)?, message_id: r.get(1)?, from_addr: r.get(2)?, from_name: r.get(3)?, date: r.get(4)?, blob: r.get(5)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Settles a reply by its Message-ID. False when it was settled already.
pub fn settle_reply(conn: &Connection, account: AccountId, message_id: &str) -> Result<bool> {
    Ok(conn.execute("INSERT OR IGNORE INTO settled_reply (account_id, message_id) VALUES (?1, ?2)", params![account, message_id])? == 1)
}
