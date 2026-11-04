//! Opening a thread for the reader pane.

use rusqlite::Connection;

use crate::calendar::card::{self, Card};
use crate::calendar::{invitation, tz};
use crate::model::*;
use crate::render::{self, MessageView, Rendered};
use crate::smime::{self, Protection};
use crate::{Jolta, Result, imap, store};

#[derive(Clone, Debug)]
pub struct ThreadMessage {
    pub id: MessageId,
    pub account: AccountId,
    pub from_name: String,
    pub from_addr: String,
    pub to: String,
    pub cc: String,
    pub date: i64,
    pub snippet: String,
    pub flags: Flags,
    pub vip: bool,
}

#[derive(Clone, Debug)]
pub struct ThreadDetail {
    pub thread_id: ThreadId,
    pub subject: String,
    pub messages: Vec<ThreadMessage>,
    pub rendered: Rendered,
    /// Who sent the remote images, when one sender sent them all.
    pub image_sender: Option<String>,
    /// Whether every message with remote images comes from a trusted sender.
    pub images_trusted: bool,
}

pub fn thread_messages(conn: &Connection, thread: ThreadId) -> Result<(String, Vec<ThreadMessage>)> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, from_name, from_addr, to_addrs, cc_addrs, date, snippet, flags, subject, message_id,
                EXISTS (SELECT 1 FROM vip v WHERE v.address = message.from_addr), account_id
         FROM message WHERE thread_id = ?1 ORDER BY id",
    )?;
    let mut subject = String::new();
    let rows = stmt.query_map([thread], |r| {
        Ok((
            ThreadMessage {
                id: r.get(0)?,
                account: r.get(11)?,
                from_name: r.get(1)?,
                from_addr: r.get(2)?,
                to: r.get(3)?,
                cc: r.get(4)?,
                date: r.get(5)?,
                snippet: r.get(6)?,
                flags: Flags(r.get(7)?),
                vip: r.get(10)?,
            },
            r.get::<_, String>(8)?,
            r.get::<_, Option<String>>(9)?,
        ))
    })?;
    let mut out = Vec::new();
    // The same message can be stored twice (the received copy and the one
    // in Sent); show it once.
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let (m, s, header_id) = row?;
        if let Some(h) = header_id {
            if !seen.insert(h) {
                continue;
            }
        }
        if subject.is_empty() {
            subject = s;
        }
        out.push(m);
    }
    Ok((subject, out))
}

/// `29 Oct 2026, 14:05` in the viewer's time zone.
pub fn format_date(unix: i64, utc_offset: i32) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let t = unix + utc_offset as i64;
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{d} {} {y}, {:02}:{:02}", MONTHS[(m - 1) as usize], secs / 3600, secs % 3600 / 60)
}

/// Loads every message of a thread (downloading bodies the store lacks) and
/// renders them. Unread messages and the newest one start expanded.
pub async fn open_thread(jolta: &Jolta, thread: ThreadId, app_css: &str, utc_offset: i32) -> Result<ThreadDetail> {
    let (subject, messages) = jolta.store.read_async(move |c| thread_messages(c, thread)).await?;
    let mut raws = Vec::with_capacity(messages.len());
    for m in &messages {
        raws.push(imap::message_source(jolta, m.id).await.unwrap_or_default());
    }
    let names: Vec<(String, String)> = messages
        .iter()
        .map(|m| {
            let from = if m.from_name.is_empty() { m.from_addr.clone() } else { m.from_name.clone() };
            (from, format_date(m.date, utc_offset))
        })
        .collect();
    let protections: Vec<Protection> = messages.iter().zip(&raws).map(|(m, raw)| smime::inspect(raw, &m.from_addr)).collect();
    let cards = invitation_cards(jolta, &messages, &raws).await?;
    let last = messages.len().saturating_sub(1);
    let views: Vec<MessageView> = messages
        .iter()
        .enumerate()
        .map(|(i, m)| MessageView {
            id: m.id.0,
            from: &names[i].0,
            to: &m.to,
            date: &names[i].1,
            snippet: &m.snippet,
            raw: &raws[i],
            open: i == last || !m.flags.has(Flags::SEEN),
            vip: m.vip,
            protection: &protections[i],
            cards: &cards[i],
        })
        .collect();
    let rendered = render::render_thread(&views, app_css);
    let mut senders: Vec<String> = messages
        .iter()
        .zip(&rendered.remote_by_message)
        .filter(|(m, n)| **n > 0 && !m.from_addr.is_empty())
        .map(|(m, _)| m.from_addr.to_ascii_lowercase())
        .collect();
    senders.sort_unstable();
    senders.dedup();
    let check = senders.clone();
    let images_trusted = !check.is_empty()
        && jolta
            .store
            .read_async(move |c| check.iter().try_fold(true, |all, a| Ok(all && store::is_sender_trusted(c, a)?)))
            .await?;
    let image_sender = (senders.len() == 1).then(|| senders.remove(0));
    Ok(ThreadDetail { thread_id: thread, subject, messages, rendered, image_sender, images_trusted })
}

/// The invitation cards of each message, empty for most.
async fn invitation_cards(jolta: &Jolta, messages: &[ThreadMessage], raws: &[Vec<u8>]) -> Result<Vec<Vec<Card>>> {
    let me: Vec<String> = jolta.store.read_async(|c| Ok(store::accounts(c)?.into_iter().map(|a| a.address).collect())).await?;
    let found: Vec<(MessageId, AccountId, String, Vec<invitation::Invitation>)> =
        messages.iter().zip(raws).map(|(m, raw)| (m.id, m.account, m.from_addr.clone(), invitation::find(raw, &me))).collect();
    if found.iter().all(|(.., i)| i.is_empty()) {
        return Ok(vec![Vec::new(); messages.len()]);
    }
    jolta
        .store
        .read_async(move |c| {
            let (now, here) = (chrono::Utc::now(), tz::here());
            found
                .iter()
                .map(|(message, account, sender, invitations)| {
                    card::cards(c, invitations, &card::Context { message: *message, account: *account, me: &me, sender, now, here })
                })
                .collect()
        })
        .await
}
