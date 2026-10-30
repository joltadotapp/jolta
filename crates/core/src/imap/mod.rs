//! IMAP engine for plain IMAP accounts (iCloud, Fastmail, others).
//!
//! A sync lists the mailboxes, then per mailbox: compares the server's UID
//! list with the store's, fetches headers of new messages newest first,
//! drops expunged ones, and refreshes flags (CHANGEDSINCE when the server has
//! CONDSTORE). Bodies follow, newest first, in a second pass.

mod gmail;
mod utf7;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use rustls_platform_verifier::ConfigVerifierExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::model::*;
use crate::store::{self, RemoteMailbox};
use crate::{Error, Kite, Result, mime, secrets};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + std::fmt::Debug> Io for T {}

pub type Session = async_imap::Session<Box<dyn Io>>;

const HEADER_BATCH: usize = 250;
const BODY_BATCH: usize = 25;
const FLAG_SWEEP: usize = 5000;
/// Bodies larger than this are fetched only when opened.
pub const MAX_BACKGROUND_BODY: u32 = 10 * 1024 * 1024;

/// TLS everywhere, except plain IMAP to a server on this machine (a local
/// test server), which never leaves the loopback interface.
async fn open_stream(host: &str, port: u16) -> Result<Box<dyn Io>> {
    let tcp = TcpStream::connect((host, port)).await?;
    if port != 993 && tcp.peer_addr()?.ip().is_loopback() {
        return Ok(Box::new(tcp));
    }
    let config = ClientConfig::with_platform_verifier()?;
    let server = ServerName::try_from(host.to_string())
        .map_err(|_| Error::Other(format!("{host:?} is not a valid server name")))?;
    Ok(Box::new(TlsConnector::from(Arc::new(config)).connect(server, tcp).await?))
}

/// What it takes to add a password IMAP account. Hosts left empty come from
/// the address for iCloud and Fastmail.
#[derive(Clone, Debug, Default)]
pub struct ImapSetup {
    pub address: String,
    pub password: String,
    pub display_name: String,
    pub username: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
}

/// Known servers for an address: (IMAP host, port, SMTP host, port).
pub fn known_servers(address: &str) -> Option<(&'static str, u16, &'static str, u16)> {
    let domain = address.rsplit('@').next().unwrap_or("").to_lowercase();
    match domain.as_str() {
        "icloud.com" | "me.com" | "mac.com" => Some(("imap.mail.me.com", 993, "smtp.mail.me.com", 587)),
        "fastmail.com" | "fastmail.fm" | "fastmail.net" | "messagingengine.com" => {
            Some(("imap.fastmail.com", 993, "smtp.fastmail.com", 465))
        }
        _ => None,
    }
}

