use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::model::*;

/// Message ids carry their date: `(unix seconds << 20) | counter`.
pub const ID_SHIFT: u32 = 20;

pub fn date_to_id_floor(unix: i64) -> i64 {
    unix.max(0) << ID_SHIFT
}

fn allocate_id(conn: &Connection, date: i64) -> Result<MessageId> {
    let base = date_to_id_floor(date);
    let max: Option<i64> = conn.query_row(
        "SELECT max(id) FROM message WHERE id BETWEEN ?1 AND ?2",
        [base, base | ((1 << ID_SHIFT) - 1)],
        |r| r.get(0),
    )?;
    Ok(MessageId(max.map_or(base, |m| m + 1)))
}

pub enum Inserted {
    New(MessageId),
    Existing(MessageId),
}

impl Inserted {
    pub fn id(&self) -> MessageId {
        match self {
            Inserted::New(id) | Inserted::Existing(id) => *id,
        }
    }
}

/// Inserts a message, or links an already known one to the mailbox and
/// refreshes its flags.
pub fn insert_message(conn: &Connection, m: &NewMessage) -> Result<Inserted> {
    let existing: Option<MessageId> = conn
        .query_row(
            "SELECT id FROM message WHERE account_id = ?1 AND remote_id = ?2",
            params![m.account_id, m.remote_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        link_mailbox(conn, id, m.mailbox_id, m.uid)?;
        conn.execute("UPDATE message SET flags = ?2 WHERE id = ?1", params![id, m.flags.0])?;
        return Ok(Inserted::Existing(id));
    }
    if let Some(id) = adopt_moved(conn, m)? {
        return Ok(Inserted::Existing(id));
    }

    let e = &m.envelope;
    let thread = resolve_thread(conn, m)?;
    let id = allocate_id(conn, m.date)?;
    let (snippet, blob, has_att) = match &m.body {
        Some(b) => (b.snippet.as_str(), Some(b.blob.as_str()), b.has_attachments),
        None => ("", None, e.has_attachments),
    };
    conn.execute(
        "INSERT INTO message (id, account_id, remote_id, thread_id, message_id, date, from_name,
                              from_addr, to_addrs, cc_addrs, subject, snippet, flags,
                              has_attachments, size, blob, list_id, list_unsubscribe)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            id, m.account_id, m.remote_id, thread, e.message_id, m.date, e.from_name, e.from_addr,
            e.to, e.cc, e.subject, snippet, m.flags.0, has_att, m.size, blob,
            e.list_id.as_ref().or(m.body.as_ref().and_then(|b| b.list_id.as_ref())),
            e.list_unsubscribe.as_ref().or(m.body.as_ref().and_then(|b| b.list_unsubscribe.as_ref()))
        ],
    )?;
    link_mailbox(conn, id, m.mailbox_id, m.uid)?;
    index_fts(conn, id, e, m.body.as_ref().map_or("", |b| b.text.as_str()))?;
    if let Some(body) = &m.body {
        super::set_invitations(conn, id, &body.invitations)?;
    }
    Ok(Inserted::New(id))
}

