//! The Swift-facing API: thin wrappers over `kite_core`. Lists cross the
//! bridge as pages; sync runs on the core's own tokio runtime and reports
//! through `CoreListener`.

use std::sync::{Arc, Mutex};

use kite_core::actions::{Action, Context};
use kite_core::compose::{self, AttachmentFile, Compose, ReplyKind};
use kite_core::engine::{Engine, Event};
use kite_core::store::OpId;
use kite_core::{AccountId, Kite, MailboxId, MessageId, ThreadId, reader, search, store};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("{message}")]
    Query { message: String },
    #[error("{message}")]
    NotFound { message: String },
    #[error("{message}")]
    SignIn { message: String },
    #[error("{message}")]
    Failed { message: String },
}

impl From<kite_core::Error> for CoreError {
    fn from(e: kite_core::Error) -> Self {
        let message = e.to_string();
        match e {
            kite_core::Error::Query(_) => CoreError::Query { message },
            kite_core::Error::NotFound(_) => CoreError::NotFound { message },
            kite_core::Error::Auth(_) => CoreError::SignIn { message },
            _ => CoreError::Failed { message },
        }
    }
}

type Result<T> = std::result::Result<T, CoreError>;

#[derive(uniffi::Record)]
pub struct AccountInfo {
    pub id: i64,
    pub kind: String,
    pub provider: String,
    pub address: String,
    pub display_name: String,
}

#[derive(uniffi::Record)]
pub struct MailboxInfo {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub role: Option<String>,
    pub unread: u32,
}