/// Signs in to check the password, then saves the account and its password
/// in the Keychain. An address that already has an IMAP account gets the new
/// password instead of a second account.
pub async fn add_account(kite: &Kite, setup: ImapSetup) -> Result<Account> {
    let address = setup.address.trim().to_string();
    if !address.contains('@') {
        return Err(Error::Auth(format!("“{address}” is not an email address")));
    }
    let known = known_servers(&address);
    let (imap_host, imap_port, smtp_host, smtp_port) = match (setup.imap_host.filter(|h| !h.trim().is_empty()), known) {
        (Some(h), _) => {
            let h = h.trim().to_string();
            let smtp = setup.smtp_host.filter(|h| !h.trim().is_empty()).unwrap_or_else(|| h.replacen("imap", "smtp", 1));
            (h, setup.imap_port.unwrap_or(993), smtp.trim().to_string(), setup.smtp_port.unwrap_or(465))
        }
        (None, Some((i, ip, s, sp))) => (i.to_string(), ip, s.to_string(), sp),
        (None, None) => return Err(Error::Auth(format!("Kite doesn't know the mail server for {address}; enter the IMAP server"))),
    };

    // iCloud takes the full address or the part before the @.
    let mut usernames = vec![setup.username.filter(|u| !u.trim().is_empty()).unwrap_or_else(|| address.clone())];
    if imap_host == "imap.mail.me.com" {
        usernames.push(address.split('@').next().unwrap_or(&address).to_string());
    }
    let mut username = None;
    let mut last_err = None;
    for u in usernames {
        match connect(&imap_host, imap_port, &u, &setup.password).await {
            Ok(mut session) => {
                session.logout().await.ok();
                username = Some(u);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let Some(username) = username else {
        return Err(match last_err {
            Some(Error::Auth(why)) if known.is_some() => Error::Auth(format!("{why}. Use an app-specific password, not the account password.")),
            Some(e) => e,
            None => Error::Auth("no username to try".into()),
        });
    };

    let existing = kite
        .store
        .read(store::accounts)?
        .into_iter()
        .find(|a| a.kind == AccountKind::Imap && a.address.eq_ignore_ascii_case(&address));
    let is_new = existing.is_none();
    let account = match existing {
        Some(a) => a,
        None => {
            let new = store::NewAccount {
                kind: AccountKind::Imap,
                address,
                display_name: setup.display_name,
                username,
                imap_host,
                imap_port,
                smtp_host,
                smtp_port,
            };
            kite.store.write(|tx| store::insert_account(tx, &new))?
        }
    };
    if let Err(e) = secrets::set_password(&account.secret_ref, &setup.password) {
        if is_new {
            kite.store.write(|tx| store::delete_account(tx, account.id))?;
        }
        return Err(e);
    }
    Ok(account)
}

pub async fn connect(host: &str, port: u16, username: &str, password: &str) -> Result<Session> {
    let mut client = async_imap::Client::new(open_stream(host, port).await?);
    client
        .read_response()
        .await?
        .ok_or_else(|| Error::Other(format!("{host} closed the connection before greeting")))?;
    client.login(username, password).await.map_err(|(e, _)| match e {
        async_imap::error::Error::No(msg) | async_imap::error::Error::Bad(msg) => {
            Error::Auth(format!("{host} refused the password ({})", server_text(&msg)))
        }
        other => Error::Imap(other),
    })
}

/// The server's own words from an async-imap NO or BAD, which arrive as
/// `code: None, info: Some("[AUTHENTICATIONFAILED] Authentication Failed")`.
fn server_text(msg: &str) -> &str {
    let text = msg.split_once("info: Some(\"").map_or(msg, |(_, t)| t.trim_end_matches(&[')', '"'][..]));
    match text.split_once("] ") {
        Some((code, rest)) if code.starts_with('[') => rest,
        _ => text,
    }
}

pub async fn connect_account(account: &Account) -> Result<Session> {
    if account.kind == AccountKind::Gmail {
        let token = crate::oauth::access_token(account).await?;
        return connect_xoauth2(&account.imap_host, account.imap_port, &account.username, &token).await;
    }
    let password = secrets::password(&account.secret_ref)?;
    connect(&account.imap_host, account.imap_port, &account.username, &password).await
}

struct XOAuth2(String);

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> String {
        self.0.clone()
    }
}

pub async fn connect_xoauth2(host: &str, port: u16, username: &str, token: &str) -> Result<Session> {
    let mut client = async_imap::Client::new(open_stream(host, port).await?);
    client
        .read_response()
        .await?
        .ok_or_else(|| Error::Other(format!("{host} closed the connection before greeting")))?;
    client
        .authenticate("XOAUTH2", XOAuth2(crate::oauth::xoauth2(username, token)))
        .await
        .map_err(|(e, _)| match e {
            async_imap::error::Error::No(msg) | async_imap::error::Error::Bad(msg) => Error::Auth(msg),
            other => Error::Imap(other),
        })
}

fn role_of(name: &str, attrs: &[NameAttribute]) -> Option<MailboxRole> {
    for a in attrs {
        let role = match a {
            NameAttribute::All => MailboxRole::All,
            NameAttribute::Archive => MailboxRole::Archive,
            NameAttribute::Drafts => MailboxRole::Drafts,
            NameAttribute::Junk => MailboxRole::Spam,
            NameAttribute::Sent => MailboxRole::Sent,
            NameAttribute::Trash => MailboxRole::Trash,
            NameAttribute::Flagged => MailboxRole::System,
            NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\Important") => MailboxRole::System,
            _ => continue,
        };
        return Some(role);
    }
    if name.eq_ignore_ascii_case("INBOX") {
        return Some(MailboxRole::Inbox);
    }
    match name.to_lowercase().as_str() {
        "sent" | "sent messages" | "sent items" | "sent mail" => Some(MailboxRole::Sent),
        "drafts" => Some(MailboxRole::Drafts),
        "archive" | "archives" => Some(MailboxRole::Archive),
        "trash" | "deleted messages" | "deleted items" | "bin" => Some(MailboxRole::Trash),
        "junk" | "spam" | "junk e-mail" | "junk mail" => Some(MailboxRole::Spam),
        "snoozed" => Some(MailboxRole::Snoozed),
        _ => None,
    }
}

pub(crate) async fn list_mailboxes(session: &mut Session) -> Result<Vec<RemoteMailbox>> {
    let names: Vec<_> = session.list(Some(""), Some("*")).await?.try_collect().await?;
    let mut out: Vec<RemoteMailbox> = names
        .iter()
        .map(|n| {
            let display = utf7::decode(n.name());
            let leaf = match n.delimiter() {
                Some(d) => display.rsplit(d).next().unwrap_or(&display).to_string(),
                None => display.clone(),
            };
            let name = if n.name().eq_ignore_ascii_case("INBOX") { "Inbox".to_string() } else { display };
            RemoteMailbox {
                important: n.attributes().iter().any(|a| matches!(a, NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\Important"))),
                role: role_of(&leaf, n.attributes()),
                remote_id: n.name().to_string(),
                name,
                delimiter: n.delimiter().map(str::to_string),
                selectable: !n.attributes().iter().any(|a| matches!(a, NameAttribute::NoSelect)),
            }
        })
        .collect();
    // Only one mailbox per role: the first one the server marked keeps it.
    let mut taken = HashSet::new();
    for m in &mut out {
        if let Some(r) = m.role.filter(|r| *r != MailboxRole::System) {
            if !taken.insert(r) {
                m.role = None;
            }
        }
    }
    Ok(out)
}

fn flags_of(fetch: &Fetch) -> Flags {
    let mut f = Flags::default();
    for flag in fetch.flags() {
        f = match flag {
            Flag::Seen => f.with(Flags::SEEN, true),
            Flag::Flagged => f.with(Flags::FLAGGED, true),
            Flag::Answered => f.with(Flags::ANSWERED, true),
            Flag::Draft => f.with(Flags::DRAFT, true),
            Flag::Deleted => f.with(Flags::DELETED, true),
            _ => f,
        };
    }
    f
}

/// Compresses UIDs into an IMAP sequence set such as `1:5,7,9:12`.
pub fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}:{end}"));
        }
        i += 1;
    }
    out
}

