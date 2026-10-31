//! Keeps accounts in sync while the app runs.
//!
//! Each account gets a worker task with its own connection. In a loop it
//! sends queued ops, then syncs what needs syncing: everything on start and
//! every few minutes, the inbox when IDLE reports a change, and any mailbox
//! an op touched. A second task holds IDLE on the inbox over another
//! connection. Connection failures back off and retry; queued ops survive
//! restarts because they live in the store.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

use crate::actions::{self, Action, Context};
use crate::compose::{self, Compose};
use crate::graph::Graph;
use crate::imap::{self, Caps, Scope, Session, SyncOptions};
use crate::model::*;
use crate::store::{self, OpId};
use crate::{Kite, Result, mime};
use futures::TryStreamExt;

const FULL_SYNC_EVERY: Duration = Duration::from_secs(10 * 60);
/// Graph has no IDLE; the inbox's delta is polled this often.
const GRAPH_POLL: Duration = Duration::from_secs(30);

/// A worker's open line to its server.
enum Link {
    Imap(Session, Caps),
    Graph(Graph),
}
const MAX_ATTEMPTS: u32 = 5;
/// How long a sent message waits in the outbox so it can be taken back.
pub const UNDO_SEND_SECONDS: i64 = 10;
/// Mail that arrived longer ago than this is not announced as new.
const NEW_MAIL_WINDOW: i64 = 15 * 60;

#[derive(Clone, Debug)]
pub enum Event {
    ListsChanged,
    SyncStarted(AccountId),
    SyncFinished(AccountId),
    SyncFailed(AccountId, String),
    NewMail { account: AccountId, thread: ThreadId, from: String, subject: String },
    ActionFailed(String),
    Sent { account: AccountId, subject: String },
    Woke { account: AccountId, thread: ThreadId, subject: String },
    /// An MCP client connected for the first time and waits for approval.
    AgentRequest { agent: i64, name: String },
    /// An agent wrote a draft for review.
    AgentDraft { draft: i64, subject: String },
    /// An agent called a tool (the audit log changed).
    AgentActivity,
    SendFailed(String),
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Default)]
struct Wake {
    notify: Notify,
    inbox_changed: AtomicBool,
    full_sync: AtomicBool,
    /// Mailboxes the user opened that have never synced.
    wanted: Mutex<HashSet<MailboxId>>,
}

impl Wake {
    fn poke(&self) {
        self.notify.notify_one();
    }
}

pub struct Engine {
    kite: Kite,
    sink: Sink,
    runtime: tokio::runtime::Handle,
    wakes: Mutex<HashMap<AccountId, Arc<Wake>>>,
}

impl Engine {
    /// Starts a worker and an IDLE task for every account that has a server.
    pub fn start(kite: Kite, sink: Sink, runtime: tokio::runtime::Handle) -> Result<Arc<Engine>> {
        let engine = Arc::new(Engine { kite, sink, runtime, wakes: Mutex::new(HashMap::new()) });
        for account in engine.kite.store.read(store::accounts)? {
            engine.spawn(account);
        }
        let (kite, sink) = (engine.kite.clone(), engine.sink.clone());
        engine.runtime.spawn_blocking(move || match scan_list_headers(&kite) {
            Ok(0) => {}
            Ok(_) => sink(Event::ListsChanged),
            Err(e) => tracing::warn!("list header scan stopped: {e}"),
        });
        Ok(engine)
    }

