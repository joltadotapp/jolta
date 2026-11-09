//! Gmail over IMAP. Gmail keeps every message in one of `[Gmail]/All Mail`,
//! Trash and Spam, and Jolta syncs those three: every message once, with
//! its labels (X-GM-LABELS) mapped onto the label mailboxes, its stable id
//! (X-GM-MSGID) as remote id and its conversation (X-GM-THRID) as thread.
//! Archive removes the \Inbox label; filing adds a label; Trash is a move.

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

/// The STORE items for a move between labels: the label added, then the
/// one taken off. All Mail is no label, so adding a label (a move out of All
/// Mail) only adds one, and archiving or taking a label off (a move into it)
/// only takes one off.
fn label_stores(from: Option<&Mailbox>, to: Option<&Mailbox>) -> (Option<String>, Option<String>) {
    let add = to.and_then(label_of).map(|l| format!("+X-GM-LABELS ({})", quote(&l)));
    let remove = from.and_then(label_of).map(|l| format!("-X-GM-LABELS ({})", quote(&l)));
    (add, remove)
}

fn quote(label: &str) -> String {
    if label.starts_with('\\') { label.to_string() } else { format!("\"{}\"", label.replace('\\', "\\\\").replace('"', "\\\"")) }
}

/// Gmail keeps each message in exactly one of All Mail, Trash and Spam, with
/// a UID there; labels are links of All Mail's messages.
fn is_home(m: &Mailbox) -> bool {
    matches!(m.role, Some(MailboxRole::All | MailboxRole::Trash | MailboxRole::Spam))
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
    if !mailboxes.iter().any(|m| m.role == Some(MailboxRole::All)) {
        return Err(Error::Other("this Gmail account shows no All Mail folder; is it enabled for IMAP?".into()));
    }
    let labels = label_map(&mailboxes, important.as_deref());
    // All Mail first, so the inbox fills first.
    let mut homes: Vec<&Mailbox> = mailboxes.iter().filter(|m| is_home(m) && m.selectable).collect();
    homes.sort_by_key(|m| m.role != Some(MailboxRole::All));

    let mut report = SyncReport { mailboxes: mailboxes.len(), ..Default::default() };
    for home in homes {
        sync_home(jolta, session, caps, account_id, home, &labels, &mut report, progress).await?;
    }
    // A message in no All Mail, Trash or Spam mailbox left Gmail (deleted
    // for good, or elsewhere) and only its label links remain. Skipped while
    // changes wait to be sent, which may still place it.
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

    if opts.bodies {
        report.bodies = fetch_bodies(jolta, session, account, progress).await?;
    }
    Ok(report)
}