#[derive(Clone, Copy, Debug)]
pub struct SyncOptions {
    pub bodies: bool,
}

#[derive(Clone, Debug)]
pub enum SyncEvent {
    Mailboxes(usize),
    Mailbox { name: String, new: usize, removed: usize, flag_changes: usize },
    Headers { mailbox: String, done: usize, total: usize },
    Bodies { done: usize },
}

#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    pub mailboxes: usize,
    pub new: usize,
    pub removed: usize,
    pub flag_changes: usize,
    pub bodies: usize,
    /// Messages stored for the first time, for new-mail notices.
    pub new_messages: Vec<MessageId>,
}

impl SyncReport {
    pub fn changed(&self) -> bool {
        self.new + self.removed + self.flag_changes + self.bodies > 0
    }
}

/// What the server can do, read once per connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub condstore: bool,
    pub moves: bool,
    pub uidplus: bool,
}

pub async fn caps(session: &mut Session) -> Result<Caps> {
    let c = session.capabilities().await?;
    Ok(Caps { condstore: c.has_str("CONDSTORE"), moves: c.has_str("MOVE"), uidplus: c.has_str("UIDPLUS") })
}

/// Which mailboxes a sync covers. `All` also refreshes the mailbox list.
#[derive(Clone, Debug)]
pub enum Scope {
    All,
    Mailboxes(Vec<MailboxId>),
}