/// A message Jolta moved into this mailbox has a link without a UID until the
/// server reports it here. The first server message with the same
/// Message-ID claims that link, so the move does not turn into a copy.
fn adopt_moved(conn: &Connection, m: &NewMessage) -> Result<Option<MessageId>> {
    let Some(message_id) = &m.envelope.message_id else { return Ok(None) };
    let id: Option<MessageId> = conn
        .query_row(
            "SELECT m.id FROM message m JOIN message_mailbox mm ON mm.message_id = m.id
             WHERE m.account_id = ?1 AND m.message_id = ?2 AND mm.mailbox_id = ?3 AND mm.uid IS NULL
             LIMIT 1",
            params![m.account_id, message_id, m.mailbox_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(id) = id else { return Ok(None) };
    conn.execute("UPDATE message SET remote_id = ?2, flags = ?3 WHERE id = ?1", params![id, m.remote_id, m.flags.0])?;
    link_mailbox(conn, id, m.mailbox_id, m.uid)?;
    Ok(Some(id))
}

/// Drops links Jolta made locally that the server never confirmed, once no
/// queued change could still confirm them.
pub fn drop_unconfirmed(conn: &Connection, mailbox: MailboxId) -> Result<()> {
    let ids: Vec<MessageId> = conn
        .prepare("SELECT message_id FROM message_mailbox WHERE mailbox_id = ?1 AND uid IS NULL")?
        .query_map([mailbox], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    unlink_messages(conn, mailbox, &ids)
}

fn link_mailbox(conn: &Connection, id: MessageId, mailbox: MailboxId, uid: Option<u32>) -> Result<()> {
    conn.execute(
        "INSERT INTO message_mailbox (mailbox_id, message_id, uid) VALUES (?1, ?2, ?3)
         ON CONFLICT (mailbox_id, message_id) DO UPDATE SET uid = excluded.uid",
        params![mailbox, id, uid],
    )?;
    Ok(())
}

fn index_fts(conn: &Connection, id: MessageId, e: &Envelope, body: &str) -> Result<()> {
    let sender = format!("{} {}", e.from_name, e.from_addr);
    let recipients = format!("{} {}", e.to, e.cc);
    conn.execute(
        "INSERT INTO message_fts (rowid, subject, sender, recipients, body) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, e.subject, sender, recipients, body],
    )?;
    Ok(())
}

/// Stores a downloaded body: blob reference, snippet, the body text in the
/// search index, and the events its calendar parts name.
pub fn attach_body(conn: &Connection, id: MessageId, body: &BodyInfo) -> Result<()> {
    let e: Option<Envelope> = conn
        .query_row(
            "SELECT from_name, from_addr, to_addrs, cc_addrs, subject FROM message WHERE id = ?1",
            [id],
            |r| {
                Ok(Envelope {
                    from_name: r.get(0)?,
                    from_addr: r.get(1)?,
                    to: r.get(2)?,
                    cc: r.get(3)?,
                    subject: r.get(4)?,
                    ..Default::default()
                })
            },
        )
        .optional()?;
    let Some(e) = e else { return Ok(()) };
    conn.execute(
        "UPDATE message SET blob = ?2, snippet = ?3, has_attachments = ?4,
                list_id = coalesce(?5, list_id), list_unsubscribe = coalesce(?6, list_unsubscribe)
         WHERE id = ?1",
        params![id, body.blob, body.snippet, body.has_attachments, body.list_id, body.list_unsubscribe],
    )?;
    conn.execute("DELETE FROM message_fts WHERE rowid = ?1", [id])?;
    index_fts(conn, id, &e, &body.text)?;
    super::set_invitations(conn, id, &body.invitations)
}

/// The next messages whose stored body hasn't been read for list headers.
pub fn header_scan_batch(conn: &Connection, limit: usize) -> Result<Vec<(MessageId, String)>> {
    let mut stmt = conn.prepare(
        "SELECT h.message_id, coalesce(m.blob, '') FROM header_scan h LEFT JOIN message m ON m.id = h.message_id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records what a header scan found and takes the message off the list.
pub fn finish_header_scan(conn: &Connection, id: MessageId, list_id: Option<&str>, list_unsubscribe: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE message SET list_id = coalesce(?2, list_id), list_unsubscribe = coalesce(?3, list_unsubscribe) WHERE id = ?1",
        params![id, list_id, list_unsubscribe],
    )?;
    conn.execute("DELETE FROM header_scan WHERE message_id = ?1", [id])?;
    Ok(())
}

pub fn smime_scan_batch(conn: &Connection, limit: usize) -> Result<Vec<(MessageId, String)>> {
    let mut stmt = conn.prepare(
        "SELECT s.message_id, coalesce(m.blob, '') FROM smime_scan s LEFT JOIN message m ON m.id = s.message_id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn finish_smime_scan(conn: &Connection, id: MessageId) -> Result<()> {
    conn.execute("DELETE FROM smime_scan WHERE message_id = ?1", [id])?;
    Ok(())
}

pub fn set_flags(conn: &Connection, id: MessageId, flags: Flags) -> Result<()> {
    conn.execute("UPDATE message SET flags = ?2 WHERE id = ?1", params![id, flags.0])?;
    Ok(())
}

/// UID to message id and flags for every message the store has in a mailbox.
pub fn mailbox_uids(conn: &Connection, mailbox: MailboxId) -> Result<HashMap<u32, (MessageId, Flags)>> {
    let mut stmt = conn.prepare(
        "SELECT mm.uid, m.id, m.flags FROM message_mailbox mm JOIN message m ON m.id = mm.message_id
         WHERE mm.mailbox_id = ?1 AND mm.uid IS NOT NULL",
    )?;
    let rows = stmt.query_map([mailbox], |r| Ok((r.get(0)?, (r.get(1)?, Flags(r.get(2)?)))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Unlinks messages from a mailbox and deletes the ones left in no mailbox.
pub fn unlink_messages(conn: &Connection, mailbox: MailboxId, ids: &[MessageId]) -> Result<()> {
    for id in ids {
        conn.execute(
            "DELETE FROM message_mailbox WHERE mailbox_id = ?1 AND message_id = ?2",
            params![mailbox, id],
        )?;
        let linked: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM message_mailbox WHERE message_id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        if !linked {
            conn.execute("DELETE FROM message_fts WHERE rowid = ?1", [id])?;
            conn.execute("DELETE FROM message WHERE id = ?1", [id])?;
        }
    }
    Ok(())
}

/// Deletes those of `ids` that no mailbox holds any more. Graph removes a
/// moved message from one folder's delta before another folder's delta adds
/// it, so rows are only deleted once every folder of a sync has been read.
pub fn delete_unfiled(conn: &Connection, ids: &[MessageId]) -> Result<usize> {
    let mut deleted = 0;
    for id in ids {
        let filed: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM message_mailbox WHERE message_id = ?1)", [id], |r| r.get(0))?;
        if !filed {
            delete_messages(conn, &[*id])?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

pub fn clear_mailbox(conn: &Connection, mailbox: MailboxId) -> Result<()> {
    let ids: Vec<MessageId> = conn
        .prepare("SELECT message_id FROM message_mailbox WHERE mailbox_id = ?1")?
        .query_map([mailbox], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    unlink_messages(conn, mailbox, &ids)
}

/// Messages of an account that have no downloaded body yet, newest first,
/// with the mailbox and UID to fetch them from.
pub fn messages_without_body(
    conn: &Connection,
    account: AccountId,
    max_size: u32,
    before: i64,
    limit: usize,
) -> Result<Vec<(MessageId, MailboxId, u32)>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, mm.mailbox_id, mm.uid FROM message m
         JOIN message_mailbox mm ON mm.message_id = m.id AND mm.uid IS NOT NULL
         WHERE m.account_id = ?1 AND m.blob IS NULL AND m.size <= ?2 AND m.id < ?3
         GROUP BY m.id ORDER BY m.id DESC LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![account, max_size, before, limit as i64], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Where to fetch a message from: its account, a mailbox path and UID.
pub fn message_location(conn: &Connection, id: MessageId) -> Result<Option<(AccountId, MailboxId, String, u32, Option<String>)>> {
    Ok(conn
        .query_row(
            "SELECT m.account_id, b.id, b.remote_id, mm.uid, m.blob FROM message m
             JOIN message_mailbox mm ON mm.message_id = m.id AND mm.uid IS NOT NULL
             JOIN mailbox b ON b.id = mm.mailbox_id
             WHERE m.id = ?1 LIMIT 1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?)
}

pub fn message_blob(conn: &Connection, id: MessageId) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT blob FROM message WHERE id = ?1", [id], |r| r.get(0))
        .optional()?
        .flatten())
}

// Threading -----------------------------------------------------------------

const REPLY_PREFIXES: &[&str] = &["re", "fwd", "fw", "aw", "sv", "vs", "antw", "wg", "tr", "rif"];

/// Lower-cased subject without reply and forward prefixes.
pub fn base_subject(subject: &str) -> (String, bool) {
    let mut s = subject.trim();
    let mut had_prefix = false;
    loop {
        let Some(colon) = s.find(':') else { break };
        let head = s[..colon].trim();
        let head = head.split('[').next().unwrap_or(head).trim();
        if REPLY_PREFIXES.iter().any(|p| head.eq_ignore_ascii_case(p)) {
            s = s[colon + 1..].trim_start();
            had_prefix = true;
        } else {
            break;
        }
    }
    (s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase(), had_prefix)
}

/// Picks the thread for a new message. A provider thread id wins. Otherwise
/// any Message-ID the message names or is named by links it to a thread; a
/// reply with no references falls back to its base subject.
fn resolve_thread(conn: &Connection, m: &NewMessage) -> Result<ThreadId> {
    let e = &m.envelope;
    let mut keys: Vec<String> = Vec::new();
    if let Some(hint) = &m.thread_hint {
        keys.push(format!("x:{hint}"));
    } else {
        keys.extend(e.message_id.iter().map(|id| format!("m:{id}")));
        keys.extend(e.references.iter().map(|id| format!("m:{id}")));
        keys.extend(e.in_reply_to.iter().map(|id| format!("m:{id}")));
    }
    let (subject, is_reply) = base_subject(&e.subject);
    let subject_key = (!subject.is_empty()).then(|| format!("s:{subject}"));

    let mut threads: Vec<ThreadId> = Vec::new();
    {
        let mut stmt = conn.prepare_cached(
            "SELECT thread_id FROM thread_key WHERE account_id = ?1 AND key = ?2",
        )?;
        for k in &keys {
            if let Some(t) = stmt.query_row(params![m.account_id, k], |r| r.get(0)).optional()? {
                if !threads.contains(&t) {
                    threads.push(t);
                }
            }
        }
        let no_refs = m.thread_hint.is_none() && e.references.is_empty() && e.in_reply_to.is_empty();
        if threads.is_empty() && is_reply && no_refs {
            if let Some(k) = &subject_key {
                if let Some(t) = stmt.query_row(params![m.account_id, k], |r| r.get(0)).optional()? {
                    threads.push(t);
                }
            }
        }
    }

    let thread = match threads.iter().min() {
        Some(&t) => t,
        None => {
            conn.execute("INSERT INTO thread (account_id) VALUES (?1)", [m.account_id])?;
            ThreadId(conn.last_insert_rowid())
        }
    };
    for &other in threads.iter().filter(|&&t| t != thread) {
        conn.execute("UPDATE message SET thread_id = ?1 WHERE thread_id = ?2", params![thread, other])?;
        conn.execute("UPDATE thread_key SET thread_id = ?1 WHERE thread_id = ?2", params![thread, other])?;
        conn.execute("DELETE FROM thread WHERE id = ?1", [other])?;
    }

    let mut insert = conn.prepare_cached(
        "INSERT INTO thread_key (account_id, key, thread_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (account_id, key) DO NOTHING",
    )?;
    for k in keys.iter().chain(subject_key.iter()) {
        insert.execute(params![m.account_id, k, thread])?;
    }
    Ok(thread)
}

#[cfg(test)]
mod tests {
    use super::base_subject;

    #[test]
    fn strips_reply_prefixes() {
        assert_eq!(base_subject("Re: RE: Fwd:  Lunch  plans"), ("lunch plans".into(), true));
        assert_eq!(base_subject("AW: [team] Re: x"), ("[team] re: x".into(), true));
        assert_eq!(base_subject("Lunch: Friday"), ("lunch: friday".into(), false));
    }
}

/// Each message's mailbox and server UID, for messages the server knows.
pub fn server_locations(conn: &Connection, ids: &[MessageId]) -> Result<Vec<(MailboxId, u32)>> {
    let mut stmt =
        conn.prepare_cached("SELECT mailbox_id, uid FROM message_mailbox WHERE message_id = ?1 AND uid IS NOT NULL")?;
    let mut out = Vec::new();
    for id in ids {
        for row in stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))? {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Message-ID header values of these messages, where they have one.
pub fn header_message_ids(conn: &Connection, ids: &[MessageId]) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached("SELECT message_id FROM message WHERE id = ?1 AND message_id IS NOT NULL")?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(mid) = stmt.query_row([id], |r| r.get(0)).optional()? {
            out.push(mid);
        }
    }
    Ok(out)
}

/// Gmail labels: links the message to exactly these label mailboxes, plus
/// `all` (All Mail), which keeps its UID.
pub fn set_labels(conn: &Connection, id: MessageId, all: MailboxId, labels: &[MailboxId]) -> Result<()> {
    let mut current: Vec<MailboxId> = conn
        .prepare_cached("SELECT mailbox_id FROM message_mailbox WHERE message_id = ?1 AND mailbox_id != ?2")?
        .query_map(params![id, all], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    current.sort();
    let mut wanted: Vec<MailboxId> = labels.iter().copied().filter(|m| *m != all).collect();
    wanted.sort();
    wanted.dedup();
    if current == wanted {
        return Ok(());
    }
    conn.execute("DELETE FROM message_mailbox WHERE message_id = ?1 AND mailbox_id != ?2", params![id, all])?;
    for m in wanted {
        conn.execute("INSERT INTO message_mailbox (mailbox_id, message_id, uid) VALUES (?1, ?2, NULL)", params![m, id])?;
    }
    Ok(())
}

/// Removes messages entirely: every mailbox link, the search index row and
/// the message row.
pub fn delete_messages(conn: &Connection, ids: &[MessageId]) -> Result<()> {
    for id in ids {
        conn.execute("DELETE FROM message_mailbox WHERE message_id = ?1", [id])?;
        conn.execute("DELETE FROM message_fts WHERE rowid = ?1", [id])?;
        conn.execute("DELETE FROM message WHERE id = ?1", [id])?;
    }
    Ok(())
}
