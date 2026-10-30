//! What the user does to a thread, as local changes plus queued ops.
//!
//! Actions apply to the local tables at once (the list updates before the
//! server answers) and queue one op for the account's engine to send. Undo
//! drops an op the engine has not sent yet, or queues its inverse.

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::*;
use crate::store::{self, Op, OpId, OpKind};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Archive,
    Trash,
    MarkRead,
    MarkUnread,
    Flag,
    Unflag,
    MoveTo(MailboxId),
}

/// The list an action is taken from; moves take messages out of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// The inbox, also for lists that are not one mailbox (search, all mail).
    Inbox,
    /// The mailbox with this role in the thread's account (unified "Sent").
    Role(MailboxRole),
    Mailbox(MailboxId),
}

fn thread_account(conn: &Connection, thread: ThreadId) -> Result<AccountId> {
    conn.query_row("SELECT account_id FROM thread WHERE id = ?1", [thread], |r| r.get(0))
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("thread {thread}")))
}

fn role_mailbox(conn: &Connection, account: AccountId, role: MailboxRole) -> Result<Option<MailboxId>> {
    Ok(conn
        .query_row("SELECT id FROM mailbox WHERE account_id = ?1 AND role = ?2", params![account, role], |r| r.get(0))
        .optional()?)
}

fn ids(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<MessageId>> {
    Ok(conn.prepare(sql)?.query_map(p, |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// Applies `action` to `thread` locally and queues the op. Returns the
/// queued op, or `None` when nothing changed.
pub fn perform(conn: &Connection, thread: ThreadId, action: Action, context: Context) -> Result<Option<(AccountId, OpId)>> {
    let account = thread_account(conn, thread)?;
    let in_thread = "SELECT id FROM message WHERE thread_id = ?1";
    let (kind, messages) = match action {
        Action::MarkRead => (
            OpKind::Flags { add: Flags::SEEN, remove: 0 },
            ids(conn, &format!("{in_thread} AND (flags & {}) = 0", Flags::SEEN), [thread])?,
        ),
        Action::MarkUnread => (
            OpKind::Flags { add: 0, remove: Flags::SEEN },
            ids(conn, &format!("{in_thread} ORDER BY id DESC LIMIT 1"), [thread])?,
        ),
        Action::Flag => (
            OpKind::Flags { add: Flags::FLAGGED, remove: 0 },
            ids(conn, &format!("{in_thread} ORDER BY id DESC LIMIT 1"), [thread])?,
        ),
        Action::Unflag => (
            OpKind::Flags { add: 0, remove: Flags::FLAGGED },
            ids(conn, &format!("{in_thread} AND (flags & {}) != 0", Flags::FLAGGED), [thread])?,
        ),
        Action::Archive | Action::Trash | Action::MoveTo(_) => {
            let role = match context {
                Context::Mailbox(m) => Some(m),
                Context::Role(r) => role_mailbox(conn, account, r)?,
                Context::Inbox => None,
            };
            let from = match role {
                Some(m) => m,
                None => role_mailbox(conn, account, MailboxRole::Inbox)?
                    .ok_or_else(|| Error::Other("This account has no inbox.".into()))?,
            };
            let to = match action {
                Action::Archive => role_mailbox(conn, account, MailboxRole::Archive)?
                    .ok_or_else(|| Error::Other("This account has no Archive folder.".into()))?,
                Action::Trash => role_mailbox(conn, account, MailboxRole::Trash)?
                    .ok_or_else(|| Error::Other("This account has no Trash folder.".into()))?,
                Action::MoveTo(m) => m,
                _ => unreachable!("only moves reach here"),
            };
            if from == to {
                return Ok(None);
            }
            let messages = ids(
                conn,
                "SELECT m.id FROM message m JOIN message_mailbox mm ON mm.message_id = m.id
                 WHERE m.thread_id = ?1 AND mm.mailbox_id = ?2",
                params![thread, from],
            )?;
            (OpKind::Move { from, to }, messages)
        }
    };
    queue(conn, account, &kind, &messages)
}

fn queue(conn: &Connection, account: AccountId, kind: &OpKind, messages: &[MessageId]) -> Result<Option<(AccountId, OpId)>> {
    let changed = store::apply_local(conn, kind, messages)?;
    if changed.is_empty() {
        return Ok(None);
    }
    Ok(Some((account, store::insert_op(conn, account, kind, &changed)?)))
}

/// Undoes an op: an unsent one is dropped and the local change reverted; a
/// sent one gets an inverse op. Returns the op to send, if any.
pub fn undo(conn: &Connection, id: OpId) -> Result<Option<(AccountId, OpId)>> {
    let Some((op, state)) = store::op(conn, id)? else { return Ok(None) };
    match state.as_str() {
        "pending" => {
            store::revert_local(conn, &op)?;
            store::cancel_op(conn, id)?;
            Ok(None)
        }
        "done" => {
            let messages: Vec<MessageId> = op.messages.iter().map(|(m, _)| *m).collect();
            queue(conn, op.account, &op.inverse(), &messages)
        }
        _ => Ok(None),
    }
}

/// Reverts an op the server refused for good.
pub fn give_up(conn: &Connection, op: &Op, error: &str) -> Result<()> {
    store::revert_local(conn, op)?;
    store::fail_attempt(conn, op.id, error, true)
}
