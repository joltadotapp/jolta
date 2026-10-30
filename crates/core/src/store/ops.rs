use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::model::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub i64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    Flags { add: u32, remove: u32 },
    Move { from: MailboxId, to: MailboxId },
}

#[derive(Clone, Debug)]
pub struct Op {
    pub id: OpId,
    pub account: AccountId,
    pub kind: OpKind,
    /// Each message with its server UID in the mailbox the op reads from,
    /// when known.
    pub messages: Vec<(MessageId, Option<u32>)>,
    pub attempts: u32,
}

impl Op {
    /// The op that undoes this one.
    pub fn inverse(&self) -> OpKind {
        match self.kind {
            OpKind::Flags { add, remove } => OpKind::Flags { add: remove, remove: add },
            OpKind::Move { from, to } => OpKind::Move { from: to, to: from },
        }
    }
}

fn encode(messages: &[(MessageId, Option<u32>)]) -> String {
    messages
        .iter()
        .map(|(id, uid)| format!("{}:{}", id.0, uid.map(|u| u.to_string()).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(",")
}

fn decode(s: &str) -> Vec<(MessageId, Option<u32>)> {
    s.split(',')
        .filter_map(|p| {
            let (id, uid) = p.split_once(':')?;
            Some((MessageId(id.parse().ok()?), uid.parse().ok()))
        })
        .collect()
}

fn uid_in(conn: &Connection, id: MessageId, mailbox: MailboxId) -> Result<Option<u32>> {
    Ok(conn
        .query_row(
            "SELECT uid FROM message_mailbox WHERE message_id = ?1 AND mailbox_id = ?2",
            params![id, mailbox],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// Applies the change to the local tables. Returns the messages it actually
/// changed with their UIDs before the change, so undo restores exactly them.
pub fn apply_local(conn: &Connection, kind: &OpKind, messages: &[MessageId]) -> Result<Vec<(MessageId, Option<u32>)>> {
    let mut changed = Vec::new();
    for &id in messages {
        match *kind {
            OpKind::Flags { add, remove } => {
                let Some(flags) =
                    conn.query_row("SELECT flags FROM message WHERE id = ?1", [id], |r| r.get::<_, u32>(0)).optional()?
                else {
                    continue;
                };
                let next = (flags | add) & !remove;
                if next != flags {
                    conn.execute("UPDATE message SET flags = ?2 WHERE id = ?1", params![id, next])?;
                    let uid = conn
                        .query_row(
                            "SELECT uid FROM message_mailbox WHERE message_id = ?1 AND uid IS NOT NULL LIMIT 1",
                            [id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    changed.push((id, uid));
                }
            }
            OpKind::Move { from, to } => {
                let linked: bool = conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM message_mailbox WHERE message_id = ?1 AND mailbox_id = ?2)",
                    params![id, from],
                    |r| r.get(0),
                )?;
                if !linked {
                    continue;
                }
                let uid = uid_in(conn, id, from)?;
                conn.execute("DELETE FROM message_mailbox WHERE message_id = ?1 AND mailbox_id = ?2", params![id, from])?;
                conn.execute(
                    "INSERT INTO message_mailbox (mailbox_id, message_id, uid) VALUES (?1, ?2, NULL)
                     ON CONFLICT DO NOTHING",
                    params![to, id],
                )?;
                changed.push((id, uid));
            }
        }
    }
    Ok(changed)
}

pub fn insert_op(conn: &Connection, account: AccountId, kind: &OpKind, messages: &[(MessageId, Option<u32>)]) -> Result<OpId> {
    let (name, from, to, add, remove) = match *kind {
        OpKind::Flags { add, remove } => ("flags", None, None, add, remove),
        OpKind::Move { from, to } => ("move", Some(from), Some(to), 0, 0),
    };
    conn.execute(
        "INSERT INTO op (account_id, kind, messages, from_mailbox, to_mailbox, add_flags, remove_flags, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch())",
        params![account, name, encode(messages), from, to, add, remove],
    )?;
    Ok(OpId(conn.last_insert_rowid()))
}

fn op_row(r: &rusqlite::Row) -> rusqlite::Result<Op> {
    let kind: String = r.get(2)?;
    let kind = match kind.as_str() {
        "move" => OpKind::Move { from: r.get(4)?, to: r.get(5)? },
        _ => OpKind::Flags { add: r.get(6)?, remove: r.get(7)? },
    };
    Ok(Op { id: OpId(r.get(0)?), account: r.get(1)?, kind, messages: decode(&r.get::<_, String>(3)?), attempts: r.get(8)? })
}

const OP_COLS: &str = "id, account_id, kind, messages, from_mailbox, to_mailbox, add_flags, remove_flags, attempts";

pub fn op(conn: &Connection, id: OpId) -> Result<Option<(Op, String)>> {
    Ok(conn
        .query_row(&format!("SELECT {OP_COLS}, state FROM op WHERE id = ?1"), [id.0], |r| {
            Ok((op_row(r)?, r.get::<_, String>(9)?))
        })
        .optional()?)
}

pub fn pending_ops(conn: &Connection, account: AccountId) -> Result<Vec<Op>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {OP_COLS} FROM op WHERE account_id = ?1 AND state = 'pending' ORDER BY id"))?;
    let rows = stmt.query_map([account], op_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn has_pending_ops(conn: &Connection, account: AccountId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM op WHERE account_id = ?1 AND state = 'pending')",
        [account],
        |r| r.get(0),
    )?)
}

pub fn finish_op(conn: &Connection, id: OpId) -> Result<()> {
    conn.execute("UPDATE op SET state = 'done' WHERE id = ?1", [id.0])?;
    Ok(())
}

pub fn cancel_op(conn: &Connection, id: OpId) -> Result<()> {
    conn.execute("DELETE FROM op WHERE id = ?1", [id.0])?;
    Ok(())
}

pub fn fail_attempt(conn: &Connection, id: OpId, error: &str, give_up: bool) -> Result<()> {
    conn.execute(
        "UPDATE op SET attempts = attempts + 1, error = ?2, state = CASE WHEN ?3 THEN 'failed' ELSE state END
         WHERE id = ?1",
        params![id.0, error, give_up],
    )?;
    Ok(())
}

/// Puts the local tables back as they were before `op`, for an op the
/// server refused for good or one undone before it was sent.
pub fn revert_local(conn: &Connection, op: &Op) -> Result<()> {
    let ids: Vec<MessageId> = op.messages.iter().map(|(id, _)| *id).collect();
    match op.kind {
        OpKind::Flags { .. } => {
            apply_local(conn, &op.inverse(), &ids)?;
        }
        OpKind::Move { from, to } => {
            for (id, uid) in &op.messages {
                conn.execute("DELETE FROM message_mailbox WHERE message_id = ?1 AND mailbox_id = ?2", params![id, to])?;
                conn.execute(
                    "INSERT INTO message_mailbox (mailbox_id, message_id, uid) VALUES (?1, ?2, ?3)
                     ON CONFLICT DO UPDATE SET uid = excluded.uid",
                    params![from, id, uid],
                )?;
            }
        }
    }
    Ok(())
}