/// Brings one home mailbox in line with Gmail. A message there gets that
/// home with its UID, and in All Mail its labels; one that left it loses
/// them and keeps its place in another home.
#[allow(clippy::too_many_arguments)]
async fn sync_home(
    jolta: &Jolta,
    session: &mut Session,
    caps: Caps,
    account_id: AccountId,
    home: &Mailbox,
    labels: &HashMap<String, MailboxId>,
    report: &mut SyncReport,
    progress: &mut impl FnMut(SyncEvent),
) -> Result<()> {
    let in_all = home.role == Some(MailboxRole::All);
    let label_ids =
        |names: &[String]| -> Vec<MailboxId> { if in_all { names.iter().filter_map(|l| labels.get(l).copied()).collect() } else { vec![] } };
    let selected = if caps.condstore { session.select_condstore(&home.remote_id).await? } else { session.select(&home.remote_id).await? };
    let home_id = home.id;
    let mut since = home.highest_modseq;
    if home.uidvalidity.is_some() && home.uidvalidity != selected.uid_validity {
        jolta.store.write_async(move |tx| store::clear_mailbox(tx, home_id)).await?;
        since = None;
    }
    let local = jolta.store.read_async(move |c| store::mailbox_uids(c, home_id)).await?;
    let server: HashSet<u32> = if selected.exists == 0 { HashSet::new() } else { session.uid_search("ALL").await? };

    let gone: Vec<MessageId> = local.iter().filter(|(u, _)| !server.contains(u)).map(|(_, (id, _))| *id).collect();
    let removed = gone.len();
    if !gone.is_empty() {
        jolta
            .store
            .write_async(move |tx| {
                let mut unlink = tx.prepare(
                    "DELETE FROM message_mailbox WHERE message_id = ?1 AND (mailbox_id = ?2 OR mailbox_id NOT IN (
                         SELECT id FROM mailbox WHERE account_id = ?3 AND role IN ('all', 'trash', 'spam')))",
                )?;
                for id in &gone {
                    unlink.execute(rusqlite::params![id, home_id, account_id])?;
                }
                Ok(())
            })
            .await?;
    }

    let mut new: Vec<u32> = server.iter().copied().filter(|u| !local.contains_key(u)).collect();
    new.sort_unstable_by(|a, b| b.cmp(a));
    let total = new.len();
    let mut added = 0;
    for (i, chunk) in new.chunks(BATCH).enumerate() {
        let fetched = fetch(session, &uid_set(chunk), ITEMS).await?;
        let rows: Vec<(NewMessage, Vec<MailboxId>)> = fetched
            .into_iter()
            .map(|f| {
                let date = f.date.or_else(|| mime::header_date(&f.header)).unwrap_or(0);
                let message = NewMessage {
                    account_id,
                    remote_id: format!("gm:{}", f.msgid),
                    mailbox_id: home_id,
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
                    store::set_labels(tx, inserted.id(), home_id, labels)?;
                    if let store::Inserted::New(id) = inserted {
                        ids.push(id);
                    }
                }
                Ok(ids)
            })
            .await?;
        added += ids.len();
        report.new_messages.extend(ids);
        progress(SyncEvent::Headers { mailbox: home.name.clone(), done: ((i + 1) * BATCH).min(total), total });
    }

    // Flag and label changes on messages Jolta already has.
    let known: HashMap<u32, (MessageId, Flags)> = local.into_iter().filter(|(u, _)| server.contains(u)).collect();
    let mut flag_changes = 0;
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
        flag_changes = changes.len();
        jolta.store
            .write_async(move |tx| {
                let pending = store::pending_messages(tx, account_id)?;
                for (id, flags, labels) in changes.iter().filter(|(id, ..)| !pending.contains(id)) {
                    store::set_flags(tx, *id, *flags)?;
                    store::set_labels(tx, *id, home_id, labels)?;
                }
                Ok(())
            })
            .await?;
    }
    let (uidvalidity, modseq) = (selected.uid_validity, selected.highest_modseq);
    jolta.store.write_async(move |tx| store::set_mailbox_cursor(tx, home_id, uidvalidity, modseq)).await?;
    report.new += added;
    report.removed += removed;
    report.flag_changes += flag_changes;
    progress(SyncEvent::Mailbox { name: home.name.clone(), new: added, removed, flag_changes });
    Ok(())
}

