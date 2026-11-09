//! Lists and search: a query becomes one SQL statement that yields matching
//! messages newest first; rows are grouped into threads as they stream.

pub mod query;

use std::collections::HashSet;

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

pub use query::{Query, Status, Term, TermKind, TextField, parse, within};

use crate::model::*;
use crate::calendar::tz;
use crate::store::{InvitationState, base_subject, date_to_id_floor};
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
    /// The shown message's sender is a VIP.
    pub vip: bool,
    /// AI sorting says the conversation waits for the user's reply.
    pub needs_reply: bool,
    pub invitation: Option<InvitationState>,
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

/// Automated senders. LIKE ignores ASCII case.
const NOTIFICATION: &str = "(m.from_addr LIKE 'noreply%' OR m.from_addr LIKE 'no-reply%' \
     OR m.from_addr LIKE 'donotreply%' OR m.from_addr LIKE 'do-not-reply%' OR m.from_addr LIKE 'notification%' \
     OR m.from_addr LIKE 'notify%' OR m.from_addr LIKE 'alert%' OR m.from_addr LIKE '%@notifications.%' \
     OR m.from_addr LIKE '%@notify.%' OR m.from_addr LIKE 'mailer-daemon%')";

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
/// its own index in date order and stops early. `is:invite` asks the store
/// first, since whether an invitation is answered depends on the calendars.
fn plan(conn: &Connection, q: &Query) -> Result<Sql> {
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
    // As on Gmail, mail in Trash or Spam shows only when the query names a
    // mailbox: `in:trash`, `in:anywhere`, or the Trash list itself.
    let names_mailbox =
        q.terms.iter().any(|t| !t.negated && matches!(t.kind, TermKind::In(_) | TermKind::Label(_) | TermKind::Mailbox(_)));
    if !names_mailbox {
        sql.conds.push(
            "m.id NOT IN (SELECT message_id FROM message_mailbox
                          WHERE mailbox_id IN (SELECT id FROM mailbox WHERE role IN ('trash', 'spam')))"
                .into(),
        );
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
            TermKind::HasList => "m.list_id IS NOT NULL".into(),
            TermKind::Is(Status::Unread) => format!("(m.flags & {}) = 0", Flags::SEEN),
            TermKind::Is(Status::Read) => format!("(m.flags & {}) != 0", Flags::SEEN),
            TermKind::Is(Status::Flagged) => format!("(m.flags & {}) != 0", Flags::FLAGGED),
            TermKind::Is(Status::Vip) => "EXISTS (SELECT 1 FROM vip v WHERE v.address = m.from_addr)".into(),
            TermKind::Is(Status::Notification) => NOTIFICATION.into(),
            TermKind::Is(Status::NeedsReply) => crate::ai::triage::NEEDS_REPLY.into(),
            TermKind::Is(Status::Invite) => {
                let ids = crate::store::unanswered_threads(conn, chrono::Utc::now(), tz::here())?;
                let p = sql.param(serde_json::to_string(&ids.iter().map(|t| t.0).collect::<Vec<_>>()).expect("numbers"));
                format!("m.thread_id IN (SELECT value FROM json_each(?{p}))")
            }
            TermKind::Category(c) => {
                let p = sql.param(c.clone());
                format!("EXISTS (SELECT 1 FROM ai_triage t WHERE t.message_id = m.id AND t.category = ?{p})")
            }
            TermKind::Is(Status::Newsletter) => {
                format!("((m.list_id IS NOT NULL OR m.list_unsubscribe IS NOT NULL) AND NOT {NOTIFICATION})")
            }
            TermKind::Account(v) => {
                let p = sql.param(v.clone());
                format!("m.account_id IN (SELECT id FROM account WHERE id = ?{p} OR address = ?{p} COLLATE NOCASE)")
            }
            TermKind::Thread(id) => format!("m.thread_id = {id}"),
            TermKind::Before(t) => format!("m.id < {}", date_to_id_floor(*t)),
            TermKind::After(t) => format!("m.id >= {}", date_to_id_floor(*t)),
        };
        sql.conds.push(if t.negated { format!("NOT ({cond})") } else { cond });
    }
    Ok(sql)
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

/// What a list is sorted on. Each key has a natural direction: dates newest
/// first, names and subjects A to Z. `reversed` flips it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub reversed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Date,
    /// The name the list shows, or the address when there is no name.
    Sender,
    /// The subject without reply and forward prefixes, as threading reads it.
    Subject,
}

