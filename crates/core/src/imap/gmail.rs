//! Gmail over IMAP. Jolta syncs only `[Gmail]/All Mail`: every message once,
//! with its labels (X-GM-LABELS) mapped onto the label mailboxes, its stable
//! id (X-GM-MSGID) as remote id and its conversation (X-GM-THRID) as thread.
//! Archive removes the \Inbox label; filing adds a label; Trash is a move.
//! Spam and Trash are not in All Mail and are not synced.

use std::collections::{HashMap, HashSet};

use async_imap::imap_proto::{AttributeValue, MessageSection, Response, SectionPath, Status};
use futures::TryStreamExt;

use super::{Caps, Session, SyncEvent, SyncOptions, SyncReport, fetch_bodies, list_mailboxes, uid_set};
use crate::model::*;
use crate::store::{self, OpKind};
use crate::{Error, Jolta, Result, mime};

const BATCH: usize = 250;
const ITEMS: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE X-GM-MSGID X-GM-THRID X-GM-LABELS BODY.PEEK[HEADER])";

#[derive(Default, Debug)]
struct GmFetch {
    uid: u32,
    flags: Flags,
    date: Option<i64>,
    size: u32,
    msgid: u64,
    thrid: u64,
    labels: Vec<String>,
    header: Vec<u8>,
}

/// "30-Oct-2026 12:27:04 +0000" to unix seconds.
fn internal_date(s: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mut parts = s.trim().split_whitespace();
    let mut date = parts.next()?.split('-');
    let d: i64 = date.next()?.trim().parse().ok()?;
    let month = date.next()?;
    let m = MONTHS.iter().position(|x| *x == month)? as i64 + 1;
    let y: i64 = date.next()?.parse().ok()?;
    let mut time = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (time.next()??, time.next()??, time.next()??);
    let zone = parts.next()?;
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let zh: i64 = zone.get(1..3)?.parse().ok()?;
    let zm: i64 = zone.get(3..5)?.parse().ok()?;
    let days = {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
    };
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss - sign * (zh * 3600 + zm * 60))
}

/// A UID FETCH with the Gmail attributes async-imap's `Fetch` does not expose.
async fn fetch(session: &mut Session, set: &str, items: &str) -> Result<Vec<GmFetch>> {
    let tag = session.run_command(format!("UID FETCH {set} {items}")).await?;
    let mut out = Vec::new();
    loop {
        let Some(resp) = session.read_response().await? else {
            return Err(Error::Other("Gmail closed the connection during FETCH".into()));
        };
        match resp.parsed() {
            Response::Fetch(_, attrs) => {
                let mut f = GmFetch::default();
                for a in attrs {
                    match a {
                        AttributeValue::Uid(u) => f.uid = *u,
                        AttributeValue::Flags(list) => {
                            for flag in list {
                                f.flags = match flag.as_ref() {
                                    "\\Seen" => f.flags.with(Flags::SEEN, true),
                                    "\\Flagged" => f.flags.with(Flags::FLAGGED, true),
                                    "\\Answered" => f.flags.with(Flags::ANSWERED, true),
                                    "\\Draft" => f.flags.with(Flags::DRAFT, true),
                                    _ => f.flags,
                                };
                            }
                        }
                        AttributeValue::InternalDate(d) => f.date = internal_date(d),
                        AttributeValue::Rfc822Size(s) => f.size = *s,
                        AttributeValue::GmailMsgId(id) => f.msgid = *id,
                        AttributeValue::GmailThrId(id) => f.thrid = *id,
                        AttributeValue::GmailLabels(labels) => f.labels = labels.iter().map(|l| unescape(l)).collect(),
                        AttributeValue::BodySection { section: Some(SectionPath::Full(MessageSection::Header)), data: Some(d), .. } => {
                            f.header = d.to_vec()
                        }
                        _ => {}
                    }
                }
                if f.uid != 0 {
                    out.push(f);
                }
            }
            Response::Done { tag: t, status, outcome } if *t == tag => {
                if *status != Status::Ok {
                    let reason = outcome.information.as_deref().unwrap_or("no reason given");
                    return Err(Error::Other(format!("Gmail FETCH failed: {reason}")));
                }
                return Ok(out);
            }
            _ => {}
        }
    }
}