pub async fn sync_account(
    kite: &Kite,
    account: &Account,
    opts: SyncOptions,
    mut progress: impl FnMut(SyncEvent),
) -> Result<SyncReport> {
    let mut session = connect_account(account).await?;
    let caps = caps(&mut session).await?;
    let report = sync_session(kite, &mut session, caps, account, &Scope::All, opts, &mut progress).await;
    session.logout().await.ok();
    report
}

pub async fn sync_session(
    kite: &Kite,
    session: &mut Session,
    caps: Caps,
    account: &Account,
    scope: &Scope,
    opts: SyncOptions,
    progress: &mut impl FnMut(SyncEvent),
) -> Result<SyncReport> {
    if account.kind == AccountKind::Gmail {
        return gmail::sync(kite, session, caps, account, opts, progress).await;
    }
    let account_id = account.id;
    let mailboxes = match scope {
        Scope::All => {
            let remote = list_mailboxes(session).await?;
            kite.store.write_async(move |tx| store::sync_mailbox_list(tx, account_id, &remote)).await?
        }
        Scope::Mailboxes(ids) => {
            let ids = ids.clone();
            kite.store
                .read_async(move |c| Ok(store::mailboxes(c, account_id)?.into_iter().filter(|m| ids.contains(&m.id)).collect()))
                .await?
        }
    };
    progress(SyncEvent::Mailboxes(mailboxes.len()));

    let mut report = SyncReport { mailboxes: mailboxes.len(), ..Default::default() };
    let mut queue: Vec<Mailbox> = mailboxes.into_iter().filter(|m| m.selectable).collect();
    while let Some(mailbox) = kite.next_mailbox(&mut queue) {
        let mailbox = &mailbox;
        let (new, removed, flag_changes) = sync_mailbox(kite, session, account, mailbox, caps.condstore, progress).await?;
        report.new += new.len();
        report.new_messages.extend(new.iter().copied());
        report.removed += removed;
        report.flag_changes += flag_changes;
        progress(SyncEvent::Mailbox { name: mailbox.name.clone(), new: new.len(), removed, flag_changes });
    }

    if opts.bodies {
        report.bodies = fetch_bodies(kite, session, account, progress).await?;
    }
    Ok(report)
}