/// Every matching thread in list order, as (thread, newest matching message).
/// One scan gives the count and lets any page be cut out without re-reading.
/// Threads with the same sender or subject stay newest first.
pub fn query_order(conn: &Connection, q: &Query, sort: Sort) -> Result<Vec<(ThreadId, MessageId)>> {
    if sort.key == SortKey::Date {
        let mut hits = scan(conn, q, None)?;
        if sort.reversed {
            hits.reverse();
        }
        return Ok(hits);
    }
    let mut keyed = Vec::new();
    let mut seen = HashSet::new();
    let queries: Vec<Query> = std::iter::once(q.clone()).chain(reminder_queries(conn, q)?).collect();
    for q in &queries {
        for_each_thread(conn, q, ", m.from_name, m.from_addr, m.subject", |thread, message, row| {
            if !seen.insert(thread) {
                return Ok(true);
            }
            let key = match sort.key {
                SortKey::Sender => {
                    let name: Option<String> = row.get(2)?;
                    let shown = match name.filter(|n| !n.trim().is_empty()) {
                        Some(name) => name,
                        None => row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    };
                    sort_text(&shown)
                }
                _ => sort_text(&base_subject(&row.get::<_, Option<String>>(4)?.unwrap_or_default()).0),
            };
            keyed.push((key, (thread, message)));
            Ok(true)
        })?;
    }
    // Empty names and subjects go last in either direction.
    keyed.sort_by(|(a, _), (b, _)| {
        a.is_empty().cmp(&b.is_empty()).then_with(|| if sort.reversed { b.cmp(a) } else { a.cmp(b) })
    });
    Ok(keyed.into_iter().map(|(_, hit)| hit).collect())
}

fn scan(conn: &Connection, q: &Query, stop_after: Option<usize>) -> Result<Vec<(ThreadId, MessageId)>> {
    let mut hits = Vec::new();
    for_each_thread(conn, q, "", |thread, message, _| {
        hits.push((thread, message));
        Ok(Some(hits.len()) != stop_after)
    })?;
    let reminded = reminder_queries(conn, q)?;
    if !reminded.is_empty() {
        let mut seen: HashSet<ThreadId> = hits.iter().map(|h| h.0).collect();
        for q in &reminded {
            for_each_thread(conn, q, "", |thread, message, _| {
                if seen.insert(thread) {
                    hits.push((thread, message));
                }
                Ok(false)
            })?;
        }
        // Message ids grow with the date, so this is newest first again.
        hits.sort_by(|a, b| b.1.cmp(&a.1));
        if let Some(n) = stop_after {
            hits.truncate(n);
        }
    }
    Ok(hits)
}

/// Inbox lists also show threads a fired "remind me if no reply" brought
/// back (`store::reminders`). For each such thread this is the list's query
/// with its inbox term swapped for that thread, so the other terms (a
/// section, a search) still apply.
fn reminder_queries(conn: &Connection, q: &Query) -> Result<Vec<Query>> {
    let inbox = q.terms.iter().position(|t| {
        !t.negated
            && match &t.kind {
                TermKind::In(v) => v.eq_ignore_ascii_case("inbox"),
                TermKind::Mailbox(id) => conn
                    .query_row("SELECT role = 'inbox' FROM mailbox WHERE id = ?1", [id], |r| r.get::<_, Option<bool>>(0))
                    .ok()
                    .flatten()
                    .unwrap_or(false),
                _ => false,
            }
    });
    let Some(i) = inbox else { return Ok(Vec::new()) };
    // One account's inbox shows only that account's reminders.
    let account: Option<AccountId> = match q.terms[i].kind {
        TermKind::Mailbox(id) => Some(conn.query_row("SELECT account_id FROM mailbox WHERE id = ?1", [id], |r| r.get(0))?),
        _ => None,
    };
    let mut out = Vec::new();
    for thread in crate::store::reminded_threads(conn)? {
        if let Some(account) = account {
            let owner: Option<AccountId> = conn
                .query_row("SELECT account_id FROM message WHERE thread_id = ?1 LIMIT 1", [thread], |r| r.get(0))
                .ok();
            if owner != Some(account) {
                continue;
            }
        }
        let mut swapped = q.clone();
        swapped.terms[i].kind = TermKind::Thread(thread.0);
        out.push(swapped);
    }
    Ok(out)
}

/// Walks matching messages newest first and calls `each` once per thread,
/// with its newest matching message and the row (`m.id`, `m.thread_id`, then
/// `extra` columns). Stops when `each` returns false.
fn for_each_thread(
    conn: &Connection,
    q: &Query,
    extra: &str,
    mut each: impl FnMut(ThreadId, MessageId, &rusqlite::Row) -> Result<bool>,
) -> Result<()> {
    let sql = plan(conn, q)?;
    let text = format!(
        "SELECT m.id, m.thread_id{extra} FROM {}{} ORDER BY {}",
        sql.from,
        where_clause(&sql),
        sql.order
    );
    let mut stmt = conn.prepare_cached(&text)?;
    let mut rows = stmt.query(params_from_iter(sql.params.iter()))?;
    let mut seen = HashSet::new();
    while let Some(row) = rows.next()? {
        let thread: ThreadId = row.get(1)?;
        if seen.insert(thread) && !each(thread, row.get(0)?, row)? {
            break;
        }
    }
    Ok(())
}