/// Gmail sends labels as quoted strings (`"\\\\Inbox"`) and the parser keeps
/// the escapes; undo them.
fn unescape(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Label names as Gmail spells them in X-GM-LABELS, mapped to mailboxes.
fn label_map(mailboxes: &[Mailbox], important: Option<&str>) -> HashMap<String, MailboxId> {
    let mut map = HashMap::new();
    for m in mailboxes {
        map.insert(m.remote_id.clone(), m.id);
        let system = match m.role {
            Some(MailboxRole::Inbox) => Some("\\Inbox"),
            Some(MailboxRole::Sent) => Some("\\Sent"),
            Some(MailboxRole::Drafts) => Some("\\Draft"),
            _ if Some(m.remote_id.as_str()) == important => Some("\\Important"),
            _ => None,
        };
        if let Some(s) = system {
            map.insert(s.to_string(), m.id);
        }
    }
    map
}

/// The X-GM-LABELS name for a mailbox, if adding or removing it is a label change.
fn label_of(m: &Mailbox) -> Option<String> {
    match m.role {
        Some(MailboxRole::Inbox) => Some("\\Inbox".into()),
        Some(MailboxRole::Sent) => Some("\\Sent".into()),
        Some(MailboxRole::Drafts) => Some("\\Draft".into()),
        Some(MailboxRole::All | MailboxRole::Trash | MailboxRole::Spam) => None,
        _ if m.remote_id.ends_with("/Important") => Some("\\Important".into()),
        _ => Some(m.remote_id.clone()),
    }
}

fn quote(label: &str) -> String {
    if label.starts_with('\\') { label.to_string() } else { format!("\"{}\"", label.replace('\\', "\\\\").replace('"', "\\\"")) }
}

pub async fn sync(
    jolta: &Jolta,
    session: &mut Session,
    caps: Caps,
    account: &Account,
    opts: SyncOptions,
    progress: &mut impl FnMut(SyncEvent),
) -> Result<SyncReport> {
    let account_id = account.id;
    let remote = list_mailboxes(session).await?;
    let important = remote.iter().find(|m| m.important).map(|m| m.remote_id.clone());
    let mailboxes = jolta.store.write_async(move |tx| store::sync_mailbox_list(tx, account_id, &remote)).await?;
    progress(SyncEvent::Mailboxes(mailboxes.len()));
    let all = mailboxes
        .iter()
        .find(|m| m.role == Some(MailboxRole::All))
        .cloned()
        .ok_or_else(|| Error::Other("this Gmail account shows no All Mail folder; is it enabled for IMAP?".into()))?;
    let labels = label_map(&mailboxes, important.as_deref());
    let label_ids = |names: &[String]| -> Vec<MailboxId> { names.iter().filter_map(|l| labels.get(l).copied()).collect() };

    let selected = if caps.condstore { session.select_condstore(&all.remote_id).await? } else { session.select(&all.remote_id).await? };
    let all_id = all.id;
    let mut since = all.highest_modseq;
    if all.uidvalidity.is_some() && all.uidvalidity != selected.uid_validity {
        jolta.store.write_async(move |tx| store::clear_mailbox(tx, all_id)).await?;
        since = None;
    }
    let local = jolta.store.read_async(move |c| store::mailbox_uids(c, all_id)).await?;
    let server: HashSet<u32> = if selected.exists == 0 { HashSet::new() } else { session.uid_search("ALL").await? };

    let mut report = SyncReport { mailboxes: mailboxes.len(), ..Default::default() };
    let gone: Vec<MessageId> = local.iter().filter(|(u, _)| !server.contains(u)).map(|(_, (id, _))| *id).collect();
    report.removed = gone.len();
    if !gone.is_empty() {
        jolta.store.write_async(move |tx| store::delete_messages(tx, &gone)).await?;
    }
    // A message in no All Mail, Trash or Spam mailbox left Gmail (trashed
    // from All Mail, deleted elsewhere) and only its label links remain.
    // Skipped while changes wait to be sent, which may still place it.
    let orphans = jolta
        .store
        .write_async(move |tx| {
            if store::has_pending_ops(tx, account_id)? {
                return Ok(0);
            }
            let ids: Vec<MessageId> = tx
                .prepare(
                    "SELECT m.id FROM message m WHERE m.account_id = ?1 AND NOT EXISTS (
                         SELECT 1 FROM message_mailbox mm JOIN mailbox b ON b.id = mm.mailbox_id
                         WHERE mm.message_id = m.id AND b.role IN ('all', 'trash', 'spam'))",
                )?
                .query_map([account_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            store::delete_messages(tx, &ids)?;
            Ok(ids.len())
        })
        .await?;
    report.removed += orphans;

    let mut new: Vec<u32> = server.iter().copied().filter(|u| !local.contains_key(u)).collect();
    new.sort_unstable_by(|a, b| b.cmp(a));
    let total = new.len();
    for (i, chunk) in new.chunks(BATCH).enumerate() {
        let fetched = fetch(session, &uid_set(chunk), ITEMS).await?;
        let rows: Vec<(NewMessage, Vec<MailboxId>)> = fetched
            .into_iter()
            .map(|f| {
                let date = f.date.or_else(|| mime::header_date(&f.header)).unwrap_or(0);
                let message = NewMessage {
                    account_id,
                    remote_id: format!("gm:{}", f.msgid),
                    mailbox_id: all_id,
                    uid: Some(f.uid),
                    date,
                    envelope: mime::parse_envelope(&f.header),
                    flags: f.flags,
                    size: f.size,
                    thread_hint: Some(format!("gm:{}", f.thrid)),
                    body: None,
                };
                (message, label_ids(&f.labels))
            })
            .collect();
        let ids = jolta
            .store
            .write_async(move |tx| {
                let mut ids = Vec::new();
                for (m, labels) in &rows {
                    let inserted = store::insert_message(tx, m)?;
                    store::set_labels(tx, inserted.id(), all_id, labels)?;
                    if let store::Inserted::New(id) = inserted {
                        ids.push(id);
                    }
                }
                Ok(ids)
            })
            .await?;
        report.new += ids.len();
        report.new_messages.extend(ids);
        progress(SyncEvent::Headers { mailbox: all.name.clone(), done: ((i + 1) * BATCH).min(total), total });
    }

    // Flag and label changes on messages Jolta already has.
    let known: HashMap<u32, (MessageId, Flags)> = local.into_iter().filter(|(u, _)| server.contains(u)).collect();
    if !known.is_empty() {
        let fetched = match (caps.condstore, since) {
            (true, Some(m)) => fetch(session, "1:*", &format!("(UID FLAGS X-GM-LABELS) (CHANGEDSINCE {m})")).await?,
            _ => {
                let mut uids: Vec<u32> = known.keys().copied().collect();
                uids.sort_unstable_by(|a, b| b.cmp(a));
                uids.truncate(5000);
                fetch(session, &uid_set(&uids), "(UID FLAGS X-GM-LABELS)").await?
            }
        };
        let changes: Vec<(MessageId, Flags, Vec<MailboxId>)> = fetched
            .into_iter()
            .filter_map(|f| known.get(&f.uid).map(|(id, _)| (*id, f.flags, label_ids(&f.labels))))
            .collect();
        report.flag_changes = changes.len();
        jolta.store
            .write_async(move |tx| {
                let pending = store::pending_messages(tx, account_id)?;
                for (id, flags, labels) in changes.iter().filter(|(id, ..)| !pending.contains(id)) {
                    store::set_flags(tx, *id, *flags)?;
                    store::set_labels(tx, *id, all_id, labels)?;
                }
                Ok(())
            })
            .await?;
    }
    let (uidvalidity, modseq) = (selected.uid_validity, selected.highest_modseq);
    jolta.store.write_async(move |tx| store::set_mailbox_cursor(tx, all_id, uidvalidity, modseq)).await?;
    progress(SyncEvent::Mailbox { name: all.name.clone(), new: report.new, removed: report.removed, flag_changes: report.flag_changes });

    if opts.bodies {
        report.bodies = fetch_bodies(jolta, session, account, progress).await?;
    }
    Ok(report)
}

/// Sends one op to Gmail. Everything happens in All Mail by UID: flags with
/// STORE, label moves with X-GM-LABELS, and Trash with MOVE.
pub async fn apply_op(jolta: &Jolta, session: &mut Session, op: &store::Op) -> Result<Vec<MailboxId>> {
    let account = op.account;
    let mailboxes = jolta.store.read_async(move |c| store::mailboxes(c, account)).await?;
    let all = mailboxes
        .iter()
        .find(|m| m.role == Some(MailboxRole::All))
        .cloned()
        .ok_or_else(|| Error::Other("no All Mail folder".into()))?;
    let ids: Vec<MessageId> = op.messages.iter().map(|(m, _)| *m).collect();
    let all_id = all.id;
    // A move out of All Mail already dropped the local All Mail entry; the
    // op kept its UID there.
    let uids: Vec<u32> = match op.kind {
        OpKind::Move { from, .. } if from == all_id => op.messages.iter().filter_map(|(_, u)| *u).collect(),
        _ => {
            jolta.store
                .read_async(move |c| {
                    Ok(store::server_locations(c, &ids)?.into_iter().filter(|(m, _)| *m == all_id).map(|(_, u)| u).collect())
                })
                .await?
        }
    };
    if uids.is_empty() {
        return Ok(vec![]);
    }
    session.select(&all.remote_id).await?;
    let set = uid_set(&uids);
    let find = |id: MailboxId| mailboxes.iter().find(|m| m.id == id).cloned();
    match op.kind {
        OpKind::Flags { add, remove } => {
            for (sign, bits) in [("+", add), ("-", remove)] {
                if bits != 0 {
                    session
                        .uid_store(&set, format!("{sign}FLAGS.SILENT ({})", super::flag_list(bits)))
                        .await?
                        .try_collect::<Vec<_>>()
                        .await?;
                }
            }
            Ok(vec![])
        }
        OpKind::Empty { .. } => unreachable!("imap::apply_op empties folders for Gmail too"),
        OpKind::Move { from, to } => {
            let (from, to) = (find(from), find(to));
            if let Some(t) = to.as_ref().filter(|t| t.role == Some(MailboxRole::Trash)) {
                session.uid_mv(&set, &t.remote_id).await?;
                return Ok(vec![]);
            }
            if let Some(label) = to.as_ref().and_then(label_of) {
                let add = format!("+X-GM-LABELS ({})", quote(&label));
                let stored: std::result::Result<Vec<_>, _> = match session.uid_store(&set, &add).await {
                    Ok(updates) => updates.try_collect().await,
                    Err(e) => Err(e),
                };
                if let Err(e) = stored {
                    // A first snooze: the Snoozed label does not exist yet.
                    let target = to.as_ref().map(|t| t.remote_id.clone()).unwrap_or_default();
                    if !e.to_string().to_uppercase().contains("NONEXISTENT") && !e.to_string().to_uppercase().contains("TRYCREATE") {
                        return Err(e.into());
                    }
                    session.create(&target).await?;
                    session.select(&all.remote_id).await?;
                    session.uid_store(&set, &add).await?.try_collect::<Vec<_>>().await?;
                }
            }
            if let Some(label) = from.as_ref().and_then(label_of) {
                session.uid_store(&set, format!("-X-GM-LABELS ({})", quote(&label))).await?.try_collect::<Vec<_>>().await?;
            }
            Ok(vec![all_id])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_internal_dates() {
        assert_eq!(internal_date("30-Oct-2026 12:27:04 +0000"), Some(1_793_363_224));
        assert_eq!(internal_date(" 1-Jan-1970 01:00:00 +0100"), Some(0));
        assert_eq!(internal_date("nonsense"), None);
    }

    #[test]
    fn unescapes_labels() {
        assert_eq!(unescape("\\\\Inbox"), "\\Inbox");
        assert_eq!(unescape("Work"), "Work");
        assert_eq!(unescape("a\\\"b"), "a\"b");
    }

    #[test]
    fn quotes_labels() {
        assert_eq!(quote("\\Inbox"), "\\Inbox");
        assert_eq!(quote("Work \"A\""), "\"Work \\\"A\\\"\"");
    }
}
