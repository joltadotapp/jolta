//! Lists and search: a query becomes one SQL statement that yields matching
//! messages newest first; rows are grouped into threads as they stream.

pub mod query;

use std::collections::HashSet;

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

pub use query::{Query, Status, Term, TermKind, TextField, parse};

use crate::model::*;
use crate::store::date_to_id_floor;
use crate::Result;

/// One row of a message list: a thread, shown through its newest matching message.
#[derive(Clone, Debug)]
pub struct ThreadRow {
    pub thread_id: ThreadId,
    pub message_id: MessageId,
    pub account_id: AccountId,
    pub date: i64,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub snippet: String,
    pub message_count: u32,
    pub unread_count: u32,
    pub flagged: bool,
    pub has_attachments: bool,
}

fn fts_string(text: &str, phrase: bool, prefix: bool) -> String {
    let quoted = format!("\"{}\"", text.replace('"', "\"\""));
    if prefix && !phrase { format!("{quoted}*") } else { quoted }
}

fn fts_term(field: Option<TextField>, text: &str, phrase: bool, prefix: bool) -> String {
    let s = fts_string(text, phrase, prefix);
    match field {
        None => s,
        Some(TextField::From) => format!("sender : {s}"),
        Some(TextField::To) => format!("recipients : {s}"),
        Some(TextField::Subject) => format!("subject : {s}"),
    }
}

const MAILBOX_MATCH: &str = "(b.role = ?1 OR b.name = ?1 COLLATE NOCASE OR b.remote_id = ?1 COLLATE NOCASE)";

fn mailbox_match(param: usize) -> String {
    MAILBOX_MATCH.replace("?1", &format!("?{param}"))
}

struct Sql {
    from: String,
    order: &'static str,
    conds: Vec<String>,
    params: Vec<Value>,
}

impl Sql {
    fn param(&mut self, v: impl Into<Value>) -> usize {
        self.params.push(v.into());
        self.params.len()
    }
}

/// Builds `SELECT m.id, m.thread_id ... ORDER BY newest`. The driving table
/// is the FTS index when there is text to match, the mailbox link table when
/// the query names a mailbox, and the message table otherwise, so each walks
/// its own index in date order and stops early.
fn plan(q: &Query) -> Sql {
    let mut sql = Sql { from: String::new(), order: "", conds: Vec::new(), params: Vec::new() };

    let positive_text: Vec<String> = q
        .terms
        .iter()
        .filter(|t| !t.negated)
        .filter_map(|t| match &t.kind {
            TermKind::Text { field, text, phrase, prefix } => Some(fts_term(*field, text, *phrase, *prefix)),
            _ => None,
        })
        .collect();
    let driver_mailbox = q.terms.iter().position(|t| {
        !t.negated
            && (matches!(t.kind, TermKind::In(ref v) | TermKind::Label(ref v) if !is_anywhere(v))
                || matches!(t.kind, TermKind::Mailbox(_)))
    });

    if !positive_text.is_empty() {
        sql.from = "message_fts JOIN message m ON m.id = message_fts.rowid".into();
        sql.order = "message_fts.rowid DESC";
        let p = sql.param(positive_text.join(" AND "));
        sql.conds.push(format!("message_fts MATCH ?{p}"));
    } else if let Some(i) = driver_mailbox {
        sql.from = "message_mailbox mm JOIN message m ON m.id = mm.message_id".into();
        sql.order = "mm.message_id DESC";
        let cond = match &q.terms[i].kind {
            TermKind::Mailbox(id) => format!("mm.mailbox_id = {id}"),
            TermKind::In(v) | TermKind::Label(v) => {
                let p = sql.param(v.clone());
                format!("mm.mailbox_id IN (SELECT b.id FROM mailbox b WHERE {})", mailbox_match(p))
            }
            _ => unreachable!("driver_mailbox only picks mailbox terms"),
        };
        sql.conds.push(cond);
    } else {
        sql.from = "message m".into();
        sql.order = "m.id DESC";
    }

    for (i, t) in q.terms.iter().enumerate() {
        let cond = match &t.kind {
            TermKind::Text { field, text, phrase, prefix } => {
                if !t.negated {
                    continue;
                }
                let p = sql.param(fts_term(*field, text, *phrase, *prefix));
                sql.conds.push(format!("m.id NOT IN (SELECT rowid FROM message_fts WHERE message_fts MATCH ?{p})"));
                continue;
            }
            TermKind::In(v) | TermKind::Label(v) => {
                if is_anywhere(v) || (Some(i) == driver_mailbox && positive_text.is_empty()) {
                    continue;
                }
                let p = sql.param(v.clone());
                format!(
                    "EXISTS (SELECT 1 FROM message_mailbox x JOIN mailbox b ON b.id = x.mailbox_id
                             WHERE x.message_id = m.id AND {})",
                    mailbox_match(p)
                )
            }
            TermKind::Mailbox(id) => {
                if Some(i) == driver_mailbox && positive_text.is_empty() {
                    continue;
                }
                format!("EXISTS (SELECT 1 FROM message_mailbox x WHERE x.message_id = m.id AND x.mailbox_id = {id})")
            }
            TermKind::HasAttachment => "m.has_attachments = 1".into(),
            TermKind::Is(Status::Unread) => format!("(m.flags & {}) = 0", Flags::SEEN),
            TermKind::Is(Status::Read) => format!("(m.flags & {}) != 0", Flags::SEEN),
            TermKind::Is(Status::Flagged) => format!("(m.flags & {}) != 0", Flags::FLAGGED),
            TermKind::Account(v) => {
                let p = sql.param(v.clone());
                format!("m.account_id IN (SELECT id FROM account WHERE id = ?{p} OR address = ?{p} COLLATE NOCASE)")
            }
            TermKind::Before(t) => format!("m.id < {}", date_to_id_floor(*t)),
            TermKind::After(t) => format!("m.id >= {}", date_to_id_floor(*t)),
        };
        sql.conds.push(if t.negated { format!("NOT ({cond})") } else { cond });
    }
    sql
}

