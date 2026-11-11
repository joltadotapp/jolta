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
    /// Adds a Gmail label to the thread's messages, which stay where they are.
    AddLabel(MailboxId),
    /// Takes a Gmail label off the thread's messages.
    RemoveLabel(MailboxId),
    /// Move to the Snoozed mailbox until this unix time, then back.
    Snooze(i64),
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
        Action::Archive | Action::Trash | Action::MoveTo(_) | Action::Snooze(_) => {
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
                Action::Archive => archive_mailbox(conn, account)?,
                Action::Trash => role_mailbox(conn, account, MailboxRole::Trash)?
                    .ok_or_else(|| Error::Other("This account has no Trash folder.".into()))?,
                Action::MoveTo(m) => m,
                Action::Snooze(_) => snoozed_mailbox(conn, account)?,
                _ => unreachable!("only moves reach here"),
            };
            for mailbox in [from, to] {
                let own: bool = conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM mailbox WHERE id = ?1 AND account_id = ?2)",
                    params![mailbox, account],
                    |r| r.get(0),
                )?;
                if !own {
                    return Err(Error::Other(format!("Mailbox {} isn't a mailbox of this account.", mailbox.0)));
                }
            }
            if from == to {
                return Ok(None);
            }
            let messages = ids(
                conn,
                "SELECT m.id FROM message m JOIN message_mailbox mm ON mm.message_id = m.id
                 WHERE m.thread_id = ?1 AND mm.mailbox_id = ?2",
                params![thread, from],
            )?;
            if let Action::Snooze(wake_at) = action {
                if messages.is_empty() {
                    return Ok(None);
                }
                conn.execute("DELETE FROM snooze WHERE thread_id = ?1 AND state = 'sleeping'", [thread])?;
                conn.execute(
                    "INSERT INTO snooze (account_id, thread_id, wake_at) VALUES (?1, ?2, ?3)",
                    params![account, thread, wake_at],
                )?;
            }
            (OpKind::Move { from, to }, messages)
        }
        // A Gmail label is a mailbox like any other, and every message
        // outside Trash and Spam is also in All Mail. Adding a label is a
        // move from All Mail that keeps the All Mail link, taking it off is
        // a move back, like archiving takes the Inbox label off.
        Action::AddLabel(label) | Action::RemoveLabel(label) => {
            check_label(conn, account, label)?;
            let all = role_mailbox(conn, account, MailboxRole::All)?
                .ok_or_else(|| Error::Other("This account has no All Mail folder.".into()))?;
            let (from, to) = if matches!(action, Action::AddLabel(_)) { (all, label) } else { (label, all) };
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

pub const NO_LABELS: &str = "Labels are a Gmail feature. Use Move (v) instead.";

/// A label the user can put on a conversation, and whether it has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub mailbox: MailboxId,
    pub name: String,
    pub applied: bool,
}

