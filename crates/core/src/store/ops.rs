use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::model::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub i64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    Flags { add: u32, remove: u32 },
    Move { from: MailboxId, to: MailboxId },
    /// Deletes everything in the server folder for good (Empty Trash). The
    /// local copies are gone before it runs; it has no inverse.
    Empty { mailbox: MailboxId },
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
    /// The op that undoes this one, if one can.
    pub fn inverse(&self) -> Option<OpKind> {
        match self.kind {
            OpKind::Flags { add, remove } => Some(OpKind::Flags { add: remove, remove: add }),
            OpKind::Move { from, to } => Some(OpKind::Move { from: to, to: from }),
            OpKind::Empty { .. } => None,
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
            OpKind::Empty { mailbox } => {
                let Some(uid) = conn
                    .query_row(
                        "SELECT uid FROM message_mailbox WHERE message_id = ?1 AND mailbox_id = ?2",
                        params![id, mailbox],
                        |r| r.get::<_, Option<u32>>(0),
                    )
                    .optional()?
                else {
                    continue;
                };
                conn.execute("DELETE FROM message_mailbox WHERE message_id = ?1 AND mailbox_id = ?2", params![id, mailbox])?;
                // A message filed elsewhere too (a Gmail label) keeps its row.
                let elsewhere: bool =
                    conn.query_row("SELECT EXISTS (SELECT 1 FROM message_mailbox WHERE message_id = ?1)", [id], |r| r.get(0))?;
                if !elsewhere {
                    super::delete_messages(conn, &[id])?;
                }
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
        OpKind::Empty { mailbox } => ("empty", Some(mailbox), None, 0, 0),
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
        "empty" => OpKind::Empty { mailbox: r.get(4)? },
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

/// The account's ops the server hasn't confirmed: waiting to be sent, or
/// claimed by the worker and on their way.
pub fn pending_ops(conn: &Connection, account: AccountId) -> Result<Vec<Op>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {OP_COLS} FROM op WHERE account_id = ?1 AND state IN ('pending', 'sending') ORDER BY id"
    ))?;
    let rows = stmt.query_map([account], op_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Claims the account's oldest waiting op for sending: from here on Undo
/// can no longer take it back unsent and queues its inverse instead.
pub fn claim_op(conn: &Connection, account: AccountId) -> Result<Option<Op>> {
    Ok(conn
        .query_row(
            &format!(
                "UPDATE op SET state = 'sending'
                 WHERE id = (SELECT id FROM op WHERE account_id = ?1 AND state = 'pending' ORDER BY id LIMIT 1)
                 RETURNING {OP_COLS}"
            ),
            [account],
            op_row,
        )
        .optional()?)
}

/// Puts the account's claimed ops back in the queue, for a worker that
/// starts after one stopped mid-send.
pub fn release_claims(conn: &Connection, account: AccountId) -> Result<()> {
    conn.execute("UPDATE op SET state = 'pending' WHERE account_id = ?1 AND state = 'sending'", [account])?;
    Ok(())
}

/// Messages with an op the server hasn't confirmed yet. Until it does, the
/// server still reports their old flags and labels, so sync leaves them be.
pub fn pending_messages(conn: &Connection, account: AccountId) -> Result<HashSet<MessageId>> {
    Ok(pending_ops(conn, account)?.into_iter().flat_map(|op| op.messages).map(|(id, _)| id).collect())
}

/// UIDs that pending moves took out of `mailbox`. The server still lists them
/// there until the move lands; sync must not link them back in.
pub fn pending_move_uids(conn: &Connection, account: AccountId, mailbox: MailboxId) -> Result<HashSet<u32>> {
    Ok(pending_ops(conn, account)?
        .into_iter()
        .filter(|op| matches!(op.kind, OpKind::Move { from, .. } if from == mailbox))
        .flat_map(|op| op.messages)
        .filter_map(|(_, uid)| uid)
        .collect())
}

pub fn has_pending_ops(conn: &Connection, account: AccountId) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM op WHERE account_id = ?1 AND state IN ('pending', 'sending'))",
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
        "UPDATE op SET attempts = attempts + 1, error = ?2, state = CASE WHEN ?3 THEN 'failed' ELSE 'pending' END
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
        OpKind::Flags { add, remove } => {
            apply_local(conn, &OpKind::Flags { add: remove, remove: add }, &ids)?;
        }
        OpKind::Empty { mailbox } => {
            // The local copies are gone; a full sync of the folder brings back what the server kept.
            conn.execute("UPDATE mailbox SET uidvalidity = NULL, delta_link = NULL WHERE id = ?1", [mailbox])?;
        }
        OpKind::Move { from, to } => {
            // Gmail's All Mail holds every message; a move there never took a link away.
            let to_all: bool = conn.query_row("SELECT coalesce(role = 'all', 0) FROM mailbox WHERE id = ?1", [to], |r| r.get(0)).unwrap_or(false);
            for (id, uid) in &op.messages {
                if !to_all {
                    conn.execute("DELETE FROM message_mailbox WHERE message_id = ?1 AND mailbox_id = ?2", params![id, to])?;
                }
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