/// Lowercase with accents dropped, so `Émile` sorts with `emile` and quotes
/// around a display name don't put it first.
pub(crate) fn sort_text(text: &str) -> String {
    const FOLD: [(&str, char); 14] = [
        ("àáâãäåāăą", 'a'), ("çćĉċč", 'c'), ("ďđ", 'd'), ("èéêëēĕėęě", 'e'), ("ìíîïĩīĭįı", 'i'),
        ("ľĺļł", 'l'), ("ñńņňŉ", 'n'), ("òóôõöøōŏő", 'o'), ("ŕŗř", 'r'), ("śŝşš", 's'),
        ("ţťŧ", 't'), ("ùúûüũūŭůűų", 'u'), ("ýÿŷ", 'y'), ("źżž", 'z'),
    ];
    text.trim_start_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_ascii() { c } else { FOLD.iter().find(|(from, _)| from.contains(c)).map_or(c, |&(_, to)| to) })
        .collect()
}

/// Every message the query matches, ignoring threads.
pub fn query_messages(conn: &Connection, q: &Query) -> Result<Vec<MessageId>> {
    let sql = plan(conn, q)?;
    let text = format!("SELECT m.id FROM {}{}", sql.from, where_clause(&sql));
    let mut stmt = conn.prepare_cached(&text)?;
    let ids = stmt.query_map(params_from_iter(sql.params.iter()), |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

pub fn thread_rows(conn: &Connection, hits: &[(ThreadId, MessageId)]) -> Result<Vec<ThreadRow>> {
    let threads: Vec<ThreadId> = hits.iter().map(|h| h.0).collect();
    let invitations = crate::store::thread_invitations(conn, &threads, chrono::Utc::now(), tz::here())?;
    hits.iter().map(|&(t, m)| thread_row(conn, t, m, invitations.get(&t).copied())).collect()
}

/// Number of threads matching the query.
pub fn query_count(conn: &Connection, q: &Query) -> Result<u32> {
    let sql = plan(conn, q)?;
    let text = format!("SELECT count(DISTINCT m.thread_id) FROM {}{}", sql.from, where_clause(&sql));
    Ok(conn.prepare_cached(&text)?.query_row(params_from_iter(sql.params.iter()), |r| r.get(0))?)
}

fn thread_row(conn: &Connection, thread: ThreadId, message: MessageId, invitation: Option<InvitationState>) -> Result<ThreadRow> {
    let mut row = conn.prepare_cached(
        "SELECT account_id, date, from_name, from_addr, subject, snippet,
                EXISTS (SELECT 1 FROM vip v WHERE v.address = message.from_addr)
         FROM message WHERE id = ?1",
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
            vip: r.get(6)?,
            needs_reply: false,
            invitation,
        })
    })?;
    // The conversation's newest message decides, as for `is:needs-reply`.
    row.needs_reply = conn
        .prepare_cached(&format!(
            "SELECT EXISTS (SELECT 1 FROM message m WHERE m.thread_id = ?1 AND {})",
            crate::ai::triage::NEEDS_REPLY
        ))?
        .query_row([thread], |r| r.get(0))?;
    conn.prepare_cached(
        "SELECT count(DISTINCT coalesce(message_id, id)), coalesce(sum((flags & ?2) = 0), 0),
                coalesce(max((flags & ?3) != 0), 0), coalesce(max(has_attachments), 0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_text_folds_case_accents_and_quotes() {
        assert_eq!(sort_text("\"Émile Dupré\""), "emile dupre\"");
        assert_eq!(sort_text("  Ångström"), "angstrom");
        assert!(sort_text("Émile") < sort_text("Frank"));
    }

    #[test]
    fn sorts_by_sender_and_subject_with_ties_newest_first() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id INTEGER PRIMARY KEY, thread_id INTEGER, from_name TEXT, from_addr TEXT, subject TEXT);
             CREATE TABLE mailbox (id INTEGER PRIMARY KEY, role TEXT);
             CREATE TABLE message_mailbox (mailbox_id INTEGER, message_id INTEGER);
             INSERT INTO message VALUES
               (1, 10, 'Zoë', 'z@x', 'Re: Budget'),
               (2, 20, '', 'anna@x', 'Lunch'),
               (3, 30, 'Émile', 'e@x', ''),
               (4, 40, 'anna', 'anna@y', 'Fwd: budget'),
               (5, 20, 'Anna', 'anna@x', 'Re: Lunch');",
        )
        .unwrap();
        let q = parse("").unwrap();
        let threads = |key, reversed| -> Vec<i64> {
            query_order(&conn, &q, Sort { key, reversed }).unwrap().into_iter().map(|(t, _)| t.0).collect()
        };
        assert_eq!(threads(SortKey::Date, false), [20, 40, 30, 10]);
        assert_eq!(threads(SortKey::Date, true), [10, 30, 40, 20]);
        assert_eq!(threads(SortKey::Sender, false), [20, 40, 30, 10]);
        assert_eq!(threads(SortKey::Sender, true), [10, 30, 20, 40]);
        assert_eq!(threads(SortKey::Subject, false), [40, 10, 20, 30]);
        assert_eq!(threads(SortKey::Subject, true), [20, 40, 10, 30]);
    }
}