async fn sync_mailbox(
    kite: &Kite,
    session: &mut Session,
    account: &Account,
    mailbox: &Mailbox,
    condstore: bool,
    progress: &mut impl FnMut(SyncEvent),
) -> Result<(Vec<MessageId>, usize, usize)> {
    let selected = if condstore {
        session.select_condstore(&mailbox.remote_id).await?
    } else {
        session.select(&mailbox.remote_id).await?
    };
    let mailbox_id = mailbox.id;
    let mut since_modseq = mailbox.highest_modseq;
    if mailbox.uidvalidity.is_some() && mailbox.uidvalidity != selected.uid_validity {
        kite.store.write_async(move |tx| store::clear_mailbox(tx, mailbox_id)).await?;
        since_modseq = None;
    }

    let local = kite.store.read_async(move |c| store::mailbox_uids(c, mailbox_id)).await?;
    let server: HashSet<u32> =
        if selected.exists == 0 { HashSet::new() } else { session.uid_search("ALL").await? };

    let removed: Vec<MessageId> =
        local.iter().filter(|(uid, _)| !server.contains(uid)).map(|(_, (id, _))| *id).collect();
    let removed_count = removed.len();
    if !removed.is_empty() {
        kite.store.write_async(move |tx| store::unlink_messages(tx, mailbox_id, &removed)).await?;
    }

    let mut new: Vec<u32> = server.iter().copied().filter(|u| !local.contains_key(u)).collect();
    new.sort_unstable_by(|a, b| b.cmp(a));
    let total = new.len();
    let mut inserted = Vec::new();
    for (i, chunk) in new.chunks(HEADER_BATCH).enumerate() {
        let fetched: Vec<Fetch> = session
            .uid_fetch(uid_set(chunk), "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])")
            .await?
            .try_collect()
            .await?;
        let messages: Vec<NewMessage> = fetched
            .iter()
            .filter_map(|f| {
                let uid = f.uid?;
                let header = f.header().unwrap_or_default();
                let date = f
                    .internal_date()
                    .map(|d| d.timestamp())
                    .or_else(|| mime::header_date(header))
                    .unwrap_or(0);
                Some(NewMessage {
                    account_id: account.id,
                    remote_id: format!("{}/{}/{uid}", mailbox.id, selected.uid_validity.unwrap_or(0)),
                    mailbox_id: mailbox.id,
                    uid: Some(uid),
                    date,
                    envelope: mime::parse_envelope(header),
                    flags: flags_of(f),
                    size: f.size.unwrap_or(0),
                    thread_hint: None,
                    body: None,
                })
            })
            .collect();
        let account_id = account.id;
        let ids = kite
            .store
            .write_async(move |tx| {
                let moving = store::pending_move_uids(tx, account_id, mailbox_id)?;
                let mut ids = Vec::new();
                for m in messages.iter().filter(|m| !m.uid.is_some_and(|u| moving.contains(&u))) {
                    if let store::Inserted::New(id) = store::insert_message(tx, m)? {
                        ids.push(id);
                    }
                }
                Ok(ids)
            })
            .await?;
        inserted.extend(ids);
        progress(SyncEvent::Headers {
            mailbox: mailbox.name.clone(),
            done: ((i + 1) * HEADER_BATCH).min(total),
            total,
        });
    }

    let mut changes: Vec<(MessageId, Flags)> = Vec::new();
    let known: HashMap<u32, (MessageId, Flags)> =
        local.into_iter().filter(|(uid, _)| server.contains(uid)).collect();
    if !known.is_empty() {
        let fetched: Vec<Fetch> = match (condstore, since_modseq) {
            (true, Some(modseq)) => {
                session.uid_fetch("1:*", format!("(UID FLAGS) (CHANGEDSINCE {modseq})")).await?.try_collect().await?
            }
            _ => {
                let mut uids: Vec<u32> = known.keys().copied().collect();
                uids.sort_unstable_by(|a, b| b.cmp(a));
                uids.truncate(FLAG_SWEEP);
                session.uid_fetch(uid_set(&uids), "(UID FLAGS)").await?.try_collect().await?
            }
        };
        for f in &fetched {
            let Some((id, old)) = f.uid.and_then(|u| known.get(&u)) else { continue };
            let flags = flags_of(f);
            if flags != *old {
                changes.push((*id, flags));
            }
        }
    }
    let change_count = changes.len();
    let (uidvalidity, modseq) = (selected.uid_validity, selected.highest_modseq);
    let account_id = account.id;
    kite.store
        .write_async(move |tx| {
            let pending = store::pending_messages(tx, account_id)?;
            for (id, flags) in changes.iter().filter(|(id, _)| !pending.contains(id)) {
                store::set_flags(tx, *id, *flags)?;
            }
            if !store::has_pending_ops(tx, account_id)? {
                store::drop_unconfirmed(tx, mailbox_id)?;
            }
            store::set_mailbox_cursor(tx, mailbox_id, uidvalidity, modseq)
        })
        .await?;
    Ok((inserted, removed_count, change_count))
}

pub(crate) fn flag_list(bits: u32) -> String {
    let names = [
        (Flags::SEEN, "\\Seen"),
        (Flags::FLAGGED, "\\Flagged"),
        (Flags::ANSWERED, "\\Answered"),
        (Flags::DRAFT, "\\Draft"),
        (Flags::DELETED, "\\Deleted"),
    ];
    names.iter().filter(|(b, _)| bits & b != 0).map(|(_, n)| *n).collect::<Vec<_>>().join(" ")
}