    /// Starts syncing an account added while the engine runs.
    pub fn add_account(self: &Arc<Self>, account: Account) {
        if !self.wakes.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&account.id) {
            self.spawn(account);
        }
    }

    fn spawn(self: &Arc<Self>, account: Account) {
        let graph = account.kind == AccountKind::Graph;
        if account.imap_host.is_empty() && !graph {
            return;
        }
        let wake = Arc::new(Wake::default());
        wake.full_sync.store(true, Ordering::SeqCst);
        self.wakes.lock().unwrap_or_else(|e| e.into_inner()).insert(account.id, wake.clone());
        if graph {
            let engine = self.clone();
            self.runtime.spawn(async move { engine.work(account, wake).await });
            return;
        }

        let idle_wake = wake.clone();
        let idle_account = account.clone();
        self.runtime.spawn(async move {
            let mut backoff = Backoff::default();
            loop {
                let started = Instant::now();
                let result = imap::idle_inbox(&idle_account, || {
                    idle_wake.inbox_changed.store(true, Ordering::SeqCst);
                    idle_wake.poke();
                })
                .await;
                if let Err(e) = result {
                    tracing::debug!("IDLE for {} ended: {e}", idle_account.address);
                }
                if started.elapsed() > Duration::from_secs(60) {
                    backoff = Backoff::default();
                }
                tokio::time::sleep(backoff.next()).await;
            }
        });

        let engine = self.clone();
        self.runtime.spawn(async move { engine.work(account, wake).await });
    }

    /// The account's settings changed: its worker reconnects with them now.
    pub fn settings_changed(&self, account: AccountId) {
        self.wake(account);
        (self.sink)(Event::ListsChanged);
    }

    fn wake(&self, account: AccountId) {
        if let Some(w) = self.wakes.lock().unwrap_or_else(|e| e.into_inner()).get(&account) {
            w.poke();
        }
    }

    /// Applies an action locally, tells the views, and queues it for the
    /// server. Returns a token for `undo`.
    /// Applies `action` to each thread in one transaction, so the list
    /// changes once. Returns the queued ops; threads with nothing to change
    /// queue none.
    pub fn perform(&self, threads: &[ThreadId], action: Action, context: Context) -> Result<Vec<OpId>> {
        let queued = self.kite.store.write(|tx| {
            threads.iter().filter_map(|&t| actions::perform(tx, t, action, context).transpose()).collect::<Result<Vec<_>>>()
        })?;
        if !queued.is_empty() {
            (self.sink)(Event::ListsChanged);
        }
        self.wake_all(&queued);
        Ok(queued.into_iter().map(|(_, op)| op).collect())
    }

    pub fn undo(&self, ops: &[OpId]) -> Result<()> {
        let queued = self.kite.store.write(|tx| ops.iter().filter_map(|&op| actions::undo(tx, op).transpose()).collect::<Result<Vec<_>>>())?;
        (self.sink)(Event::ListsChanged);
        self.wake_all(&queued);
        Ok(())
    }

    fn wake_all(&self, ops: &[(AccountId, OpId)]) {
        let mut accounts: Vec<AccountId> = ops.iter().map(|&(a, _)| a).collect();
        accounts.sort_unstable();
        accounts.dedup();
        for account in accounts {
            self.wake(account);
        }
    }

    /// Queues a message for sending after the undo window. Returns a token
    /// for `cancel_send`.
    pub fn send(&self, compose: &Compose) -> Result<i64> {
        let account_id = compose.account.ok_or_else(|| crate::Error::Other("Choose an account to send from.".into()))?;
        let account = self.kite.store.read(|c| store::account(c, account_id))?;
        let built = compose::build(&account, compose, true)?;
        let blob = self.kite.blobs.put(&built.raw)?;
        let id = self.kite.store.write(|tx| {
            let draft = store::save_draft(tx, compose)?;
            tx.execute("UPDATE draft SET dirty = 0 WHERE id = ?1", [draft])?;
            store::queue_outbox(
                tx, account_id, Some(draft), &blob, &built.message_id, &built.from, &built.recipients,
                compose.replying_to, UNDO_SEND_SECONDS,
            )
        })?;
        self.wake(account_id);
        Ok(id)
    }

    /// Takes a waiting message back. Returns it as a draft to reopen.
    pub fn cancel_send(&self, outbox: i64) -> Result<Option<Compose>> {
        let draft = self.kite.store.write(|tx| store::cancel_send(tx, outbox))?;
        match draft {
            Some(d) => Ok(self.kite.store.read(|c| store::draft(c, d))?),
            None => Ok(None),
        }
    }

    /// Saves a draft locally and, soon after, to the server's Drafts.
    pub fn save_draft(&self, compose: &Compose) -> Result<i64> {
        let id = self.kite.store.write(|tx| store::save_draft(tx, compose))?;
        if let Some(a) = compose.account {
            self.wake(a);
        }
        Ok(id)
    }

    pub fn discard_draft(&self, id: i64) -> Result<()> {
        let account = self.kite.store.write(|tx| {
            store::discard_draft(tx, id)?;
            Ok(tx.query_row("SELECT account_id FROM draft WHERE id = ?1", [id], |r| r.get::<_, Option<AccountId>>(0))?)
        })?;
        if let Some(a) = account {
            self.wake(a);
        }
        Ok(())
    }

    /// Removes an account, its mail and its Keychain entry. Its worker stops
    /// on its next round.
    /// The user is looking at `mailbox`: sync it before other mailboxes, and
    /// now if it has never synced.
    pub fn focus_mailbox(&self, mailbox: Option<MailboxId>) {
        *self.kite.focus.lock().unwrap_or_else(|e| e.into_inner()) = mailbox;
        let Some(id) = mailbox else { return };
        let Ok(m) = self.kite.store.read(|c| store::mailbox(c, id)) else { return };
        if m.synced {
            return;
        }
        if let Some(w) = self.wakes.lock().unwrap_or_else(|e| e.into_inner()).get(&m.account_id) {
            w.wanted.lock().unwrap_or_else(|e| e.into_inner()).insert(id);
            w.poke();
        }
    }

    pub fn remove_account(&self, account: AccountId) -> Result<()> {
        let a = self.kite.store.read(|c| store::account(c, account))?;
        self.kite.store.write(|tx| store::delete_account(tx, account))?;
        crate::secrets::delete(&a.secret_ref)?;
        crate::secrets::delete(&crate::secrets::smtp_ref(&a.secret_ref))?;
        self.wake(account);
        (self.sink)(Event::ListsChanged);
        Ok(())
    }

    /// Syncs every account now, all mailboxes.
    pub fn sync_now(&self) {
        for w in self.wakes.lock().unwrap_or_else(|e| e.into_inner()).values() {
            w.full_sync.store(true, Ordering::SeqCst);
            w.poke();
        }
    }

    async fn work(self: Arc<Self>, mut account: Account, wake: Arc<Wake>) {
        let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let mut conn: Option<Link> = None;
        let poll = (account.kind == AccountKind::Graph).then_some(GRAPH_POLL);
        let mut backoff = Backoff::default();
        let mut last_full = Instant::now();
        loop {
            let id = account.id;
            match self.kite.store.read(|c| store::account(c, id)) {
                Err(crate::Error::NotFound(_)) => {
                    // The account was removed; stop syncing it.
                    self.wakes.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                    return;
                }
                // New settings: reconnect with them.
                Ok(current) if current != account => {
                    account = current;
                    conn = None;
                }
                _ => {}
            }
            match self.round(&account, &wake, &mut conn, started).await {
                Ok(()) => backoff = Backoff::default(),
                Err(e) => {
                    conn = None;
                    (self.sink)(Event::SyncFailed(account.id, e.to_string()));
                    tokio::time::sleep(backoff.next()).await;
                    continue;
                }
            }
            if last_full.elapsed() >= FULL_SYNC_EVERY {
                wake.full_sync.store(true, Ordering::SeqCst);
                last_full = Instant::now();
                continue;
            }
            let until_full = FULL_SYNC_EVERY.saturating_sub(last_full.elapsed());
            let account_id = account.id;
            let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
            let until_wake = self.kite.store.read(|c| store::next_wake(c, account_id)).ok().flatten().map(|t| (t - now).max(0));
            let until_send = self
                .kite
                .store
                .read(|c| store::next_send_in(c, account_id))
                .ok()
                .flatten()
                .into_iter()
                .chain(until_wake)
                .min()
                .map_or(until_full, |s| Duration::from_secs(s as u64).min(until_full));
            let wait = poll.map_or(until_send, |p| p.min(until_send));
            tokio::select! {
                _ = wake.notify.notified() => {}
                _ = tokio::time::sleep(wait) => {
                    if poll.is_some() {
                        wake.inbox_changed.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
    }

    /// One pass: connect if needed, send ops, sync.
    async fn round(&self, account: &Account, wake: &Wake, conn: &mut Option<Link>, started: i64) -> Result<()> {
        if conn.is_none() {
            *conn = Some(if account.kind == AccountKind::Graph {
                let graph = Graph::new(account)?;
                graph.whoami().await?;
                Link::Graph(graph)
            } else {
                let mut session = imap::connect_account(account).await?;
                let caps = imap::caps(&mut session).await?;
                Link::Imap(session, caps)
            });
        }
        let link = conn.as_mut().expect("connected above");

        let mut touched: HashSet<MailboxId> = HashSet::new();
        let account_id = account.id;
        self.wake_snoozed(account_id).await?;
        for op in self.kite.store.read_async(move |c| store::pending_ops(c, account_id)).await? {
            let applied = match link {
                Link::Imap(session, caps) => imap::apply_op(&self.kite, session, *caps, &op).await,
                Link::Graph(g) => g.apply_op(&self.kite, &op).await,
            };
            match applied {
                Ok(mailboxes) => {
                    touched.extend(mailboxes);
                    let id = op.id;
                    self.kite.store.write_async(move |tx| store::finish_op(tx, id)).await?;
                }
                Err(e) if op.attempts + 1 >= MAX_ATTEMPTS => {
                    let message = e.to_string();
                    let op2 = op.clone();
                    let m2 = message.clone();
                    self.kite.store.write_async(move |tx| actions::give_up(tx, &op2, &m2)).await?;
                    (self.sink)(Event::ActionFailed(message));
                    (self.sink)(Event::ListsChanged);
                }
                Err(e) => {
                    let (id, message) = (op.id, e.to_string());
                    self.kite.store.write_async(move |tx| store::fail_attempt(tx, id, &message, false)).await?;
                    return Err(e);
                }
            }
        }

        let sent = self.send_due(account, link).await?;
        if !sent.is_empty() {
            let sent_box = self.role_mailbox(account_id, MailboxRole::Sent).await?;
            touched.extend(sent_box.map(|m| m.id));
        }
        self.sync_drafts(account, link).await?;

        let scope = if wake.full_sync.swap(false, Ordering::SeqCst) {
            wake.inbox_changed.store(false, Ordering::SeqCst);
            Some(Scope::All)
        } else {
            if wake.inbox_changed.swap(false, Ordering::SeqCst) {
                let inbox = self
                    .kite
                    .store
                    .read_async(move |c| Ok(store::mailboxes(c, account_id)?.into_iter().find(|m| m.role == Some(MailboxRole::Inbox))))
                    .await?;
                touched.extend(inbox.map(|m| m.id));
            }
            touched.extend(wake.wanted.lock().unwrap_or_else(|e| e.into_inner()).drain());
            (!touched.is_empty()).then(|| Scope::Mailboxes(touched.into_iter().collect()))
        };
        let Some(scope) = scope else { return Ok(()) };

        let full = matches!(scope, Scope::All);
        if full {
            (self.sink)(Event::SyncStarted(account.id));
        }
        let mut last_event = Instant::now();
        let sink = self.sink.clone();
        let mut dirty = false;
        let mut on_progress = |e: imap::SyncEvent| {
            // Tell the views only when this step stored something.
            dirty |= match e {
                imap::SyncEvent::Headers { .. } | imap::SyncEvent::Bodies { .. } => true,
                imap::SyncEvent::Mailbox { new, removed, flag_changes, .. } => new + removed + flag_changes > 0,
                imap::SyncEvent::Mailboxes(_) => false,
            };
            if dirty && last_event.elapsed() > Duration::from_millis(700) {
                last_event = Instant::now();
                dirty = false;
                sink(Event::ListsChanged);
            }
        };
        let report = match link {
            Link::Imap(session, caps) => {
                imap::sync_session(&self.kite, session, *caps, account, &scope, SyncOptions { bodies: true }, &mut on_progress).await?
            }
            Link::Graph(g) => g.sync(&self.kite, &scope, &mut on_progress).await?,
        };
        if report.changed() {
            (self.sink)(Event::ListsChanged);
        }
        if full {
            (self.sink)(Event::SyncFinished(account.id));
        }
        if let Link::Imap(session, caps) = link {
            self.file_sent_copies(account, session, *caps, &sent).await?;
        }
        self.announce(account.id, report.new_messages, started).await
    }

    /// Moves snoozed threads whose time has come back to the inbox.
    async fn wake_snoozed(&self, account: AccountId) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let due = self.kite.store.read_async(move |c| store::due_snoozes(c, account, now)).await?;
        for (id, thread) in due {
            let subject = self
                .kite
                .store
                .write_async(move |tx| {
                    actions::wake(tx, id, thread)?;
                    Ok(tx
                        .query_row("SELECT subject FROM message WHERE thread_id = ?1 ORDER BY id DESC LIMIT 1", [thread], |r| r.get(0))
                        .unwrap_or_default())
                })
                .await?;
            (self.sink)(Event::Woke { account, thread, subject });
            (self.sink)(Event::ListsChanged);
        }
        Ok(())
    }

    async fn role_mailbox(&self, account: AccountId, role: MailboxRole) -> Result<Option<Mailbox>> {
        self.kite
            .store
            .read_async(move |c| Ok(store::mailboxes(c, account)?.into_iter().find(|m| m.role == Some(role))))
            .await
    }

    /// Sends every outbox message whose undo window has passed.
    async fn send_due(&self, account: &Account, link: &mut Link) -> Result<Vec<store::OutboxItem>> {
        let account_id = account.id;
        let due = self.kite.store.read_async(move |c| store::due_outbox(c, account_id)).await?;
        let mut sent = Vec::new();
        for item in due {
            let raw = self.kite.blobs.get(&item.blob)?;
            let result = match link {
                Link::Graph(g) => g.send_mime(&raw).await,
                Link::Imap(..) => crate::smtp::send(account, &item.sender, &item.recipients, &raw).await,
            };
            match result {
                Ok(()) => {
                    let done = item.clone();
                    self.kite
                        .store
                        .write_async(move |tx| {
                            store::outbox_sent(tx, &done)?;
                            if let Some(original) = done.replying_to {
                                let kind = store::OpKind::Flags { add: Flags::ANSWERED, remove: 0 };
                                let changed = store::apply_local(tx, &kind, &[original])?;
                                if !changed.is_empty() {
                                    store::insert_op(tx, done.account, &kind, &changed)?;
                                }
                            }
                            Ok(())
                        })
                        .await?;
                    let subject = mime::parse_envelope(&raw).subject;
                    (self.sink)(Event::Sent { account: account_id, subject });
                    sent.push(item);
                }
                Err(e) => {
                    let give_up = item.attempts + 1 >= MAX_ATTEMPTS;
                    let (id, message) = (item.id, e.to_string());
                    self.kite.store.write_async(move |tx| store::outbox_failed(tx, id, &message, give_up)).await?;
                    (self.sink)(Event::SendFailed(e.to_string()));
                }
            }
        }
        Ok(sent)
    }

    /// Puts a copy of each sent message in Sent, unless the server already
    /// did (Gmail and some others file it themselves).
    async fn file_sent_copies(&self, account: &Account, session: &mut Session, caps: Caps, sent: &[store::OutboxItem]) -> Result<()> {
        if sent.is_empty() {
            return Ok(());
        }
        let Some(sent_box) = self.role_mailbox(account.id, MailboxRole::Sent).await? else { return Ok(()) };
        let mut appended = false;
        for item in sent {
            let (account_id, mid) = (account.id, item.message_id.clone());
            if self.kite.store.read_async(move |c| store::in_sent(c, account_id, &mid)).await? {
                continue;
            }
            let raw = self.kite.blobs.get(&item.blob)?;
            session.append(&sent_box.remote_id, Some("(\\Seen)"), None, &raw).await?;
            appended = true;
        }
        if appended {
            imap::sync_session(&self.kite, session, caps, account, &Scope::Mailboxes(vec![sent_box.id]), SyncOptions { bodies: true }, &mut |_| {}).await?;
            (self.sink)(Event::ListsChanged);
        }
        Ok(())
    }

    /// Replaces or removes server copies of drafts that changed.
    async fn sync_drafts(&self, account: &Account, link: &mut Link) -> Result<()> {
        let account_id = account.id;
        let dirty = self.kite.store.read_async(move |c| store::dirty_drafts(c, account_id)).await?;
        if dirty.is_empty() {
            return Ok(());
        }
        let session = match link {
            Link::Imap(session, _) => session,
            Link::Graph(g) => {
                for d in dirty {
                    let built = (!d.deleted).then(|| compose::build(account, &d.compose, false)).transpose()?;
                    g.save_draft(built.as_ref().map(|b| b.raw.as_slice()), d.server_message_id.as_deref()).await?;
                    let (id, deleted, server_id) = (d.id, d.deleted, built.map(|b| b.message_id));
                    self.kite.store.write_async(move |tx| store::draft_synced(tx, id, server_id.as_deref(), deleted)).await?;
                }
                return Ok(());
            }
        };
        let Some(drafts_box) = self.role_mailbox(account.id, MailboxRole::Drafts).await? else {
            for d in dirty {
                let (id, deleted) = (d.id, d.deleted);
                self.kite.store.write_async(move |tx| store::draft_synced(tx, id, None, deleted)).await?;
            }
            return Ok(());
        };
        session.select(&drafts_box.remote_id).await?;
        for d in dirty {
            if let Some(old) = &d.server_message_id {
                let uids = session.uid_search(format!("HEADER Message-ID \"<{}>\"", old.replace('"', ""))).await?;
                if !uids.is_empty() {
                    let set = imap::uid_set(&uids.into_iter().collect::<Vec<_>>());
                    session.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
                    session.expunge().await?.try_collect::<Vec<_>>().await?;
                }
            }
            let mut server_id = None;
            if !d.deleted {
                let built = compose::build(account, &d.compose, false)?;
                session.append(&drafts_box.remote_id, Some("(\\Draft \\Seen)"), None, &built.raw).await?;
                server_id = Some(built.message_id);
            }
            let (id, deleted) = (d.id, d.deleted);
            self.kite.store.write_async(move |tx| store::draft_synced(tx, id, server_id.as_deref(), deleted)).await?;
        }
        Ok(())
    }

    /// Tells the app about new, unread inbox mail that arrived after launch.
    async fn announce(&self, account: AccountId, ids: Vec<MessageId>, started: i64) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let fresh = self
            .kite
            .store
            .read_async(move |c| {
                let mut stmt = c.prepare(
                    "SELECT m.thread_id, CASE m.from_name WHEN '' THEN m.from_addr ELSE m.from_name END, m.subject
                     FROM message m JOIN message_mailbox mm ON mm.message_id = m.id
                     JOIN mailbox b ON b.id = mm.mailbox_id AND b.role = 'inbox'
                     WHERE m.id = ?1 AND (m.flags & 1) = 0 AND m.date >= ?2",
                )?;
                let mut out = Vec::new();
                for id in ids.iter().take(5) {
                    if let Ok(row) = stmt.query_row(rusqlite::params![id, started - NEW_MAIL_WINDOW], |r| {
                        Ok((r.get::<_, ThreadId>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                    }) {
                        out.push(row);
                    }
                }
                Ok(out)
            })
            .await?;
        for (thread, from, subject) in fresh {
            (self.sink)(Event::NewMail { account, thread, from, subject });
        }
        Ok(())
    }
}

/// 2, 4, 8 ... seconds, capped at five minutes.
#[derive(Default)]
struct Backoff(u32);

impl Backoff {
    fn next(&mut self) -> Duration {
        self.0 = (self.0 + 1).min(8);
        Duration::from_secs((1u64 << self.0).min(300))
    }
}

/// Reads List-Id and List-Unsubscribe from bodies stored before Kite kept
/// them, a batch per write so the app's own writes get turns. Returns how
/// many messages it read.
fn scan_list_headers(kite: &Kite) -> Result<usize> {
    let mut done = 0;
    loop {
        let batch = kite.store.read(|c| store::header_scan_batch(c, 200))?;
        if batch.is_empty() {
            return Ok(done);
        }
        let found: Vec<_> = batch
            .into_iter()
            .map(|(id, blob)| {
                let envelope = kite.blobs.get(&blob).ok().map(|raw| crate::mime::parse_envelope(&raw)).unwrap_or_default();
                (id, envelope.list_id, envelope.list_unsubscribe)
            })
            .collect();
        done += found.len();
        kite.store.write(|tx| {
            for (id, list_id, unsubscribe) in &found {
                store::finish_header_scan(tx, *id, list_id.as_deref(), unsubscribe.as_deref())?;
            }
            Ok(())
        })?;
    }
}