/// Sends one op to Gmail, by UID in each message's home: flags with STORE,
/// a move into or out of Trash or Spam with MOVE, and other moves as label
/// changes in All Mail with X-GM-LABELS.
pub async fn apply_op(jolta: &Jolta, session: &mut Session, op: &store::Op) -> Result<Vec<MailboxId>> {
    let account = op.account;
    let mailboxes = jolta.store.read_async(move |c| store::mailboxes(c, account)).await?;
    let all = mailboxes
        .iter()
        .find(|m| m.role == Some(MailboxRole::All))
        .cloned()
        .ok_or_else(|| Error::Other("no All Mail folder".into()))?;
    let find = |id: MailboxId| mailboxes.iter().find(|m| m.id == id).cloned();
    let home_of = |id: MailboxId| find(id).filter(is_home);

    let mut by_home: HashMap<MailboxId, Vec<u32>> = HashMap::new();
    match op.kind {
        // A move out of a home already dropped the local link there; the op
        // kept its UID. A message moved in before a sync learned its UID
        // there, as when Undo follows a trash, is found by its Gmail id.
        OpKind::Move { from, .. } if home_of(from).is_some() => {
            let mut uids: Vec<u32> = op.messages.iter().filter_map(|(_, u)| *u).collect();
            let missing: Vec<MessageId> = op.messages.iter().filter(|(_, u)| u.is_none()).map(|(m, _)| *m).collect();
            if !missing.is_empty() {
                let gmail_ids: Vec<String> = jolta
                    .store
                    .read_async(move |c| {
                        let mut stmt = c.prepare("SELECT remote_id FROM message WHERE id = ?1")?;
                        let mut out = Vec::new();
                        for id in &missing {
                            let remote: String = stmt.query_row([id], |r| r.get(0))?;
                            out.extend(remote.strip_prefix("gm:").map(str::to_string));
                        }
                        Ok(out)
                    })
                    .await?;
                if !gmail_ids.is_empty() {
                    let path = home_of(from).map(|m| m.remote_id).unwrap_or_default();
                    session.select(&path).await?;
                    for id in gmail_ids {
                        uids.extend(session.uid_search(format!("X-GM-MSGID {id}")).await?);
                    }
                }
            }
            by_home.insert(from, uids);
        }
        _ => {
            let ids: Vec<MessageId> = op.messages.iter().map(|(m, _)| *m).collect();
            for (mailbox, uid) in jolta.store.read_async(move |c| store::server_locations(c, &ids)).await? {
                if home_of(mailbox).is_some() {
                    by_home.entry(mailbox).or_default().push(uid);
                }
            }
        }
    }

    let mut touched = Vec::new();
    for (home, uids) in by_home.into_iter().filter(|(_, uids)| !uids.is_empty()) {
        let path = home_of(home).map(|m| m.remote_id).unwrap_or_default();
        session.select(&path).await?;
        let set = uid_set(&uids);
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
            }
            OpKind::Empty { .. } => unreachable!("imap::apply_op empties folders for Gmail too"),
            OpKind::Move { from, to } => {
                let (from, to) = (find(from), find(to));
                touched.push(all.id);
                if home != all.id || to.as_ref().is_some_and(is_home) {
                    let target = to.ok_or_else(|| Error::NotFound("the mailbox to move to".into()))?;
                    session.uid_mv(&set, &target.remote_id).await?;
                    continue;
                }
                let (add, remove) = label_stores(from.as_ref(), to.as_ref());
                if let Some(add) = add {
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
                if let Some(remove) = remove {
                    session.uid_store(&set, remove).await?.try_collect::<Vec<_>>().await?;
                }
            }
        }
    }
    Ok(touched)
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
    fn label_changes_become_x_gm_labels_stores() {
        let mailbox = |id, remote: &str, role| Mailbox {
            id: MailboxId(id),
            account_id: AccountId(1),
            remote_id: remote.into(),
            name: remote.into(),
            role,
            delimiter: Some("/".into()),
            selectable: true,
            uidvalidity: None,
            highest_modseq: None,
            hidden: false,
            synced: true,
        };
        let all = mailbox(1, "[Gmail]/All Mail", Some(MailboxRole::All));
        let inbox = mailbox(2, "INBOX", Some(MailboxRole::Inbox));
        let work = mailbox(3, "Clients/Acme \"B\"", None);
        let stores = |from: &Mailbox, to: &Mailbox| label_stores(Some(from), Some(to));
        assert_eq!(stores(&all, &work), (Some(r#"+X-GM-LABELS ("Clients/Acme \"B\"")"#.into()), None), "adding a label");
        assert_eq!(stores(&work, &all), (None, Some(r#"-X-GM-LABELS ("Clients/Acme \"B\"")"#.into())), "taking it off");
        assert_eq!(stores(&inbox, &all), (None, Some("-X-GM-LABELS (\\Inbox)".into())), "archiving");
        assert_eq!(stores(&inbox, &work), (Some(r#"+X-GM-LABELS ("Clients/Acme \"B\"")"#.into()), Some("-X-GM-LABELS (\\Inbox)".into())), "moving");
    }

    #[test]
    fn quotes_labels() {
        assert_eq!(quote("\\Inbox"), "\\Inbox");
        assert_eq!(quote("Work \"A\""), "\"Work \\\"A\\\"\"");
    }

    use super::super::fake_gmail::{FakeGmail, SPAM, TRASH};
    use crate::actions::{self, Action, Context};

    async fn signed_in(name: &str, server: &FakeGmail) -> (Jolta, Account, Session) {
        let dir = std::env::temp_dir().join(format!("jolta-gmail-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let jolta = Jolta::open(&dir).unwrap();
        let new = store::NewAccount {
            kind: AccountKind::Gmail,
            address: "g@test.invalid".into(),
            display_name: String::new(),
            username: "g@test.invalid".into(),
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 0,
        };
        let account = jolta.store.write(|tx| store::insert_account(tx, &new)).unwrap();
        (jolta, account, server.connect().await)
    }

    async fn sync_once(jolta: &Jolta, session: &mut Session, account: &Account) {
        sync(jolta, session, Caps::default(), account, SyncOptions { bodies: false }, &mut |_| {}).await.unwrap();
    }

    /// The subjects the app's list shows for a query.
    fn list(jolta: &Jolta, query: &str) -> Vec<String> {
        let q = crate::search::parse(query).unwrap();
        let mut out: Vec<String> =
            jolta.store.read(|c| crate::search::query_page(c, &q, 0, 50)).unwrap().into_iter().map(|r| r.subject).collect();
        out.sort();
        out
    }

    fn thread(jolta: &Jolta, subject: &str) -> ThreadId {
        jolta.store.read(|c| Ok(c.query_row("SELECT thread_id FROM message WHERE subject = ?1", [subject], |r| r.get(0))?)).unwrap()
    }

    fn mailbox(jolta: &Jolta, account: &Account, role: MailboxRole) -> MailboxId {
        let id = account.id;
        jolta.store.read(|c| store::mailboxes(c, id)).unwrap().into_iter().find(|m| m.role == Some(role)).unwrap().id
    }

    /// Sends the queued ops, as the engine does before it syncs.
    async fn send_ops(jolta: &Jolta, session: &mut Session, account: &Account) {
        let id = account.id;
        while let Some(op) = jolta.store.write(|tx| store::claim_op(tx, id)).unwrap() {
            super::super::apply_op(jolta, session, Caps::default(), &op).await.unwrap();
            jolta.store.write(|tx| store::finish_op(tx, op.id)).unwrap();
        }
    }

    async fn act(jolta: &Jolta, session: &mut Session, account: &Account, subject: &str, action: Action, context: Context) -> store::OpId {
        let thread = thread(jolta, subject);
        let (_, op) = jolta.store.write(|tx| actions::perform(tx, thread, action, context)).unwrap().expect("something to change");
        send_ops(jolta, session, account).await;
        op
    }

    /// Found on 2026-11-09: a conversation trashed in Jolta reached Gmail's
    /// Trash, then left Jolta at the next sync, since Gmail's Trash wasn't
    /// synced and the message had left All Mail.
    #[tokio::test]
    async fn a_conversation_trashed_in_jolta_stays_in_trash() {
        let server = FakeGmail::default();
        server.deliver("Bin me", &["\\Inbox"]);
        server.deliver("Keep", &["\\Inbox", "Work"]);
        let (jolta, account, mut session) = signed_in("trashed-here", &server).await;
        sync_once(&jolta, &mut session, &account).await;
        assert_eq!(list(&jolta, "in:inbox"), ["Bin me", "Keep"]);

        act(&jolta, &mut session, &account, "Bin me", Action::Trash, Context::Inbox).await;
        assert_eq!(server.subjects(TRASH), ["Bin me"], "Gmail has it in Trash");
        sync_once(&jolta, &mut session, &account).await;
        assert_eq!(list(&jolta, "in:trash"), ["Bin me"], "and so does Jolta after the next sync");
        assert_eq!(list(&jolta, "in:inbox"), ["Keep"]);
        assert_eq!(list(&jolta, "in:Work"), ["Keep"]);
    }

    /// Mail trashed or marked as spam on the web shows in Jolta's Trash and
    /// Spam, and reading it there reaches Gmail.
    #[tokio::test]
    async fn trash_and_spam_from_elsewhere_arrive() {
        let server = FakeGmail::default();
        for subject in ["Bin me", "Junk", "Keep"] {
            server.deliver(subject, &["\\Inbox"]);
        }
        let (jolta, account, mut session) = signed_in("trashed-elsewhere", &server).await;
        sync_once(&jolta, &mut session, &account).await;
        server.move_to("Bin me", TRASH);
        server.move_to("Junk", SPAM);
        sync_once(&jolta, &mut session, &account).await;
        assert_eq!(list(&jolta, "in:trash"), ["Bin me"]);
        assert_eq!(list(&jolta, "in:spam"), ["Junk"]);
        assert_eq!(list(&jolta, "in:inbox"), ["Keep"]);

        act(&jolta, &mut session, &account, "Bin me", Action::MarkRead, Context::Role(MailboxRole::Trash)).await;
        assert!(server.seen("Bin me"), "read in Trash reached Gmail");
        sync_once(&jolta, &mut session, &account).await;
        assert!(list(&jolta, "in:trash is:unread").is_empty(), "and stays read");
    }

    /// Moving a conversation out of Trash brings it back on Gmail too, also
    /// when Undo follows the trash before a sync learned its Trash UID.
    #[tokio::test]
    async fn back_out_of_trash() {
        let server = FakeGmail::default();
        server.deliver("Oops", &["\\Inbox"]);
        server.deliver("Undo me", &["\\Inbox"]);
        let (jolta, account, mut session) = signed_in("back-out", &server).await;
        sync_once(&jolta, &mut session, &account).await;
        let inbox = mailbox(&jolta, &account, MailboxRole::Inbox);

        server.move_to("Oops", TRASH);
        sync_once(&jolta, &mut session, &account).await;
        act(&jolta, &mut session, &account, "Oops", Action::MoveTo(inbox), Context::Role(MailboxRole::Trash)).await;

        let trashed = act(&jolta, &mut session, &account, "Undo me", Action::Trash, Context::Inbox).await;
        assert_eq!(server.subjects(TRASH), ["Undo me"]);
        let undo = jolta.store.write(|tx| actions::undo(tx, trashed)).unwrap();
        assert!(undo.is_some(), "a sent trash is undone by the opposite move");
        send_ops(&jolta, &mut session, &account).await;

        assert_eq!(server.subjects("INBOX"), ["Oops", "Undo me"], "both are back in Gmail's inbox");
        assert!(server.subjects(TRASH).is_empty());
        sync_once(&jolta, &mut session, &account).await;
        assert_eq!(list(&jolta, "in:inbox"), ["Oops", "Undo me"]);
        assert!(list(&jolta, "in:trash").is_empty());
    }
}