/// Sends one queued op to the server. Returns the mailboxes it changed, so
/// the caller can sync them (a moved message gets its new UID that way).
pub async fn apply_op(kite: &Kite, session: &mut Session, caps: Caps, op: &store::Op) -> Result<Vec<MailboxId>> {
    let account = op.account;
    let kind = kite.store.read_async(move |c| Ok(store::account(c, account)?.kind)).await?;
    if kind == AccountKind::Gmail {
        return gmail::apply_op(kite, session, op).await;
    }
    let paths: HashMap<MailboxId, String> = kite
        .store
        .read_async(move |c| Ok(store::mailboxes(c, account)?.into_iter().map(|m| (m.id, m.remote_id)).collect()))
        .await?;
    let path = |m: &MailboxId| paths.get(m).cloned().ok_or_else(|| Error::NotFound(format!("mailbox {m}")));
    match op.kind {
        store::OpKind::Flags { add, remove } => {
            let ids: Vec<MessageId> = op.messages.iter().map(|(m, _)| *m).collect();
            let locations = kite.store.read_async(move |c| store::server_locations(c, &ids)).await?;
            let mut by_mailbox: HashMap<MailboxId, Vec<u32>> = HashMap::new();
            for (mailbox, uid) in locations {
                by_mailbox.entry(mailbox).or_default().push(uid);
            }
            for (mailbox, uids) in &by_mailbox {
                session.select(path(mailbox)?).await?;
                for (sign, bits) in [("+", add), ("-", remove)] {
                    if bits != 0 {
                        session
                            .uid_store(uid_set(uids), format!("{sign}FLAGS.SILENT ({})", flag_list(bits)))
                            .await?
                            .try_collect::<Vec<_>>()
                            .await?;
                    }
                }
            }
            Ok(by_mailbox.into_keys().collect())
        }
        store::OpKind::Move { from, to } => {
            session.select(path(&from)?).await?;
            let mut uids: Vec<u32> = op.messages.iter().filter_map(|(_, u)| *u).collect();
            let missing: Vec<MessageId> = op.messages.iter().filter(|(_, u)| u.is_none()).map(|(m, _)| *m).collect();
            if !missing.is_empty() {
                let header_ids = kite.store.read_async(move |c| store::header_message_ids(c, &missing)).await?;
                for mid in header_ids {
                    let found = session.uid_search(format!("HEADER Message-ID \"<{}>\"", mid.replace('"', ""))).await?;
                    uids.extend(found);
                }
            }
            if uids.is_empty() {
                return Ok(vec![from, to]);
            }
            let set = uid_set(&uids);
            let target = path(&to)?;
            let moved = if caps.moves { session.uid_mv(&set, &target).await } else { session.uid_copy(&set, &target).await };
            if let Err(e) = moved {
                // NO [TRYCREATE]: the target does not exist yet (a first snooze).
                if !e.to_string().to_uppercase().contains("TRYCREATE") && !e.to_string().to_uppercase().contains("NONEXISTENT") {
                    return Err(e.into());
                }
                session.create(&target).await?;
                session.select(path(&from)?).await?;
                if caps.moves {
                    session.uid_mv(&set, &target).await?;
                } else {
                    session.uid_copy(&set, &target).await?;
                }
            }
            if !caps.moves {
                session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                if caps.uidplus {
                    session.uid_expunge(&set).await?.try_collect::<Vec<_>>().await?;
                } else {
                    session.expunge().await?.try_collect::<Vec<_>>().await?;
                }
            }
            Ok(vec![from, to])
        }
    }
}

/// Holds IDLE on INBOX over its own connection and calls `changed` whenever
/// the server reports something. Returns only on a connection error.
pub async fn idle_inbox(account: &Account, changed: impl Fn()) -> Result<()> {
    let mut session = connect_account(account).await?;
    session.examine("INBOX").await?;
    loop {
        let mut handle = session.idle();
        handle.init().await?;
        let (wait, stop) = handle.wait_with_timeout(std::time::Duration::from_secs(25 * 60));
        let response = wait.await?;
        drop(stop);
        session = handle.done().await?;
        if matches!(response, async_imap::extensions::idle::IdleResponse::NewData(_)) {
            changed();
        }
    }
}

pub(crate) async fn store_body(kite: &Kite, id: MessageId, raw: Vec<u8>) -> Result<()> {
    let kite2 = kite.clone();
    tokio::task::spawn_blocking(move || {
        let hash = kite2.blobs.put(&raw)?;
        let (_, body) = mime::parse_full(&raw, hash);
        kite2.store.write(|tx| store::attach_body(tx, id, &body))
    })
    .await?
}