#[derive(uniffi::Record)]
pub struct ThreadRow {
    pub thread_id: i64,
    pub message_id: i64,
    pub account_id: i64,
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

#[derive(uniffi::Record)]
pub struct ThreadView {
    pub thread_id: i64,
    pub subject: String,
    pub message_count: u32,
    pub html: String,
    pub remote_images: u32,
}

#[derive(uniffi::Enum)]
pub enum CoreEvent {
    /// Stored mail changed; visible lists should reload their counts and pages.
    ListsChanged,
    SyncStarted { account_id: i64 },
    SyncFinished { account_id: i64 },
    SyncFailed { account_id: i64, message: String },
    NewMail { account_id: i64, thread_id: i64, from: String, subject: String },
    Sent { account_id: i64, subject: String },
    SendFailed { message: String },
    /// The server refused an action for good; the change was rolled back.
    ActionFailed { message: String },
}

#[derive(uniffi::Record, Clone)]
pub struct ComposeFile {
    pub name: String,
    pub mime_type: String,
    pub path: String,
}

/// A message being written. Recipient fields are comma-separated, as typed.
#[derive(uniffi::Record, Clone)]
pub struct ComposeData {
    pub account_id: Option<i64>,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub replying_to: Option<i64>,
    pub attachments: Vec<ComposeFile>,
    pub draft_id: Option<i64>,
}

impl From<Compose> for ComposeData {
    fn from(c: Compose) -> Self {
        ComposeData {
            account_id: c.account.map(|a| a.0),
            to: c.to,
            cc: c.cc,
            bcc: c.bcc,
            subject: c.subject,
            body: c.body,
            in_reply_to: c.in_reply_to,
            references: c.references,
            replying_to: c.replying_to.map(|m| m.0),
            attachments: c
                .attachments
                .into_iter()
                .map(|a| ComposeFile { name: a.name, mime_type: a.mime_type, path: a.path })
                .collect(),
            draft_id: c.draft,
        }
    }
}

impl From<ComposeData> for Compose {
    fn from(c: ComposeData) -> Self {
        Compose {
            account: c.account_id.map(AccountId),
            to: c.to,
            cc: c.cc,
            bcc: c.bcc,
            subject: c.subject,
            body: c.body,
            in_reply_to: c.in_reply_to,
            references: c.references,
            replying_to: c.replying_to.map(MessageId),
            attachments: c
                .attachments
                .into_iter()
                .map(|a| AttachmentFile { name: a.name, mime_type: a.mime_type, path: a.path })
                .collect(),
            draft: c.draft_id,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum ReplyMode {
    Reply,
    ReplyAll,
    Forward,
}

#[derive(uniffi::Record)]
pub struct AttachmentData {
    pub name: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[derive(uniffi::Enum)]
pub enum ThreadAction {
    Archive,
    Trash,
    MarkRead,
    MarkUnread,
    Flag,
    Unflag,
    MoveTo { mailbox_id: i64 },
}

/// The list an action is taken from.
#[derive(uniffi::Enum)]
pub enum ListContext {
    Inbox,
    /// A role across accounts, such as "sent" or "archive".
    Role { role: String },
    Mailbox { mailbox_id: i64 },
}

impl ListContext {
    fn into_core(self) -> Result<Context> {
        Ok(match self {
            ListContext::Inbox => Context::Inbox,
            ListContext::Role { role } => Context::Role(role.parse().map_err(|message| CoreError::Failed { message })?),
            ListContext::Mailbox { mailbox_id } => Context::Mailbox(MailboxId(mailbox_id)),
        })
    }
}

impl From<ThreadAction> for Action {
    fn from(a: ThreadAction) -> Action {
        match a {
            ThreadAction::Archive => Action::Archive,
            ThreadAction::Trash => Action::Trash,
            ThreadAction::MarkRead => Action::MarkRead,
            ThreadAction::MarkUnread => Action::MarkUnread,
            ThreadAction::Flag => Action::Flag,
            ThreadAction::Unflag => Action::Unflag,
            ThreadAction::MoveTo { mailbox_id } => Action::MoveTo(MailboxId(mailbox_id)),
        }
    }
}

#[uniffi::export(with_foreign)]
pub trait CoreListener: Send + Sync {
    fn on_event(&self, event: CoreEvent);
}

#[uniffi::export]
pub fn core_greeting() -> String {
    kite_core::greeting()
}

#[derive(uniffi::Object)]
pub struct Core {
    kite: Kite,
    runtime: tokio::runtime::Runtime,
    listener: Arc<dyn CoreListener>,
    engine: Mutex<Option<Arc<Engine>>>,
    /// The last list's full thread order, so pages deep in a long list are
    /// slices instead of re-scans. Cleared whenever stored mail changes.
    order: Arc<Mutex<Option<(String, Arc<Vec<(ThreadId, MessageId)>>)>>>,
}

impl Core {
    fn order(&self, query: &str) -> Result<Arc<Vec<(ThreadId, MessageId)>>> {
        let mut cache = self.order.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((q, order)) = cache.as_ref() {
            if q == query {
                return Ok(order.clone());
            }
        }
        let q = search::parse(query)?;
        let order = Arc::new(self.kite.store.read(|c| search::query_order(c, &q))?);
        *cache = Some((query.to_string(), order.clone()));
        Ok(order)
    }
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn open(data_dir: String, listener: Arc<dyn CoreListener>) -> Result<Arc<Core>> {
        let kite = Kite::open(std::path::Path::new(&data_dir))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("kite-core")
            .enable_all()
            .build()
            .map_err(|e| CoreError::Failed { message: e.to_string() })?;
        Ok(Arc::new(Core {
            kite,
            runtime,
            listener,
            engine: Mutex::new(None),
            order: Arc::new(Mutex::new(None)),
        }))
    }

    pub fn accounts(&self) -> Result<Vec<AccountInfo>> {
        Ok(self
            .kite
            .store
            .read(store::accounts)?
            .into_iter()
            .map(|a| AccountInfo {
                id: a.id.0,
                kind: a.kind.to_string(),
                provider: a.provider_label().to_string(),
                address: a.address,
                display_name: a.display_name,
            })
            .collect())
    }

    pub fn mailboxes(&self, account_id: i64) -> Result<Vec<MailboxInfo>> {
        self.kite.store.read(|c| {
            let boxes = store::mailboxes(c, AccountId(account_id))?;
            let mut unread = c.prepare_cached(
                "SELECT count(*) FROM message_mailbox mm JOIN message m ON m.id = mm.message_id
                 WHERE mm.mailbox_id = ?1 AND (m.flags & 1) = 0",
            )?;
            boxes
                .into_iter()
                .filter(|b| b.selectable)
                .map(|b| {
                    Ok(MailboxInfo {
                        id: b.id.0,
                        account_id: b.account_id.0,
                        unread: unread.query_row([b.id], |r| r.get(0))?,
                        name: b.name,
                        role: b.role.map(|r| r.to_string()),
                    })
                })
                .collect()
        })
        .map_err(Into::into)
    }

    pub fn query_count(&self, query: String) -> Result<u32> {
        Ok(self.order(&query)?.len() as u32)
    }

    pub fn query_page(&self, query: String, offset: u32, limit: u32) -> Result<Vec<ThreadRow>> {
        let order = self.order(&query)?;
        let start = (offset as usize).min(order.len());
        let end = (start + limit as usize).min(order.len());
        let rows = self.kite.store.read(|c| search::thread_rows(c, &order[start..end]))?;
        Ok(rows
            .into_iter()
            .map(|r| ThreadRow {
                thread_id: r.thread_id.0,
                message_id: r.message_id.0,
                account_id: r.account_id.0,
                date: r.date,
                from_name: r.from_name,
                from_addr: r.from_addr,
                subject: r.subject,
                snippet: r.snippet,
                message_count: r.message_count,
                unread_count: r.unread_count,
                flagged: r.flagged,
                has_attachments: r.has_attachments,
            })
            .collect())
    }

    pub async fn open_thread(&self, thread_id: i64, app_css: String, utc_offset: i32) -> Result<ThreadView> {
        let kite = self.kite.clone();
        let detail = self
            .runtime
            .spawn(async move { reader::open_thread(&kite, ThreadId(thread_id), &app_css, utc_offset).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(ThreadView {
            thread_id,
            subject: detail.subject,
            message_count: detail.messages.len() as u32,
            html: detail.rendered.html,
            remote_images: detail.rendered.remote_images,
        })
    }

    /// Starts syncing every account and keeps it up to date (IDLE, queued
    /// actions, periodic full syncs) until the app quits.
    pub fn start(&self) -> Result<()> {
        let mut engine = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        if engine.is_some() {
            return Ok(());
        }
        let (order, listener) = (self.order.clone(), self.listener.clone());
        let sink: kite_core::engine::Sink = Arc::new(move |event| {
            let event = match event {
                Event::ListsChanged => {
                    *order.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    CoreEvent::ListsChanged
                }
                Event::SyncStarted(a) => CoreEvent::SyncStarted { account_id: a.0 },
                Event::SyncFinished(a) => CoreEvent::SyncFinished { account_id: a.0 },
                Event::SyncFailed(a, message) => CoreEvent::SyncFailed { account_id: a.0, message },
                Event::NewMail { account, thread, from, subject } => {
                    CoreEvent::NewMail { account_id: account.0, thread_id: thread.0, from, subject }
                }
                Event::ActionFailed(message) => CoreEvent::ActionFailed { message },
                Event::Sent { account, subject } => CoreEvent::Sent { account_id: account.0, subject },
                Event::SendFailed(message) => CoreEvent::SendFailed { message },
            };
            listener.on_event(event);
        });
        *engine = Some(Engine::start(self.kite.clone(), sink, self.runtime.handle().clone())?);
        Ok(())
    }

    /// Applies an action to a thread right away and sends it to the server.
    /// Returns a token for `undo`, or nothing when nothing changed.
    pub fn perform(&self, action: ThreadAction, thread_id: i64, context: ListContext) -> Result<Option<i64>> {
        let engine = self.running_engine()?;
        Ok(engine.perform(ThreadId(thread_id), action.into(), context.into_core()?)?.map(|op| op.0))
    }

    pub fn undo(&self, token: i64) -> Result<()> {
        Ok(self.running_engine()?.undo(OpId(token))?)
    }

    /// A blank message from the first account that can send.
    pub fn new_compose(&self) -> Result<ComposeData> {
        let account = self.kite.store.read(store::accounts)?.into_iter().find(|a| !a.smtp_host.is_empty());
        Ok(Compose { account: account.map(|a| a.id), ..Default::default() }.into())
    }

    /// A reply, reply-all or forward of the newest message in a thread.
    pub async fn reply_compose(&self, thread_id: i64, mode: ReplyMode, utc_offset: i32) -> Result<ComposeData> {
        let kite = self.kite.clone();
        self.runtime
            .spawn(async move {
                let message: MessageId = kite
                    .store
                    .read_async(move |c| Ok(c.query_row("SELECT max(id) FROM message WHERE thread_id = ?1", [thread_id], |r| r.get(0))?))
                    .await?;
                let raw = kite_core::imap::message_source(&kite, message).await?;
                let kind = match mode {
                    ReplyMode::Reply => ReplyKind::Reply,
                    ReplyMode::ReplyAll => ReplyKind::ReplyAll,
                    ReplyMode::Forward => ReplyKind::Forward,
                };
                kite.store.read_async(move |c| compose::reply(c, message, &raw, kind, utc_offset)).await
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queues a message; it leaves after the undo window. Returns a token
    /// for `cancel_send`.
    pub fn send(&self, compose: ComposeData) -> Result<i64> {
        Ok(self.running_engine()?.send(&compose.into())?)
    }

    /// Takes a waiting message back and returns it for editing.
    pub fn cancel_send(&self, token: i64) -> Result<Option<ComposeData>> {
        Ok(self.running_engine()?.cancel_send(token)?.map(Into::into))
    }

    pub fn save_draft(&self, compose: ComposeData) -> Result<i64> {
        Ok(self.running_engine()?.save_draft(&compose.into())?)
    }

    pub fn discard_draft(&self, draft_id: i64) -> Result<()> {
        Ok(self.running_engine()?.discard_draft(draft_id)?)
    }

    pub fn drafts(&self) -> Result<Vec<ComposeData>> {
        Ok(self.kite.store.read(store::drafts)?.into_iter().map(Into::into).collect())
    }

    /// The bytes of one attachment of a message, for opening or saving.
    pub async fn attachment(&self, message_id: i64, index: u32) -> Result<AttachmentData> {
        let kite = self.kite.clone();
        self.runtime
            .spawn(async move {
                let raw = kite_core::imap::message_source(&kite, MessageId(message_id)).await?;
                kite_core::render::attachment_bytes(&raw, index as usize)
                    .ok_or_else(|| kite_core::Error::NotFound(format!("attachment {index}")))
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map(|(name, mime_type, bytes)| AttachmentData { name, mime_type, bytes })
            .map_err(Into::into)
    }

    pub fn sync_now(&self) {
        if let Some(engine) = self.engine.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            engine.sync_now();
        }
    }
}

impl Core {
    fn running_engine(&self) -> Result<Arc<Engine>> {
        self.engine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| CoreError::Failed { message: "the core has not been started".into() })
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // The runtime may not block inside an async context; let it go quietly.
        let rt = std::mem::replace(
            &mut self.runtime,
            tokio::runtime::Builder::new_current_thread().build().expect("an empty runtime builds"),
        );
        rt.shutdown_background();
    }
}
