//! Sorting new mail: whether a message waits for the user's reply, and
//! which of a few categories it belongs to. One short request per incoming
//! message; the answer is two lines, which the local model is held to with a
//! grammar.

use rusqlite::{Connection, OptionalExtension};

use super::{Ai, Cancel, Finish, Request};
use crate::model::*;
use crate::{Kite, Result, imap};

pub const CATEGORIES: [&str; 5] = ["personal", "work", "finance", "newsletter", "notification"];

/// A message in the list waits for a reply when this holds for it: the
/// model said so, it isn't answered, and it is the newest in its thread.
pub const NEEDS_REPLY: &str = "(EXISTS (SELECT 1 FROM ai_triage t WHERE t.message_id = m.id AND t.needs_reply = 1)
    AND (m.flags & 4) = 0
    AND NOT EXISTS (SELECT 1 FROM message r WHERE r.thread_id = m.thread_id AND r.date > m.date))";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Triage {
    pub category: String,
    pub needs_reply: bool,
}

const SYSTEM: &str = "You sort incoming email for a busy person. Answer in exactly two lines:\n\
category: one of personal, work, finance, newsletter, notification\n\
needs_reply: yes or no\n\n\
personal is friends and family. work is clients, colleagues and projects, written by a person. \
finance is about money: banks, bank statements, bills, invoices, accountants, taxes and payments, \
whether a person or a system sent it. newsletter is subscribed or promotional content. \
notification is other automated mail: build and system alerts, deliveries, shop receipts, confirmations.\n\
needs_reply is yes only when the sender expects an answer or an action from the reader: a question, \
a request, an invitation that needs a response. Automated mail, newsletters and FYI messages are no.";

/// GBNF for the two lines, so the local model can't answer anything else.
const GRAMMAR: &str = r#"root ::= "category: " cat "\nneeds_reply: " yn
cat ::= "personal" | "work" | "finance" | "newsletter" | "notification"
yn ::= "yes" | "no""#;

fn parse(text: &str) -> Option<Triage> {
    let mut category = None;
    let mut needs_reply = None;
    for line in text.lines() {
        let line = line.trim().to_lowercase();
        if let Some(v) = line.strip_prefix("category:") {
            category = CATEGORIES.iter().find(|c| v.trim().starts_with(**c)).map(|c| c.to_string());
        } else if let Some(v) = line.strip_prefix("needs_reply:") {
            needs_reply = Some(v.trim().starts_with("yes"));
        }
    }
    Some(Triage { category: category?, needs_reply: needs_reply? })
}

pub fn triage_of(conn: &Connection, message: MessageId) -> Result<Option<Triage>> {
    Ok(conn
        .query_row("SELECT category, needs_reply FROM ai_triage WHERE message_id = ?1", [message], |r| {
            Ok(Triage { category: r.get(0)?, needs_reply: r.get(1)? })
        })
        .optional()?)
}

/// Incoming inbox messages from the last `days` that have not been sorted,
/// newest first. Mail the user sent is never sorted.
pub fn unsorted_inbox(conn: &Connection, since: i64) -> Result<Vec<MessageId>> {
    let mut stmt = conn.prepare(
        "SELECT m.id FROM message m
         JOIN message_mailbox mm ON mm.message_id = m.id
         JOIN mailbox b ON b.id = mm.mailbox_id AND b.role = 'inbox'
         WHERE m.date >= ?1
           AND NOT EXISTS (SELECT 1 FROM ai_triage t WHERE t.message_id = m.id)
           AND NOT EXISTS (SELECT 1 FROM account a WHERE a.address = m.from_addr COLLATE NOCASE)
         GROUP BY m.id ORDER BY m.date DESC",
    )?;
    let ids = stmt.query_map([since], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Sorts one message and stores the answer. None when it was sorted already,
/// is the user's own, or the request was cancelled.
pub async fn sort(kite: &Kite, ai: &Ai, message: MessageId, cancel: &Cancel) -> Result<Option<Triage>> {
    let (settings, meta) = kite
        .store
        .read_async(move |c| {
            let settings = super::settings(c)?;
            let done = triage_of(c, message)?.is_some();
            let meta: Option<(String, String, String, bool)> = c
                .query_row(
                    "SELECT from_name, from_addr, subject,
                            EXISTS (SELECT 1 FROM account a WHERE a.address = from_addr COLLATE NOCASE)
                     FROM message WHERE id = ?1",
                    [message],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            Ok((settings, if done { None } else { meta }))
        })
        .await?;
    let Some((name, address, subject, mine)) = meta else { return Ok(None) };
    if mine || settings.provider.is_none() {
        return Ok(None);
    }
    let raw = imap::message_source(kite, message).await?;
    let mut body = super::tasks::message_text(&raw);
    if body.len() > 3_000 {
        let mut end = 3_000;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    let headers = crate::mime::parse_envelope(&raw);
    let automated = headers.list_id.is_some() || headers.list_unsubscribe.is_some();
    let request = Request {
        system: SYSTEM.into(),
        user: format!(
            "From: {name} <{address}>\nSubject: {subject}{}\n\n{body}",
            if automated { "\n(sent through a mailing list)" } else { "" }
        ),
        max_tokens: 20,
        temperature: 0.0,
        grammar: Some(GRAMMAR.into()),
    };
    let text = match ai.generate(&settings, &request, cancel, &mut |_| {}).await? {
        Finish::Done(text) => text,
        Finish::Cancelled => return Ok(None),
    };
    let Some(triage) = parse(&text) else {
        return Err(crate::Error::Other(format!("the model's answer couldn't be read: {text:?}")));
    };
    let (stored, model) = (triage.clone(), settings.label());
    kite.store
        .write(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO ai_triage (message_id, category, needs_reply, model, at) VALUES (?1, ?2, ?3, ?4, unixepoch())",
                rusqlite::params![message, stored.category, stored.needs_reply, model],
            )?;
            Ok(())
        })?;
    Ok(Some(triage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_two_lines() {
        assert_eq!(parse("category: work\nneeds_reply: yes"), Some(Triage { category: "work".into(), needs_reply: true }));
        assert_eq!(
            parse("Category: Finance.\nNeeds_reply: No"),
            Some(Triage { category: "finance".into(), needs_reply: false })
        );
        assert_eq!(parse("category: spam\nneeds_reply: no"), None);
        assert_eq!(parse("I think this is work."), None);
    }
}
