use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::*;
use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct NewAccount {
    pub kind: AccountKind,
    pub address: String,
    pub display_name: String,
    pub username: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
}

const ACCOUNT_COLS: &str =
    "id, kind, address, display_name, username, imap_host, imap_port, smtp_host, smtp_port, secret_ref";

fn account_row(r: &Row) -> rusqlite::Result<Account> {
    Ok(Account {
        id: r.get(0)?,
        kind: r.get(1)?,
        address: r.get(2)?,
        display_name: r.get(3)?,
        username: r.get(4)?,
        imap_host: r.get(5)?,
        imap_port: r.get(6)?,
        smtp_host: r.get(7)?,
        smtp_port: r.get(8)?,
        secret_ref: r.get(9)?,
    })
}

pub fn insert_account(conn: &Connection, a: &NewAccount) -> Result<Account> {
    conn.execute(
        "INSERT INTO account (kind, address, display_name, username, imap_host, imap_port,
                              smtp_host, smtp_port, secret_ref, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, unixepoch())",
        params![
            a.kind, a.address, a.display_name, a.username, a.imap_host, a.imap_port, a.smtp_host,
            a.smtp_port, crate::secrets::new_ref(&a.address)
        ],
    )?;
    account(conn, AccountId(conn.last_insert_rowid()))
}

pub fn accounts(conn: &Connection) -> Result<Vec<Account>> {
    let mut stmt = conn.prepare(&format!("SELECT {ACCOUNT_COLS} FROM account ORDER BY id"))?;
    let rows = stmt.query_map([], account_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn account(conn: &Connection, id: AccountId) -> Result<Account> {
    conn.query_row(&format!("SELECT {ACCOUNT_COLS} FROM account WHERE id = ?1"), [id], account_row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("account {id}")))
}

/// Finds an account by id or address.
pub fn find_account(conn: &Connection, spec: &str) -> Result<Account> {
    if let Ok(id) = spec.parse::<i64>() {
        return account(conn, AccountId(id));
    }
    conn.query_row(
        &format!("SELECT {ACCOUNT_COLS} FROM account WHERE address = ?1 COLLATE NOCASE"),
        [spec],
        account_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("account {spec}")))
}

/// Deletes the account and everything that belongs to it. The FTS rows go
/// first because the contentless index has no foreign key to cascade from.
pub fn delete_account(conn: &Connection, id: AccountId) -> Result<()> {
    conn.execute(
        "DELETE FROM message_fts WHERE rowid IN (SELECT id FROM message WHERE account_id = ?1)",
        [id],
    )?;
    conn.execute("DELETE FROM message_mailbox WHERE message_id IN (SELECT id FROM message WHERE account_id = ?1)", [id])?;
    conn.execute("DELETE FROM message WHERE account_id = ?1", [id])?;
    conn.execute("DELETE FROM thread_key WHERE account_id = ?1", [id])?;
    conn.execute("DELETE FROM thread WHERE account_id = ?1", [id])?;
    conn.execute("DELETE FROM mailbox WHERE account_id = ?1", [id])?;
    conn.execute("DELETE FROM account WHERE id = ?1", [id])?;
    Ok(())
}

const MAILBOX_COLS: &str =
    "id, account_id, remote_id, name, role, delimiter, selectable, uidvalidity, highest_modseq";

fn mailbox_row(r: &Row) -> rusqlite::Result<Mailbox> {
    Ok(Mailbox {
        id: r.get(0)?,
        account_id: r.get(1)?,
        remote_id: r.get(2)?,
        name: r.get(3)?,
        role: r.get(4)?,
        delimiter: r.get(5)?,
        selectable: r.get(6)?,
        uidvalidity: r.get(7)?,
        highest_modseq: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
    })
}

pub fn mailboxes(conn: &Connection, account: AccountId) -> Result<Vec<Mailbox>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MAILBOX_COLS} FROM mailbox WHERE account_id = ?1 ORDER BY
           CASE role WHEN 'inbox' THEN 0 WHEN 'drafts' THEN 1 WHEN 'sent' THEN 2
                     WHEN 'archive' THEN 3 WHEN 'all' THEN 4 WHEN 'spam' THEN 6
                     WHEN 'trash' THEN 7 ELSE 5 END, name"
    ))?;
    let rows = stmt.query_map([account], mailbox_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub struct RemoteMailbox {
    pub remote_id: String,
    pub name: String,
    pub role: Option<MailboxRole>,
    pub delimiter: Option<String>,
    pub selectable: bool,
}

/// Brings the account's mailbox list in line with the server's. Mailboxes
/// that disappeared are deleted along with their message links.
pub fn sync_mailbox_list(
    conn: &Connection,
    account: AccountId,
    remote: &[RemoteMailbox],
) -> Result<Vec<Mailbox>> {
    let mut keep = Vec::with_capacity(remote.len());
    for m in remote {
        conn.execute(
            "INSERT INTO mailbox (account_id, remote_id, name, role, delimiter, selectable)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account_id, remote_id) DO UPDATE SET
               name = excluded.name, role = excluded.role,
               delimiter = excluded.delimiter, selectable = excluded.selectable",
            params![account, m.remote_id, m.name, m.role, m.delimiter, m.selectable],
        )?;
        keep.push(m.remote_id.as_str());
    }
    for old in mailboxes(conn, account)? {
        if !keep.contains(&old.remote_id.as_str()) {
            crate::store::clear_mailbox(conn, old.id)?;
            conn.execute("DELETE FROM mailbox WHERE id = ?1", [old.id])?;
        }
    }
    mailboxes(conn, account)
}

pub fn set_mailbox_cursor(
    conn: &Connection,
    mailbox: MailboxId,
    uidvalidity: Option<u32>,
    highest_modseq: Option<u64>,
) -> Result<()> {
    conn.execute(
        "UPDATE mailbox SET uidvalidity = ?2, highest_modseq = ?3 WHERE id = ?1",
        params![mailbox, uidvalidity, highest_modseq.map(|v| v as i64)],
    )?;
    Ok(())
}