pub(crate) async fn fetch_bodies(
    kite: &Kite,
    session: &mut Session,
    account: &Account,
    progress: &mut impl FnMut(SyncEvent),
) -> Result<usize> {
    let account_id = account.id;
    let mut before = i64::MAX;
    let mut done = 0;
    let mut paths: HashMap<MailboxId, String> = HashMap::new();
    for m in kite.store.read_async(move |c| store::mailboxes(c, account_id)).await? {
        paths.insert(m.id, m.remote_id);
    }
    loop {
        let batch = kite
            .store
            .read_async(move |c| store::messages_without_body(c, account_id, MAX_BACKGROUND_BODY, before, BODY_BATCH))
            .await?;
        let Some(last) = batch.last() else { break };
        before = last.0.0;

        let mut by_mailbox: HashMap<MailboxId, Vec<(u32, MessageId)>> = HashMap::new();
        for (id, mailbox, uid) in batch {
            by_mailbox.entry(mailbox).or_default().push((uid, id));
        }
        for (mailbox, items) in by_mailbox {
            let Some(path) = paths.get(&mailbox) else { continue };
            session.examine(path).await?;
            let uids: Vec<u32> = items.iter().map(|(u, _)| *u).collect();
            let ids: HashMap<u32, MessageId> = items.into_iter().collect();
            let fetched: Vec<Fetch> =
                session.uid_fetch(uid_set(&uids), "(UID BODY.PEEK[])").await?.try_collect().await?;
            for f in fetched {
                let (Some(uid), Some(raw)) = (f.uid, f.body()) else { continue };
                let Some(&id) = ids.get(&uid) else { continue };
                store_body(kite, id, raw.to_vec()).await?;
                done += 1;
            }
        }
        progress(SyncEvent::Bodies { done });
    }
    Ok(done)
}

/// The raw message, from the blob store or else downloaded now.
pub async fn message_source(kite: &Kite, id: MessageId) -> Result<Vec<u8>> {
    if let Some(blob) = kite.store.read_async(move |c| store::message_blob(c, id)).await? {
        if let Some(n) = blob.strip_prefix("synth:") {
            let n = n.parse().unwrap_or(0);
            return kite.store.read_async(move |c| crate::synth::raw_message(c, id, n)).await;
        }
        return kite.blobs.get(&blob);
    }
    let graph: Option<(Account, String)> = kite
        .store
        .read_async(move |c| {
            let (account, remote): (AccountId, String) =
                c.query_row("SELECT account_id, remote_id FROM message WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let account = store::account(c, account)?;
            Ok((account.kind == AccountKind::Graph).then_some((account, remote)))
        })
        .await?;
    if let Some((account, remote)) = graph {
        let raw = crate::graph::Graph::new(&account)?.message_mime(&remote).await?;
        store_body(kite, id, raw.clone()).await?;
        return Ok(raw);
    }
    let loc = kite.store.read_async(move |c| store::message_location(c, id)).await?;
    let Some((account_id, _, path, uid, _)) = loc else {
        return Err(Error::NotFound(format!("message {id}")));
    };
    let account = kite.store.read_async(move |c| store::account(c, account_id)).await?;
    let mut session = connect_account(&account).await?;
    session.examine(&path).await?;
    let fetched: Vec<Fetch> = session.uid_fetch(uid.to_string(), "(UID BODY.PEEK[])").await?.try_collect().await?;
    session.logout().await.ok();
    let raw = fetched
        .iter()
        .find_map(|f| f.body())
        .ok_or_else(|| Error::NotFound(format!("message {id} is no longer on the server")))?
        .to_vec();
    store_body(kite, id, raw.clone()).await?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::uid_set;

    #[test]
    fn reads_server_text() {
        assert_eq!(super::server_text(r#"code: None, info: Some("[AUTHENTICATIONFAILED] Authentication Failed")"#), "Authentication Failed");
        assert_eq!(super::server_text("Invalid credentials"), "Invalid credentials");
    }

    #[test]
    fn compresses_uid_sets() {
        assert_eq!(uid_set(&[9, 1, 2, 3, 5, 10, 11, 3]), "1:3,5,9:11");
        assert_eq!(uid_set(&[7]), "7");
    }
}
