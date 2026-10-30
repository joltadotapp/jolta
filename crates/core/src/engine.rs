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
use crate::imap::{self, Caps, Scope, Session, SyncOptions};
use crate::model::*;
use crate::store::{self, OpId};
use crate::{Kite, Result};

const FULL_SYNC_EVERY: Duration = Duration::from_secs(10 * 60);
const MAX_ATTEMPTS: u32 = 5;
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
            tokio::select! {
                _ = wake.notify.notified() => {}
                _ = tokio::time::sleep(until_full) => {}
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
        self.announce(account.id, report.new_messages, started).await
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