fn is_anywhere(mailbox: &str) -> bool {
    mailbox.eq_ignore_ascii_case("anywhere") || mailbox.eq_ignore_ascii_case("any")
}

fn where_clause(sql: &Sql) -> String {
    if sql.conds.is_empty() { String::new() } else { format!(" WHERE {}", sql.conds.join(" AND ")) }
}

/// Threads matching the query, newest first: `offset` threads skipped, then
/// up to `limit` returned. Stops reading once the page is full.
pub fn query_page(conn: &Connection, q: &Query, offset: usize, limit: usize) -> Result<Vec<ThreadRow>> {
    let hits = scan(conn, q, Some(offset + limit))?;
    thread_rows(conn, hits.get(offset..).unwrap_or_default())
}

/// Every matching thread in list order, as (thread, newest matching message).
/// One scan gives the count and lets any page be cut out without re-reading.
pub fn query_order(conn: &Connection, q: &Query) -> Result<Vec<(ThreadId, MessageId)>> {
    scan(conn, q, None)
}

fn scan(conn: &Connection, q: &Query, stop_after: Option<usize>) -> Result<Vec<(ThreadId, MessageId)>> {
    let sql = plan(q);
    let text = format!(
        "SELECT m.id, m.thread_id FROM {}{} ORDER BY {}",
        sql.from,
        where_clause(&sql),
        sql.order
    );
    let mut stmt = conn.prepare_cached(&text)?;
    let mut rows = stmt.query(params_from_iter(sql.params.iter()))?;
    let mut seen = HashSet::new();
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        let thread: ThreadId = row.get(1)?;
        if seen.insert(thread) {
            hits.push((thread, row.get::<_, MessageId>(0)?));
            if Some(hits.len()) == stop_after {
                break;
            }
        }
    }
    Ok(hits)
}

pub fn thread_rows(conn: &Connection, hits: &[(ThreadId, MessageId)]) -> Result<Vec<ThreadRow>> {
    hits.iter().map(|&(t, m)| thread_row(conn, t, m)).collect()
}

/// Number of threads matching the query.
pub fn query_count(conn: &Connection, q: &Query) -> Result<u32> {
    let sql = plan(q);
    let text = format!("SELECT count(DISTINCT m.thread_id) FROM {}{}", sql.from, where_clause(&sql));
    Ok(conn.prepare_cached(&text)?.query_row(params_from_iter(sql.params.iter()), |r| r.get(0))?)
}

fn thread_row(conn: &Connection, thread: ThreadId, message: MessageId) -> Result<ThreadRow> {
    let mut row = conn.prepare_cached(
        "SELECT account_id, date, from_name, from_addr, subject, snippet FROM message WHERE id = ?1",
    )?
    .query_row([message], |r| {
        Ok(ThreadRow {
            thread_id: thread,
            message_id: message,
            account_id: r.get(0)?,
            date: r.get(1)?,
            from_name: r.get(2)?,
            from_addr: r.get(3)?,
            subject: r.get(4)?,
            snippet: r.get(5)?,
            message_count: 0,
            unread_count: 0,
            flagged: false,
            has_attachments: false,
        })
    })?;
    conn.prepare_cached(
        "SELECT count(*), coalesce(sum((flags & ?2) = 0), 0), coalesce(max((flags & ?3) != 0), 0),
                coalesce(max(has_attachments), 0)
         FROM message WHERE thread_id = ?1",
    )?
    .query_row(rusqlite::params![thread, Flags::SEEN, Flags::FLAGGED], |r| {
        row.message_count = r.get(0)?;
        row.unread_count = r.get(1)?;
        row.flagged = r.get(2)?;
        row.has_attachments = r.get(3)?;
        Ok(())
    })?;
    Ok(row)
}
