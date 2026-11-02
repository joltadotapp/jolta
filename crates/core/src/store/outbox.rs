use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::Result;
use crate::compose::{AttachmentFile, Compose};
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
                   state = 'editing', dirty = 1, updated = ?12
                 WHERE id = ?1",
                params![id, c.account, c.to, c.cc, c.bcc, c.subject, c.body, c.in_reply_to, refs, c.replying_to, attachments, now()],
            )?;
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO draft (account_id, to_addrs, cc_addrs, bcc_addrs, subject, body, in_reply_to, refs,
                                    replying_to, attachments, dirty, updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, ?11)",
                params![c.account, c.to, c.cc, c.bcc, c.subject, c.body, c.in_reply_to, refs, c.replying_to, attachments, now()],
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
    })
}

const DRAFT_COLS: &str =
    "id, account_id, to_addrs, cc_addrs, bcc_addrs, subject, body, in_reply_to, refs, replying_to, attachments";

pub fn draft(conn: &Connection, id: i64) -> Result<Option<Compose>> {
    Ok(conn.query_row(&format!("SELECT {DRAFT_COLS} FROM draft WHERE id = ?1"), [id], draft_row).optional()?)
}

pub fn drafts(conn: &Connection) -> Result<Vec<Compose>> {
    let mut stmt = conn.prepare(&format!("SELECT {DRAFT_COLS} FROM draft WHERE state = 'editing' ORDER BY updated DESC"))?;
    let rows = stmt.query_map([], draft_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
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
            deleted: r.get::<_, String>(11)? == "deleted",
            server_message_id: r.get(12)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records the server copy, unless the draft changed again meanwhile.
pub fn draft_synced(conn: &Connection, id: i64, server_message_id: Option<&str>, deleted: bool) -> Result<()> {
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

/// Seconds until the next queued message is due, if any.
pub fn next_send_in(conn: &Connection, account: AccountId) -> Result<Option<i64>> {
    let at: Option<i64> = conn.query_row(
        "SELECT min(send_after) FROM outbox WHERE account_id = ?1 AND state = 'queued'",
        [account],
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