/// The account's own labels, the mailboxes without a role, by name. A
/// thread has a label when any of its messages has it, as in Gmail.
pub fn labels(conn: &Connection, thread: ThreadId) -> Result<Vec<Label>> {
    let account = thread_account(conn, thread)?;
    require_labels(conn, account)?;
    let mut stmt = conn.prepare(
        "SELECT b.id, b.name, EXISTS (SELECT 1 FROM message_mailbox mm JOIN message m ON m.id = mm.message_id
                                       WHERE mm.mailbox_id = b.id AND m.thread_id = ?2)
         FROM mailbox b WHERE b.account_id = ?1 AND b.role IS NULL AND b.selectable
         ORDER BY b.name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map(params![account, thread], |r| Ok(Label { mailbox: r.get(0)?, name: r.get(1)?, applied: r.get(2)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn require_labels(conn: &Connection, account: AccountId) -> Result<()> {
    if store::account(conn, account)?.kind.has_labels() { Ok(()) } else { Err(Error::Other(NO_LABELS.into())) }
}

fn check_label(conn: &Connection, account: AccountId, label: MailboxId) -> Result<()> {
    require_labels(conn, account)?;
    let is_label: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM mailbox WHERE id = ?1 AND account_id = ?2 AND role IS NULL AND selectable)",
        params![label, account],
        |r| r.get(0),
    )?;
    if !is_label {
        return Err(Error::Other(format!("Mailbox {} isn't a label of this account.", label.0)));
    }
    Ok(())
}

/// Where archiving puts mail. Gmail has no Archive folder: archiving
/// leaves the message in All Mail only.
fn archive_mailbox(conn: &Connection, account: AccountId) -> Result<MailboxId> {
    role_mailbox(conn, account, MailboxRole::Archive)?
        .or(role_mailbox(conn, account, MailboxRole::All)?)
        .ok_or_else(|| Error::Other("This account has no Archive folder.".into()))
}

/// Archives these messages of the account from its inbox, as Archive does
/// for a thread, without the rest of their threads. Returns the queued op,
/// or `None` when none of them is in the inbox.
pub fn archive_messages(conn: &Connection, account: AccountId, messages: &[MessageId]) -> Result<Option<(AccountId, OpId)>> {
    let Some(from) = role_mailbox(conn, account, MailboxRole::Inbox)? else { return Ok(None) };
    let to = archive_mailbox(conn, account)?;
    let mut in_inbox = conn.prepare_cached("SELECT 1 FROM message_mailbox mm JOIN message m ON m.id = mm.message_id WHERE mm.message_id = ?1 AND mm.mailbox_id = ?2 AND m.account_id = ?3")?;
    let mut moving = Vec::new();
    for &m in messages {
        if in_inbox.exists(params![m, from, account])? {
            moving.push(m);
        }
    }
    queue(conn, account, &OpKind::Move { from, to }, &moving)
}

/// The account's Snoozed mailbox. The first snooze adds it locally; the
/// server copy is created when the move reaches the server.
fn snoozed_mailbox(conn: &Connection, account: AccountId) -> Result<MailboxId> {
    if let Some(m) = role_mailbox(conn, account, MailboxRole::Snoozed)? {
        return Ok(m);
    }
    conn.execute(
        "INSERT INTO mailbox (account_id, remote_id, name, role, delimiter, selectable)
         VALUES (?1, 'Snoozed', 'Snoozed', 'snoozed', NULL, 1)
         ON CONFLICT (account_id, remote_id) DO UPDATE SET role = 'snoozed'",
        [account],
    )?;
    Ok(MailboxId(conn.query_row(
        "SELECT id FROM mailbox WHERE account_id = ?1 AND remote_id = 'Snoozed'",
        [account],
        |r| r.get(0),
    )?))
}

fn queue(conn: &Connection, account: AccountId, kind: &OpKind, messages: &[MessageId]) -> Result<Option<(AccountId, OpId)>> {
    let changed = store::apply_local(conn, kind, messages)?;
    if changed.is_empty() {
        return Ok(None);
    }
    Ok(Some((account, store::insert_op(conn, account, kind, &changed)?)))
}

/// Undoes an op: one the worker never claimed is dropped and the local
/// change reverted; one it claimed, which may have reached the server even
/// when it failed and waits again, or a sent one gets an inverse op that
/// runs after it. Returns the op to send, if any.
pub fn undo(conn: &Connection, id: OpId) -> Result<Option<(AccountId, OpId)>> {
    let Some((op, state)) = store::op(conn, id)? else { return Ok(None) };
    match state.as_str() {
        "pending" if !op.tried => {
            store::revert_local(conn, &op)?;
            store::cancel_op(conn, id)?;
            forget_snooze(conn, &op)?;
            Ok(None)
        }
        "pending" | "sending" | "done" => {
            let Some(inverse) = op.inverse() else { return Ok(None) };
            let messages: Vec<MessageId> = op.messages.iter().map(|(m, _)| *m).collect();
            forget_snooze(conn, &op)?;
            queue(conn, op.account, &inverse, &messages)
        }
        _ => Ok(None),
    }
}

/// Undoing a snooze also cancels its wake-up.
fn forget_snooze(conn: &Connection, op: &Op) -> Result<()> {
    if let OpKind::Move { to, .. } = op.kind {
        if role_mailbox(conn, op.account, MailboxRole::Snoozed)? == Some(to) {
            for (m, _) in &op.messages {
                conn.execute(
                    "DELETE FROM snooze WHERE state = 'sleeping' AND thread_id = (SELECT thread_id FROM message WHERE id = ?1)",
                    [m],
                )?;
            }
        }
    }
    Ok(())
}

/// Marks these messages read, one op per account.
pub fn mark_read(conn: &Connection, messages: &[MessageId]) -> Result<Vec<(AccountId, OpId)>> {
    let mut by_account: std::collections::BTreeMap<AccountId, Vec<MessageId>> = Default::default();
    let mut account_of = conn.prepare_cached("SELECT account_id FROM message WHERE id = ?1")?;
    for &m in messages {
        by_account.entry(account_of.query_row([m], |r| r.get(0))?).or_default().push(m);
    }
    let kind = OpKind::Flags { add: Flags::SEEN, remove: 0 };
    let mut ops = Vec::new();
    for (account, ids) in by_account {
        ops.extend(queue(conn, account, &kind, &ids)?);
    }
    Ok(ops)
}

/// Empties a folder for good: its messages leave the store now, and the
/// queued op empties the server folder, including mail Jolta never
/// downloaded. Not undoable.
pub fn empty(conn: &Connection, mailbox: MailboxId) -> Result<(AccountId, OpId)> {
    let account: AccountId = conn.query_row("SELECT account_id FROM mailbox WHERE id = ?1", [mailbox], |r| r.get(0))?;
    let messages = ids(conn, "SELECT message_id FROM message_mailbox WHERE mailbox_id = ?1", [mailbox])?;
    let kind = OpKind::Empty { mailbox };
    let changed = store::apply_local(conn, &kind, &messages)?;
    Ok((account, store::insert_op(conn, account, &kind, &changed)?))
}

/// Wakes a snoozed thread: back to the inbox, unread. A thread that has
/// left Snoozed since, say to the Trash, stays where it is and gets `None`.
pub fn wake(conn: &Connection, snooze: i64, thread: ThreadId) -> Result<Option<Vec<(AccountId, OpId)>>> {
    let holds = store::snooze_holds(conn, snooze)?;
    store::snooze_woken(conn, snooze)?;
    if !holds {
        return Ok(None);
    }
    let mut ops = Vec::new();
    let account = thread_account(conn, thread)?;
    if let Some(inbox) = role_mailbox(conn, account, MailboxRole::Inbox)? {
        ops.extend(perform(conn, thread, Action::MoveTo(inbox), Context::Role(MailboxRole::Snoozed))?);
    }
    ops.extend(perform(conn, thread, Action::MarkUnread, Context::Inbox)?);
    Ok(Some(ops))
}

/// Reverts an op the server refused for good.
pub fn give_up(conn: &Connection, op: &Op, error: &str) -> Result<()> {
    store::revert_local(conn, op)?;
    store::fail_attempt(conn, op.id, error, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data folder that is removed when the test ends.
    struct Dir(std::path::PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn store() -> (Dir, crate::store::Store) {
        let path = std::env::temp_dir().join(format!("jolta-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&path).unwrap();
        let dir = Dir(path);
        let store = crate::store::Store::open(&dir.0.join("jolta.sqlite")).unwrap();
        store
            .write(|c| {
                c.execute_batch(
                    "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'imap', 'a@x', 'a@x', 'r', 0);
                     INSERT INTO mailbox (id, account_id, remote_id, name, role) VALUES
                       (1, 1, 'INBOX', 'INBOX', 'inbox'), (2, 1, 'Trash', 'Trash', 'trash'), (3, 1, 'Work', 'Work', NULL);
                     INSERT INTO thread (id, account_id) VALUES (1, 1);
                     INSERT INTO message (id, account_id, remote_id, thread_id, date, flags) VALUES
                       (1, 1, 'r1', 1, 0, 0), (2, 1, 'r2', 1, 0, 0), (3, 1, 'r3', 1, 0, 1);
                     INSERT INTO message_mailbox (mailbox_id, message_id, uid) VALUES
                       (2, 1, 11), (2, 2, 12), (3, 2, 7), (1, 3, 5);",
                )?;
                Ok(())
            })
            .unwrap();
        (dir, store)
    }

    /// The worker claims an op before sending it, so Undo can no longer
    /// drop it unsent and queues the opposite move instead; an unclaimed op
    /// is still dropped.
    #[test]
    fn undo_of_an_op_on_its_way_queues_its_opposite() {
        let (_dir, store) = store();
        let inbox = |c: &Connection| -> Result<bool> { Ok(c.query_row("SELECT EXISTS (SELECT 1 FROM message_mailbox WHERE message_id = 3 AND mailbox_id = 1)", [], |r| r.get(0))?) };
        store
            .write(|c| {
                let (_, unsent) = queue(c, AccountId(1), &OpKind::Move { from: MailboxId(1), to: MailboxId(3) }, &[MessageId(3)])?.unwrap();
                assert_eq!(undo(c, unsent)?, None);
                assert!(store::op(c, unsent)?.is_none(), "an unclaimed op is dropped");

                let (_, archive) = queue(c, AccountId(1), &OpKind::Move { from: MailboxId(1), to: MailboxId(3) }, &[MessageId(3)])?.unwrap();
                let claimed = store::claim_op(c, AccountId(1))?.expect("claimed");
                assert_eq!(claimed.id, archive);
                assert!(store::claim_op(c, AccountId(1))?.is_none(), "claimed once");
                let (_, back) = undo(c, archive)?.expect("the opposite move");
                assert_eq!(store::op(c, archive)?.map(|(_, state)| state).as_deref(), Some("sending"), "the claimed op stays");
                assert_eq!(store::op(c, back)?.unwrap().0.kind, OpKind::Move { from: MailboxId(3), to: MailboxId(1) });
                assert!(inbox(c)?);
                store::finish_op(c, archive)?;
                assert!(store::pending_messages(c, AccountId(1))?.contains(&MessageId(3)), "sync leaves it in the inbox until the opposite move lands");
                assert_eq!(store::claim_op(c, AccountId(1))?.map(|op| op.id), Some(back));
                store::fail_attempt(c, back, "offline", false)?;
                assert_eq!(store::claim_op(c, AccountId(1))?.map(|op| op.id), Some(back), "a failed attempt goes back in the queue");
                store::release_claims(c, AccountId(1))?;
                assert_eq!(store::claim_op(c, AccountId(1))?.map(|op| op.id), Some(back), "a claim left by a stopped worker");
                Ok(())
            })
            .unwrap();
    }

    /// An op's id is its Undo token. An op dropped unsent never hands that
    /// id to the next op, so a stale Undo, say from an old toast, takes
    /// back nothing instead of the newer action.
    #[test]
    fn a_dropped_op_keeps_its_undo_token_to_itself() {
        let (_dir, store) = store();
        let to_work = OpKind::Move { from: MailboxId(1), to: MailboxId(3) };
        store
            .write(|c| {
                let (_, dropped) = queue(c, AccountId(1), &to_work, &[MessageId(3)])?.unwrap();
                assert_eq!(undo(c, dropped)?, None);
                let (_, newer) = queue(c, AccountId(1), &to_work, &[MessageId(3)])?.unwrap();
                assert_ne!(newer, dropped, "the dropped op's token is not given out again");
                assert_eq!(undo(c, dropped)?, None);
                assert!(store::op(c, newer)?.is_some(), "a stale Undo leaves the newer op queued");
                Ok(())
            })
            .unwrap();
    }

    /// An op that went back in the queue after an attempt, whether the
    /// attempt failed or a worker stopped mid-send, may have reached the
    /// server, so Undo keeps it and queues the opposite move.
    #[test]
    fn undo_of_an_op_tried_before_queues_its_opposite() {
        let (_dir, store) = store();
        let to_work = OpKind::Move { from: MailboxId(1), to: MailboxId(3) };
        store
            .write(|c| {
                for (how, failed) in [("failed", true), ("stopped", false)] {
                    let (_, archive) = queue(c, AccountId(1), &to_work, &[MessageId(3)])?.unwrap();
                    assert_eq!(store::claim_op(c, AccountId(1))?.map(|op| op.id), Some(archive));
                    if failed {
                        store::fail_attempt(c, archive, "the connection dropped before the OK", false)?;
                    } else {
                        store::release_claims(c, AccountId(1))?;
                    }
                    let (_, back) = undo(c, archive)?.unwrap_or_else(|| panic!("the opposite move after an attempt that {how}"));
                    assert_eq!(store::op(c, archive)?.map(|(_, state)| state).as_deref(), Some("pending"), "the op tried before stays after it {how}");
                    assert_eq!(store::claim_op(c, AccountId(1))?.map(|op| op.id), Some(archive), "it is sent again, first");
                    store::finish_op(c, archive)?;
                    assert_eq!(store::claim_op(c, AccountId(1))?.map(|op| op.id), Some(back), "then its opposite");
                    store::finish_op(c, back)?;
                    assert_eq!(store::claim_op(c, AccountId(1))?.map(|op| op.id), None, "and nothing more");
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn empty_deletes_the_folder_but_keeps_messages_filed_elsewhere() {
        let (_dir, store) = store();
        let (_, op) = store.write(|c| empty(c, MailboxId(2))).unwrap();
        store
            .write(|c| {
                let left: Vec<i64> = c.prepare("SELECT id FROM message ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                assert_eq!(left, [2, 3], "message 1 lived only in Trash; message 2 is also in Work");
                let in_trash: i64 = c.query_row("SELECT count(*) FROM message_mailbox WHERE mailbox_id = 2", [], |r| r.get(0))?;
                assert_eq!(in_trash, 0);
                let (queued, _) = store::op(c, op)?.unwrap();
                assert_eq!(queued.kind, OpKind::Empty { mailbox: MailboxId(2) });
                assert_eq!(undo(c, op)?, None, "emptying has no undo");
                Ok(())
            })
            .unwrap();
    }

    /// A Gmail account with the labels Work and Avocats beside Gmail's own
    /// mailboxes. Thread 2 has two messages in the inbox; message 4 already
    /// has Work.
    fn gmail(c: &Connection) -> Result<()> {
        c.execute_batch(
            "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (2, 'gmail', 'g@x', 'g@x', 'g', 0);
             INSERT INTO mailbox (id, account_id, remote_id, name, role, selectable) VALUES
               (10, 2, 'INBOX', 'INBOX', 'inbox', 1), (11, 2, '[Gmail]/All Mail', 'All Mail', 'all', 1),
               (12, 2, 'Work', 'Work', NULL, 1), (13, 2, '[Gmail]/Trash', 'Trash', 'trash', 1),
               (14, 2, '[Gmail]/Starred', 'Starred', 'system', 1), (15, 2, 'Snoozed', 'Snoozed', 'snoozed', 1),
               (16, 2, 'Avocats', 'Avocats', NULL, 1);
             INSERT INTO thread (id, account_id) VALUES (2, 2);
             INSERT INTO message (id, account_id, remote_id, thread_id, date, flags) VALUES
               (4, 2, 'gm:4', 2, 0, 0), (5, 2, 'gm:5', 2, 0, 0);
             INSERT INTO message_mailbox (mailbox_id, message_id, uid) VALUES
               (11, 4, 104), (11, 5, 105), (10, 4, NULL), (10, 5, NULL), (12, 4, NULL);",
        )?;
        Ok(())
    }

    fn links(c: &Connection, mailbox: i64) -> Result<Vec<(i64, Option<u32>)>> {
        Ok(c.prepare("SELECT message_id, uid FROM message_mailbox WHERE mailbox_id = ?1 ORDER BY message_id")?
            .query_map([mailbox], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Labelling a conversation in Gmail keeps it in the inbox and in All
    /// Mail, and sends a move from All Mail, which Gmail turns into
    /// +X-GM-LABELS for the messages' All Mail UIDs.
    #[test]
    fn adding_a_label_keeps_the_conversation_where_it_is() {
        let (_dir, store) = store();
        store
            .write(|c| {
                gmail(c)?;
                let inbox_before = links(c, 10)?;
                let (account, op) = perform(c, ThreadId(2), Action::AddLabel(MailboxId(16)), Context::Inbox)?.expect("queued");
                assert_eq!(account, AccountId(2));
                assert_eq!(links(c, 16)?, [(4, None), (5, None)], "both messages have the label");
                assert_eq!(links(c, 10)?, inbox_before, "still in the inbox");
                assert_eq!(links(c, 11)?, [(4, Some(104)), (5, Some(105))], "still in All Mail with their UIDs");
                let (queued, _) = store::op(c, op)?.unwrap();
                assert_eq!(queued.kind, OpKind::Move { from: MailboxId(11), to: MailboxId(16) });
                assert_eq!(queued.messages, [(MessageId(4), Some(104)), (MessageId(5), Some(105))], "sent by All Mail UID");

                let (_, work) = perform(c, ThreadId(2), Action::AddLabel(MailboxId(12)), Context::Inbox)?.expect("queued");
                assert_eq!(store::op(c, work)?.unwrap().0.messages, [(MessageId(5), Some(105))], "message 4 already had Work");
                assert_eq!(links(c, 12)?, [(4, None), (5, None)]);
                assert_eq!(perform(c, ThreadId(2), Action::AddLabel(MailboxId(12)), Context::Inbox)?, None, "nothing left to label");
                Ok(())
            })
            .unwrap();
    }

    /// Only Gmail's All Mail keeps a message moved out of it; an IMAP
    /// server's \All folder is left to the server, as before.
    #[test]
    fn a_move_out_of_an_imap_all_folder_still_moves() {
        let (_dir, store) = store();
        store
            .write(|c| {
                c.execute_batch("INSERT INTO mailbox (id, account_id, remote_id, name, role) VALUES (4, 1, 'All', 'All', 'all');
                                 INSERT INTO message_mailbox (mailbox_id, message_id, uid) VALUES (4, 3, 9);")?;
                queue(c, AccountId(1), &OpKind::Move { from: MailboxId(4), to: MailboxId(3) }, &[MessageId(3)])?.expect("queued");
                assert_eq!(links(c, 4)?, [], "the IMAP move took it out of All");
                assert_eq!(links(c, 3)?, [(2, Some(7)), (3, None)]);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn undo_takes_an_added_label_off_again() {
        let (_dir, store) = store();
        store
            .write(|c| {
                gmail(c)?;
                let before = (links(c, 10)?, links(c, 11)?, links(c, 12)?);
                let (_, op) = perform(c, ThreadId(2), Action::AddLabel(MailboxId(12)), Context::Inbox)?.unwrap();
                assert_eq!(undo(c, op)?, None, "unsent, so dropped");
                assert_eq!((links(c, 10)?, links(c, 11)?, links(c, 12)?), before, "message 4 keeps the Work it had");

                let (_, op) = perform(c, ThreadId(2), Action::AddLabel(MailboxId(12)), Context::Inbox)?.unwrap();
                assert_eq!(store::claim_op(c, AccountId(2))?.map(|o| o.id), Some(op));
                let (_, back) = undo(c, op)?.expect("an op on its way gets its opposite");
                let (inverse, _) = store::op(c, back)?.unwrap();
                assert_eq!(inverse.kind, OpKind::Move { from: MailboxId(12), to: MailboxId(11) }, "Gmail sends -X-GM-LABELS");
                assert_eq!(inverse.messages.iter().map(|(m, _)| m.0).collect::<Vec<_>>(), [5], "only the message it labelled");
                assert_eq!((links(c, 10)?, links(c, 11)?, links(c, 12)?), before);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn removing_a_label_keeps_the_conversation_in_the_inbox() {
        let (_dir, store) = store();
        store
            .write(|c| {
                gmail(c)?;
                let before = (links(c, 10)?, links(c, 11)?, links(c, 12)?);
                let (_, op) = perform(c, ThreadId(2), Action::RemoveLabel(MailboxId(12)), Context::Inbox)?.expect("queued");
                assert_eq!(links(c, 12)?, [], "the label is off");
                assert_eq!(links(c, 10)?, before.0, "still in the inbox");
                assert_eq!(links(c, 11)?, before.1, "still in All Mail");
                let (queued, _) = store::op(c, op)?.unwrap();
                assert_eq!(queued.kind, OpKind::Move { from: MailboxId(12), to: MailboxId(11) });
                assert_eq!(queued.messages.iter().map(|(m, _)| m.0).collect::<Vec<_>>(), [4]);
                assert_eq!(perform(c, ThreadId(2), Action::RemoveLabel(MailboxId(12)), Context::Inbox)?, None);

                assert_eq!(undo(c, op)?, None);
                assert_eq!((links(c, 10)?, links(c, 11)?, links(c, 12)?), before, "undo puts it back");

                let (_, op) = perform(c, ThreadId(2), Action::RemoveLabel(MailboxId(12)), Context::Inbox)?.unwrap();
                store::claim_op(c, AccountId(2))?;
                store::finish_op(c, op)?;
                let (_, back) = undo(c, op)?.expect("a sent op gets its opposite");
                assert_eq!(store::op(c, back)?.unwrap().0.kind, OpKind::Move { from: MailboxId(11), to: MailboxId(12) }, "Gmail sends +X-GM-LABELS");
                assert_eq!((links(c, 10)?, links(c, 11)?, links(c, 12)?), before);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn labels_are_the_accounts_own_and_only_on_gmail() {
        let (_dir, store) = store();
        store
            .write(|c| {
                gmail(c)?;
                let names = labels(c, ThreadId(2))?.into_iter().map(|l| (l.name, l.applied)).collect::<Vec<_>>();
                assert_eq!(names, [("Avocats".to_string(), false), ("Work".to_string(), true)], "no Inbox, All Mail, Trash, Starred or Snoozed");
                for action in [Action::AddLabel(MailboxId(10)), Action::AddLabel(MailboxId(14)), Action::RemoveLabel(MailboxId(11)), Action::AddLabel(MailboxId(3))] {
                    assert!(perform(c, ThreadId(2), action, Context::Inbox).is_err(), "{action:?} is no label of this account");
                }
                let refused = |r: Result<_>| matches!(r, Err(Error::Other(m)) if m == NO_LABELS);
                assert!(refused(labels(c, ThreadId(1)).map(|_| ())), "an IMAP account has no labels");
                assert!(refused(perform(c, ThreadId(1), Action::AddLabel(MailboxId(3)), Context::Inbox).map(|_| ())));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn a_move_names_only_mailboxes_of_the_threads_own_account() {
        let (_dir, store) = store();
        store
            .write(|c| {
                gmail(c)?;
                for (action, context) in [
                    (Action::MoveTo(MailboxId(12)), Context::Inbox),
                    (Action::Trash, Context::Mailbox(MailboxId(10))),
                    (Action::MoveTo(MailboxId(99)), Context::Inbox),
                ] {
                    assert!(perform(c, ThreadId(1), action, context).is_err(), "{action:?} from {context:?}");
                }
                let ops: i64 = c.query_row("SELECT COUNT(*) FROM op", [], |r| r.get(0))?;
                assert_eq!(ops, 0, "nothing was queued");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn mark_read_queues_only_unread_messages() {
        let (_dir, store) = store();
        let ops = store.write(|c| mark_read(c, &[MessageId(1), MessageId(3)])).unwrap();
        assert_eq!(ops.len(), 1);
        store
            .write(|c| {
                let (op, _) = store::op(c, ops[0].1)?.unwrap();
                assert_eq!(op.messages.iter().map(|(m, _)| m.0).collect::<Vec<_>>(), [1], "message 3 was already read");
                Ok(())
            })
            .unwrap();
    }
}
