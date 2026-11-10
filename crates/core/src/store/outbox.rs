use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::Result;
use crate::compose::{AttachmentFile, Compose, ImageSize};
use crate::model::*;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn encode_attachments(list: &[AttachmentFile]) -> String {
    list.iter().map(|a| format!("{}\t{}\t{}", a.name, a.mime_type, a.path)).collect::<Vec<_>>().join("\n")
}

fn decode_attachments(s: &str) -> Vec<AttachmentFile> {
    s.lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            Some(AttachmentFile { name: p.next()?.into(), mime_type: p.next()?.into(), path: p.next()?.into() })
        })
        .collect()
}

/// Saves a compose as a draft (new or updated) and marks it for the server.
pub fn save_draft(conn: &Connection, c: &Compose) -> Result<i64> {
    let refs = c.references.join(" ");
    let attachments = encode_attachments(&c.attachments);
    match c.draft {
        Some(id) => {
            conn.execute(
                "UPDATE draft SET account_id = ?2, to_addrs = ?3, cc_addrs = ?4, bcc_addrs = ?5, subject = ?6,
                   body = ?7, in_reply_to = ?8, refs = ?9, replying_to = ?10, attachments = ?11,
                   state = 'editing', dirty = 1, updated = ?12, image_size = ?13, from_addr = ?14
                 WHERE id = ?1",
                params![id, c.account, c.to, c.cc, c.bcc, c.subject, c.body, c.in_reply_to, refs, c.replying_to, attachments, now(), c.image_size.name(), c.from],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO draft (account_id, to_addrs, cc_addrs, bcc_addrs, subject, body, in_reply_to, refs,
                                    replying_to, attachments, dirty, updated, image_size, from_addr)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11, ?12, ?13)",
                params![c.account, c.to, c.cc, c.bcc, c.subject, c.body, c.in_reply_to, refs, c.replying_to, attachments, now(), c.image_size.name(), c.from],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

fn draft_row(r: &Row) -> rusqlite::Result<Compose> {
    Ok(Compose {
        draft: Some(r.get(0)?),
        account: r.get(1)?,
        to: r.get(2)?,
        cc: r.get(3)?,
        bcc: r.get(4)?,
        subject: r.get(5)?,
        body: r.get(6)?,
        in_reply_to: r.get(7)?,
        references: r.get::<_, String>(8)?.split_whitespace().map(str::to_string).collect(),
        replying_to: r.get(9)?,
        attachments: decode_attachments(&r.get::<_, String>(10)?),
        image_size: ImageSize::from_name(&r.get::<_, String>(11)?),
        from: r.get(12)?,
    })
}

const DRAFT_COLS: &str =
    "id, account_id, to_addrs, cc_addrs, bcc_addrs, subject, body, in_reply_to, refs, replying_to, attachments, image_size, from_addr";

pub fn draft(conn: &Connection, id: i64) -> Result<Option<Compose>> {
    Ok(conn.query_row(&format!("SELECT {DRAFT_COLS} FROM draft WHERE id = ?1"), [id], draft_row).optional()?)
}

pub fn drafts(conn: &Connection) -> Result<Vec<Compose>> {
    let mut stmt = conn.prepare(&format!("SELECT {DRAFT_COLS} FROM draft WHERE state = 'editing' ORDER BY updated DESC"))?;
    let rows = stmt.query_map([], draft_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The newest draft a conversation holds in a Drafts mailbox.
pub enum ThreadDraft {
    /// Jolta saved it: the draft as written.
    Saved(Compose),
    /// Written elsewhere: the message to read it from.
    Elsewhere(MessageId),
}

/// None when the conversation has no draft, or its draft is being sent or
/// discarded.
pub fn thread_draft(conn: &Connection, thread: ThreadId) -> Result<Option<ThreadDraft>> {
    let found: Option<(MessageId, AccountId, Option<String>)> = conn
        .query_row(
            "SELECT m.id, m.account_id, m.message_id FROM message m
             JOIN message_mailbox mm ON mm.message_id = m.id
             JOIN mailbox b ON b.id = mm.mailbox_id AND b.role = 'drafts'
             WHERE m.thread_id = ?1 ORDER BY m.date DESC, m.id DESC LIMIT 1",
            [thread],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((message, account, message_id)) = found else { return Ok(None) };
    Ok(match saved_draft(conn, account, message_id.as_deref())? {
        Some((c, state)) if state == "editing" => Some(ThreadDraft::Saved(c)),
        Some(_) => None,
        None => Some(ThreadDraft::Elsewhere(message)),
    })
}

/// The draft whose server copy has this Message-ID, with its state.
fn saved_draft(conn: &Connection, account: AccountId, server_message_id: Option<&str>) -> Result<Option<(Compose, String)>> {
    let Some(id) = server_message_id else { return Ok(None) };
    Ok(conn
        .query_row(
            &format!("SELECT {DRAFT_COLS}, state FROM draft WHERE account_id = ?1 AND server_message_id = ?2 ORDER BY id DESC LIMIT 1"),
            params![account, id],
            |r| Ok((draft_row(r)?, r.get("state")?)),
        )
        .optional()?)
}

/// Makes `message`, a draft written elsewhere, a local draft holding `c`,
/// so saving it replaces that server copy. Opening it again returns the same
/// draft.
pub fn adopt_draft(conn: &Connection, message: MessageId, c: &Compose) -> Result<Compose> {
    let (account, message_id): (AccountId, Option<String>) =
        conn.query_row("SELECT account_id, message_id FROM message WHERE id = ?1", [message], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if let Some((existing, _)) = saved_draft(conn, account, message_id.as_deref())? {
        return Ok(existing);
    }
    let id = save_draft(conn, &Compose { account: Some(account), draft: None, ..c.clone() })?;
    conn.execute("UPDATE draft SET server_message_id = ?2, dirty = 0 WHERE id = ?1", params![id, message_id])?;
    draft(conn, id)?.ok_or_else(|| crate::Error::NotFound(format!("draft {id}")))
}

/// Marks a draft gone; the engine deletes its server copy, then the row.
pub fn discard_draft(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE draft SET state = 'deleted', dirty = 1 WHERE id = ?1", [id])?;
    Ok(())
}

pub struct DirtyDraft {
    pub id: i64,
    pub compose: Compose,
    pub deleted: bool,
    pub server_message_id: Option<String>,
}

pub fn dirty_drafts(conn: &Connection, account: AccountId) -> Result<Vec<DirtyDraft>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {DRAFT_COLS}, state, server_message_id FROM draft WHERE dirty = 1 AND account_id = ?1"
    ))?;
    let rows = stmt.query_map([account], |r| {
        Ok(DirtyDraft {
            id: r.get(0)?,
            compose: draft_row(r)?,
            deleted: r.get::<_, String>("state")? == "deleted",
            server_message_id: r.get("server_message_id")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records the server copy, unless the draft changed again meanwhile. The
/// copy it replaced leaves Drafts here too, so opening it can't bring back
/// the old text.
pub fn draft_synced(conn: &Connection, id: i64, server_message_id: Option<&str>, deleted: bool) -> Result<()> {
    let old: Option<(AccountId, Option<String>)> =
        conn.query_row("SELECT account_id, server_message_id FROM draft WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if let Some((account, Some(old))) = old.filter(|(_, old)| old.as_deref() != server_message_id) {
        let mut stmt = conn.prepare(
            "SELECT mm.mailbox_id, m.id FROM message m JOIN message_mailbox mm ON mm.message_id = m.id
             JOIN mailbox b ON b.id = mm.mailbox_id AND b.role = 'drafts'
             WHERE m.account_id = ?1 AND m.message_id = ?2",
        )?;
        let copies: Vec<(MailboxId, MessageId)> =
            stmt.query_map(params![account, old], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        for (mailbox, message) in copies {
            super::unlink_messages(conn, mailbox, &[message])?;
        }
    }
    if deleted {
        conn.execute("DELETE FROM draft WHERE id = ?1", [id])?;
    } else {
        conn.execute(
            "UPDATE draft SET server_message_id = ?2, dirty = 0 WHERE id = ?1",
            params![id, server_message_id],
        )?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct OutboxItem {
    pub id: i64,
    pub account: AccountId,
    pub draft: Option<i64>,
    pub blob: String,
    pub message_id: String,
    pub sender: String,
    pub recipients: Vec<String>,
    pub replying_to: Option<MessageId>,
    pub attempts: u32,
}

pub fn queue_outbox(
    conn: &Connection,
    account: AccountId,
    draft: Option<i64>,
    blob: &str,
    message_id: &str,
    sender: &str,
    recipients: &[String],
    replying_to: Option<MessageId>,
    delay_seconds: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO outbox (account_id, draft_id, blob, message_id, sender, recipients, replying_to, send_after, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![account, draft, blob, message_id, sender, recipients.join(","), replying_to, now() + delay_seconds, now()],
    )?;
    if let Some(d) = draft {
        conn.execute("UPDATE draft SET state = 'sending' WHERE id = ?1", [d])?;
    }
    Ok(conn.last_insert_rowid())
}

/// Takes a message back out of the outbox while it is still waiting.
/// Returns its draft so the app can reopen it.
pub fn cancel_send(conn: &Connection, id: i64) -> Result<Option<i64>> {
    let draft: Option<Option<i64>> = conn
        .query_row("SELECT draft_id FROM outbox WHERE id = ?1 AND state = 'queued'", [id], |r| r.get(0))
        .optional()?;
    let Some(draft) = draft else { return Ok(None) };
    conn.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
    if let Some(d) = draft {
        conn.execute("UPDATE draft SET state = 'editing' WHERE id = ?1", [d])?;
    }
    Ok(draft)
}

/// Makes a waiting message due now, for a retry the user asked for.
pub fn send_now(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("UPDATE outbox SET send_after = min(send_after, ?2) WHERE id = ?1 AND state = 'queued'", params![id, now()])?;
    Ok(())
}

pub fn due_outbox(conn: &Connection, account: AccountId) -> Result<Vec<OutboxItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, draft_id, blob, message_id, sender, recipients, replying_to, attempts
         FROM outbox WHERE account_id = ?1 AND state = 'queued' AND send_after <= ?2 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![account, now()], |r| {
        Ok(OutboxItem {
            id: r.get(0)?,
            account: r.get(1)?,
            draft: r.get(2)?,
            blob: r.get(3)?,
            message_id: r.get(4)?,
            sender: r.get(5)?,
            recipients: r.get::<_, String>(6)?.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect(),
            replying_to: r.get(7)?,
            attempts: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A message waiting in the outbox past the undo window: sent later.
#[derive(Clone, Debug)]
pub struct Scheduled {
    pub outbox: i64,
    pub account: AccountId,
    pub subject: String,
    pub send_at: i64,
}

/// Messages set to go out later than `after`, soonest first.
pub fn scheduled(conn: &Connection, after: i64) -> Result<Vec<Scheduled>> {
    let mut stmt = conn.prepare(
        "SELECT o.id, o.account_id, coalesce(d.subject, ''), o.send_after FROM outbox o LEFT JOIN draft d ON d.id = o.draft_id
         WHERE o.state = 'queued' AND o.send_after > ?1 ORDER BY o.send_after",
    )?;
    let rows = stmt.query_map([after], |r| Ok(Scheduled { outbox: r.get(0)?, account: r.get(1)?, subject: r.get(2)?, send_at: r.get(3)? }))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Seconds until the next queued message due after `after` is due, if any.
pub fn next_send_in(conn: &Connection, account: AccountId, after: i64) -> Result<Option<i64>> {
    let at: Option<i64> = conn.query_row(
        "SELECT min(send_after) FROM outbox WHERE account_id = ?1 AND state = 'queued' AND send_after > ?2",
        params![account, after],
        |r| r.get(0),
    )?;
    Ok(at.map(|t| (t - now()).max(0)))
}

pub fn outbox_sent(conn: &Connection, item: &OutboxItem) -> Result<()> {
    conn.execute("UPDATE outbox SET state = 'sent' WHERE id = ?1", [item.id])?;
    if let Some(d) = item.draft {
        conn.execute("UPDATE draft SET state = 'deleted', dirty = 1 WHERE id = ?1", [d])?;
    }
    Ok(())
}

pub fn outbox_failed(conn: &Connection, id: i64, error: &str, give_up: bool) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET attempts = attempts + 1, error = ?2,
           state = CASE WHEN ?3 THEN 'failed' ELSE state END,
           send_after = CASE WHEN ?3 THEN send_after ELSE ?4 END
         WHERE id = ?1",
        params![id, error, give_up, now() + 30],
    )?;
    Ok(())
}

/// Whether the account's Sent mailbox holds a message with this Message-ID.
pub fn in_sent(conn: &Connection, account: AccountId, message_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM message m JOIN message_mailbox mm ON mm.message_id = m.id
                        JOIN mailbox b ON b.id = mm.mailbox_id AND b.role = 'sent'
                        WHERE m.account_id = ?1 AND m.message_id = ?2)",
        params![account, message_id],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (std::path::PathBuf, super::super::Store) {
        let dir = std::env::temp_dir().join(format!("jolta-outbox-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = super::super::Store::open(&dir.join("jolta.sqlite")).unwrap();
        store
            .write(|c| Ok(c.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'imap', 'a@x', 'a@x', 'r', 0);")?))
            .unwrap();
        (dir, store)
    }

    fn compose(subject: &str) -> Compose {
        Compose { account: Some(AccountId(1)), subject: subject.into(), ..Compose::default() }
    }

    /// An outbox row's id is its Undo send token, and an answered
    /// invitation keeps it to find its reply mail again. A message taken
    /// back never hands that id to the next one, so a stale token can't
    /// take back, or send early, a different message.
    #[test]
    fn a_message_taken_back_keeps_its_undo_send_token_to_itself() {
        let (dir, store) = store();
        store
            .write(|c| {
                let queue = |c: &Connection, draft| queue_outbox(c, AccountId(1), Some(draft), "blob", "<m@x>", "a@x", &["b@x".into()], None, 30);
                let first = save_draft(c, &compose("First"))?;
                let taken_back = queue(c, first)?;
                assert_eq!(cancel_send(c, taken_back)?, Some(first));
                let second = save_draft(c, &compose("Second"))?;
                let newer = queue(c, second)?;
                assert_ne!(newer, taken_back, "the token is not given out again");
                assert_eq!(cancel_send(c, taken_back)?, None, "a stale Undo send takes back nothing");
                assert_eq!(scheduled(c, 0)?.len(), 1, "the newer message still waits");
                Ok(())
            })
            .unwrap();
        std::fs::remove_dir_all(dir).ok();
    }

    /// A draft's id is how the app and agents open and save it. A draft
    /// gone for good never hands that id to a new one, so a compose window
    /// or an agent's note left over from it can't open or overwrite
    /// another draft.
    #[test]
    fn a_draft_gone_for_good_keeps_its_id_to_itself() {
        let (dir, store) = store();
        store
            .write(|c| {
                let gone = save_draft(c, &compose("Gone"))?;
                discard_draft(c, gone)?;
                draft_synced(c, gone, None, true)?;
                let newer = save_draft(c, &compose("New"))?;
                assert_ne!(newer, gone, "the id is not given out again");
                assert!(draft(c, gone)?.is_none(), "the old id opens nothing");
                Ok(())
            })
            .unwrap();
        std::fs::remove_dir_all(dir).ok();
    }
}
