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
use crate::imap::{self, Caps, Scope, Session, SyncOptions};
use crate::model::*;
use crate::store::{self, OpId};
use crate::{Kite, Result, mime};
use futures::TryStreamExt;

const FULL_SYNC_EVERY: Duration = Duration::from_secs(10 * 60);
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
    SendFailed(String),
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Default)]
struct Wake {
    notify: Notify,
    inbox_changed: AtomicBool,
    full_sync: AtomicBool,
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
        Ok(engine)
    }

    fn spawn(self: &Arc<Self>, account: Account) {
        if account.imap_host.is_empty() {
            return;
        }
        let wake = Arc::new(Wake::default());
        wake.full_sync.store(true, Ordering::SeqCst);
        self.wakes.lock().unwrap_or_else(|e| e.into_inner()).insert(account.id, wake.clone());

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

    fn wake(&self, account: AccountId) {
        if let Some(w) = self.wakes.lock().unwrap_or_else(|e| e.into_inner()).get(&account) {
            w.poke();
        }
    }

    /// Applies an action locally, tells the views, and queues it for the
    /// server. Returns a token for `undo`.
    pub fn perform(&self, thread: ThreadId, action: Action, context: Context) -> Result<Option<OpId>> {
        let queued = self.kite.store.write(|tx| actions::perform(tx, thread, action, context))?;
        (self.sink)(Event::ListsChanged);
        Ok(queued.map(|(account, op)| {
            self.wake(account);
            op
        }))
    }

    pub fn undo(&self, op: OpId) -> Result<()> {
        let queued = self.kite.store.write(|tx| actions::undo(tx, op))?;
        (self.sink)(Event::ListsChanged);
        if let Some((account, _)) = queued {
            self.wake(account);
        }
        Ok(())
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

    /// Syncs every account now, all mailboxes.
    pub fn sync_now(&self) {
        for w in self.wakes.lock().unwrap_or_else(|e| e.into_inner()).values() {
            w.full_sync.store(true, Ordering::SeqCst);
            w.poke();
        }
    }

    async fn work(self: Arc<Self>, account: Account, wake: Arc<Wake>) {
        let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let mut conn: Option<(Session, Caps)> = None;
        let mut backoff = Backoff::default();
        let mut last_full = Instant::now();
        loop {
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
            let until_send = self
                .kite
                .store
                .read(|c| store::next_send_in(c, account_id))
                .ok()
                .flatten()
                .map_or(until_full, |s| Duration::from_secs(s as u64).min(until_full));
            tokio::select! {
                _ = wake.notify.notified() => {}
                _ = tokio::time::sleep(until_send) => {}
            }
        }
    }

    /// One pass: connect if needed, send ops, sync.
    async fn round(&self, account: &Account, wake: &Wake, conn: &mut Option<(Session, Caps)>, started: i64) -> Result<()> {
        if conn.is_none() {
            let mut session = imap::connect_account(account).await?;
            let caps = imap::caps(&mut session).await?;
            *conn = Some((session, caps));
        }
        let (session, caps) = conn.as_mut().expect("connected above");
        let caps = *caps;

        let mut touched: HashSet<MailboxId> = HashSet::new();
        let account_id = account.id;
        for op in self.kite.store.read_async(move |c| store::pending_ops(c, account_id)).await? {
            match imap::apply_op(&self.kite, session, caps, &op).await {
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

        let sent = self.send_due(account, session).await?;
        if !sent.is_empty() {
            let sent_box = self.role_mailbox(account_id, MailboxRole::Sent).await?;
            touched.extend(sent_box.map(|m| m.id));
        }
        self.sync_drafts(account, session).await?;

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
            (!touched.is_empty()).then(|| Scope::Mailboxes(touched.into_iter().collect()))
        };
        let Some(scope) = scope else { return Ok(()) };

        let full = matches!(scope, Scope::All);
        if full {
            (self.sink)(Event::SyncStarted(account.id));
        }
        let mut last_event = Instant::now();
        let sink = self.sink.clone();
        let report = imap::sync_session(&self.kite, session, caps, account, &scope, SyncOptions { bodies: true }, &mut |_| {
            if last_event.elapsed() > Duration::from_millis(700) {
                last_event = Instant::now();
                sink(Event::ListsChanged);
            }
        })
        .await?;
        if report.changed() {
            (self.sink)(Event::ListsChanged);
        }
        if full {
            (self.sink)(Event::SyncFinished(account.id));
        }
        self.file_sent_copies(account, session, caps, &sent).await?;
        self.announce(account.id, report.new_messages, started).await
    }

    async fn role_mailbox(&self, account: AccountId, role: MailboxRole) -> Result<Option<Mailbox>> {
        self.kite
            .store
            .read_async(move |c| Ok(store::mailboxes(c, account)?.into_iter().find(|m| m.role == Some(role))))
            .await
    }

    /// Sends every outbox message whose undo window has passed.
    async fn send_due(&self, account: &Account, _session: &mut Session) -> Result<Vec<store::OutboxItem>> {
        let account_id = account.id;
        let due = self.kite.store.read_async(move |c| store::due_outbox(c, account_id)).await?;
        let mut sent = Vec::new();
        for item in due {
            let raw = self.kite.blobs.get(&item.blob)?;
            match crate::smtp::send(account, &item.sender, &item.recipients, &raw).await {
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
    async fn sync_drafts(&self, account: &Account, session: &mut Session) -> Result<()> {
        let account_id = account.id;
        let dirty = self.kite.store.read_async(move |c| store::dirty_drafts(c, account_id)).await?;
        if dirty.is_empty() {
            return Ok(());
        }
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
