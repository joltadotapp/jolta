//! The Swift-facing API: thin wrappers over `jolta_core`. Lists cross the
//! bridge as pages; sync runs on the core's own tokio runtime and reports
//! through `CoreListener`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jolta_core::actions::{Action, Context};
use jolta_core::compose::{self, AttachmentFile, Compose, ReplyKind};
use jolta_core::engine::{Engine, Event};
use jolta_core::store::OpId;
use jolta_core::people::{self, People};
use jolta_core::rich;
use jolta_core::{AccountId, Jolta, MailboxId, MessageId, ThreadId, reader, search, store};

uniffi::setup_scaffolding!();

mod ai;
pub mod calendar;

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

impl From<jolta_core::Error> for CoreError {
    fn from(e: jolta_core::Error) -> Self {
        let message = e.to_string();
        match e {
            jolta_core::Error::Query(_) => CoreError::Query { message },
            jolta_core::Error::NotFound(_) => CoreError::NotFound { message },
            jolta_core::Error::Auth(_) => CoreError::SignIn { message },
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

impl From<jolta_core::Account> for AccountInfo {
    fn from(a: jolta_core::Account) -> Self {
        AccountInfo {
            id: a.id.0,
            kind: a.kind.to_string(),
            provider: a.provider_label().to_string(),
            address: a.address,
            display_name: a.display_name,
        }
    }
}

#[derive(uniffi::Record)]
pub struct MailboxInfo {
    pub id: i64,
    pub account_id: i64,
    pub name: String,
    pub role: Option<String>,
    pub unread: u32,
    pub hidden: bool,
    /// False until a sync has gone through it once.
    pub synced: bool,
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
    pub vip: bool,
    pub needs_reply: bool,
}

#[derive(uniffi::Record)]
pub struct ThreadView {
    pub thread_id: i64,
    pub subject: String,
    pub message_count: u32,
    pub html: String,
    pub remote_images: u32,
    /// The one sender of this conversation's remote images, or empty.
    pub image_sender: String,
    pub images_trusted: bool,
    /// The conversation comes from a mailing list that can be left.
    pub mailing_list: bool,
}

#[derive(uniffi::Enum)]
pub enum UnsubscribeResult {
    Unsubscribed { list: String },
    /// An unsubscribe email to `to` is in the outbox.
    Emailed { list: String, to: String },
    /// The list only offers a page where the reader confirms by hand.
    OpenPage { list: String, url: String },
    NotAList,
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum ConnectionSecurity {
    Tls,
    StartTls,
    Plain,
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum SmtpLogin {
    SameAsIncoming,
    Separate,
    NoAuth,
}

/// An account's connection settings, for the settings screen.
#[derive(uniffi::Record)]
pub struct AccountSettingsInfo {
    pub display_name: String,
    pub address: String,
    pub username: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: ConnectionSecurity,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: ConnectionSecurity,
    pub smtp_login: SmtpLogin,
    pub smtp_username: String,
    pub verify_certs: bool,
}

impl From<jolta_core::AccountSettings> for AccountSettingsInfo {
    fn from(s: jolta_core::AccountSettings) -> Self {
        let security = |s: jolta_core::Security| match s {
            jolta_core::Security::Tls => ConnectionSecurity::Tls,
            jolta_core::Security::StartTls => ConnectionSecurity::StartTls,
            jolta_core::Security::Plain => ConnectionSecurity::Plain,
        };
        AccountSettingsInfo {
            display_name: s.display_name,
            address: s.address,
            username: s.username,
            imap_host: s.imap_host,
            imap_port: s.imap_port,
            imap_security: security(s.imap_security),
            smtp_host: s.smtp_host,
            smtp_port: s.smtp_port,
            smtp_security: security(s.smtp_security),
            smtp_login: match s.smtp_auth {
                jolta_core::SmtpAuth::SameAsIncoming => SmtpLogin::SameAsIncoming,
                jolta_core::SmtpAuth::Separate => SmtpLogin::Separate,
                jolta_core::SmtpAuth::NoAuth => SmtpLogin::NoAuth,
            },
            smtp_username: s.smtp_username,
            verify_certs: s.verify_certs,
        }
    }
}

impl From<AccountSettingsInfo> for jolta_core::AccountSettings {
    fn from(s: AccountSettingsInfo) -> Self {
        let security = |s: ConnectionSecurity| match s {
            ConnectionSecurity::Tls => jolta_core::Security::Tls,
            ConnectionSecurity::StartTls => jolta_core::Security::StartTls,
            ConnectionSecurity::Plain => jolta_core::Security::Plain,
        };
        jolta_core::AccountSettings {
            display_name: s.display_name,
            address: s.address,
            username: s.username,
            imap_host: s.imap_host,
            imap_port: s.imap_port,
            imap_security: security(s.imap_security),
            smtp_host: s.smtp_host,
            smtp_port: s.smtp_port,
            smtp_security: security(s.smtp_security),
            smtp_auth: match s.smtp_login {
                SmtpLogin::SameAsIncoming => jolta_core::SmtpAuth::SameAsIncoming,
                SmtpLogin::Separate => jolta_core::SmtpAuth::Separate,
                SmtpLogin::NoAuth => jolta_core::SmtpAuth::NoAuth,
            },
            smtp_username: s.smtp_username,
            verify_certs: s.verify_certs,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum CoreEvent {
    /// Stored mail changed; visible lists should reload their counts and pages.
    ListsChanged,
    SyncStarted { account_id: i64 },
    SyncFinished { account_id: i64 },
    SyncFailed { account_id: i64, message: String },
    NewMail { account_id: i64, thread_id: i64, from: String, subject: String, vip: bool },
    Sent { account_id: i64, subject: String },
    /// A snoozed thread came back to the inbox.
    Woke { account_id: i64, thread_id: i64, subject: String },
    /// Nobody answered in time; the thread is back in the inbox.
    NoReply { account_id: i64, thread_id: i64, subject: String },
    /// An agent connected for the first time; ask the user to approve it.
    AgentRequest { agent_id: i64, name: String },
    /// An agent wrote a draft for the user to review.
    AgentDraft { draft_id: i64, subject: String },
    /// An agent used a tool; the audit log changed.
    AgentActivity,
    SendFailed { message: String },
    /// The server refused an action for good; the change was rolled back.
    ActionFailed { message: String },
    /// A local model's download moved on, finished, stopped or was removed.
    ModelChanged { id: String, state: ai::ModelState },
    /// The occurrences of these calendars changed; redraw views that show them.
    CalendarChanged { calendar_ids: Vec<i64> },
    /// A calendar sync ended. `error` is None when it reached the server.
    CalendarSynced { calendar_id: i64, error: Option<String> },
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

/// How the message list is sorted. Each key has a natural direction: dates
/// newest first, senders and subjects A to Z. `reversed` flips it.
#[derive(uniffi::Record, Clone, Copy, PartialEq, Eq)]
pub struct ListSort {
    pub key: SortKey,
    pub reversed: bool,
}

#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Date,
    Sender,
    Subject,
}

impl From<ListSort> for search::Sort {
    fn from(s: ListSort) -> search::Sort {
        let key = match s.key {
            SortKey::Date => search::SortKey::Date,
            SortKey::Sender => search::SortKey::Sender,
            SortKey::Subject => search::SortKey::Subject,
        };
        search::Sort { key, reversed: s.reversed }
    }
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
    /// Hide the thread until this unix time.
    Snooze { until: i64 },
}

#[derive(uniffi::Record, Clone)]
pub struct SnippetInfo {
    pub id: i64,
    pub name: String,
    pub body: String,
}

/// A run of message text with one set of styles.
#[derive(uniffi::Record)]
pub struct TextSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub link: Option<String>,
}

impl From<rich::Span> for TextSpan {
    fn from(s: rich::Span) -> Self {
        TextSpan { text: s.text, bold: s.bold, italic: s.italic, strike: s.strike, link: s.link }
    }
}

impl From<TextSpan> for rich::Span {
    fn from(s: TextSpan) -> Self {
        rich::Span { text: s.text, bold: s.bold, italic: s.italic, strike: s.strike, link: s.link }
    }
}

#[derive(uniffi::Record)]
pub struct PersonInfo {
    pub name: String,
    pub address: String,
}

#[derive(uniffi::Record, Clone)]
pub struct SectionInfo {
    pub id: i64,
    pub name: String,
    pub query: String,
}

#[derive(uniffi::Record)]
pub struct SnoozeInfo {
    pub thread_id: i64,
    pub account_id: i64,
    pub wake_at: i64,
    pub subject: String,
}

/// A message set to go out later.
#[derive(uniffi::Record)]
pub struct ScheduledInfo {
    /// For `cancel_send`, which takes it back for editing.
    pub token: i64,
    pub account_id: i64,
    pub subject: String,
    pub send_at: i64,
}

#[derive(uniffi::Record)]
pub struct AgentInfo {
    pub id: i64,
    pub name: String,
    /// pending, approved or denied
    pub state: String,
    /// read, organize, draft
    pub grants: Vec<String>,
    pub last_seen: i64,
}

#[derive(uniffi::Record)]
pub struct AuditInfo {
    pub agent: String,
    pub tool: String,
    pub summary: String,
    pub outcome: String,
    pub at: i64,
}

#[derive(uniffi::Record)]
pub struct SnoozeChoice {
    pub label: String,
    pub at: i64,
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
            ThreadAction::Snooze { until } => Action::Snooze(until),
        }
    }
}

#[uniffi::export(with_foreign)]
pub trait CoreListener: Send + Sync {
    fn on_event(&self, event: CoreEvent);
}

#[uniffi::export]
pub fn core_greeting() -> String {
    jolta_core::greeting()
}

#[derive(uniffi::Enum)]
pub enum OAuthProvider {
    Google,
    Microsoft,
}

/// A browser sign-in in progress. Open `url()`, then await `finish()`.
#[derive(uniffi::Object)]
pub struct SignIn {
    url: String,
    provider: jolta_core::oauth::Provider,
    pending: Mutex<Option<jolta_core::oauth::Pending>>,
    jolta: Jolta,
    runtime: tokio::runtime::Handle,
    engine: Option<Arc<Engine>>,
}

#[uniffi::export]
impl SignIn {
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Waits for the browser to come back, then adds (or re-authorizes) the account.
    pub async fn finish(&self) -> Result<AccountInfo> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| CoreError::Failed { message: "this sign-in already finished".into() })?;
        let (jolta, provider, engine) = (self.jolta.clone(), self.provider, self.engine.clone());
        self.runtime
            .spawn(async move {
                let signed = pending
                    .finish()
                    .await
                    .map_err(|e| jolta_core::Error::Auth(jolta_core::graph::explain_sign_in_error(&e.to_string())))?;
                let account = jolta_core::oauth::save_account(&jolta, provider, &signed)?;
                if let Some(engine) = engine {
                    engine.add_account(account.clone());
                }
                Ok::<_, jolta_core::Error>(AccountInfo::from(account))
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map_err(Into::into)
    }
}

#[derive(uniffi::Object)]
pub struct Core {
    jolta: Jolta,
    runtime: tokio::runtime::Runtime,
    listener: Arc<dyn CoreListener>,
    engine: Mutex<Option<Arc<Engine>>>,
    engine_sink: Mutex<Option<jolta_core::engine::Sink>>,
    /// The last list's full thread order, so pages deep in a long list are
    /// slices instead of re-scans. Cleared whenever stored mail changes.
    order: Arc<Mutex<Option<(String, ListSort, Arc<Vec<(ThreadId, MessageId)>>)>>>,
    /// Address suggestions, rebuilt when older than `PEOPLE_FRESH` or when
    /// the address book or forgotten addresses change.
    people: Arc<Mutex<Option<(Instant, Arc<People>)>>>,
    address_book: Arc<Mutex<Vec<(String, String)>>>,
    ai: Arc<jolta_core::ai::Ai>,
    downloads: ai::Downloads,
    /// Sorting new mail and summaries made ahead, one job at a time.
    background: Arc<jolta_core::ai::background::Background>,
}

const PEOPLE_FRESH: Duration = Duration::from_secs(120);

fn build_people(jolta: &Jolta, book: &Mutex<Vec<(String, String)>>) -> jolta_core::Result<People> {
    let book = book.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    jolta.store.read(|c| People::build(c, now, &book))
}

impl Core {
    fn order(&self, query: &str, sort: ListSort) -> Result<Arc<Vec<(ThreadId, MessageId)>>> {
        let mut cache = self.order.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((q, s, order)) = cache.as_ref() {
            if q == query && *s == sort {
                return Ok(order.clone());
            }
        }
        let q = search::parse(query)?;
        let order = Arc::new(self.jolta.store.read(|c| search::query_order(c, &q, sort.into()))?);
        *cache = Some((query.to_string(), sort, order.clone()));
        Ok(order)
    }
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn open(data_dir: String, listener: Arc<dyn CoreListener>) -> Result<Arc<Core>> {
        let jolta = Jolta::open(std::path::Path::new(&data_dir))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("jolta-core")
            .enable_all()
            .build()
            .map_err(|e| CoreError::Failed { message: e.to_string() })?;
        let ai = Arc::new(jolta_core::ai::Ai::new(&jolta.data_dir, ai::helper_path()));
        let order: Arc<Mutex<Option<(String, ListSort, Arc<Vec<(ThreadId, MessageId)>>)>>> = Arc::new(Mutex::new(None));
        let changed: jolta_core::ai::background::Changed = {
            let (order, listener) = (order.clone(), listener.clone());
            Arc::new(move || {
                *order.lock().unwrap_or_else(|e| e.into_inner()) = None;
                listener.on_event(CoreEvent::ListsChanged);
            })
        };
        let background = jolta_core::ai::background::Background::start(jolta.clone(), ai.clone(), changed, runtime.handle());
        Ok(Arc::new(Core {
            runtime,
            listener,
            engine: Mutex::new(None),
            engine_sink: Mutex::new(None),
            order,
            people: Arc::new(Mutex::new(None)),
            address_book: Arc::new(Mutex::new(Vec::new())),
            ai,
            downloads: Arc::default(),
            background,
            jolta,
        }))
    }

    pub fn accounts(&self) -> Result<Vec<AccountInfo>> {
        Ok(self
            .jolta
            .store
            .read(store::accounts)?
            .into_iter()
            .map(AccountInfo::from)
            .collect())
    }

    pub fn mailboxes(&self, account_id: i64) -> Result<Vec<MailboxInfo>> {
        self.jolta.store.read(|c| {
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
                        hidden: b.hidden,
                        synced: b.synced,
                    })
                })
                .collect()
        })
        .map_err(Into::into)
    }

    /// Threads matching a query, for badges. Leaves the list cache alone.
    pub fn query_count(&self, query: String) -> Result<u32> {
        if let Some((cached, _, order)) = self.order.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            if *cached == query {
                return Ok(order.len() as u32);
            }
        }
        let q = search::parse(&query)?;
        Ok(self.jolta.store.read(|c| search::query_count(c, &q))?)
    }

    /// Length of the list being shown; caches its order for `query_page`.
    pub fn list_count(&self, query: String, sort: ListSort) -> Result<u32> {
        Ok(self.order(&query, sort)?.len() as u32)
    }

    pub fn query_page(&self, query: String, sort: ListSort, offset: u32, limit: u32) -> Result<Vec<ThreadRow>> {
        let order = self.order(&query, sort)?;
        let rows = self.jolta.store.read(|c| search::thread_rows(c, slice(&order, offset, limit)))?;
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
                vip: r.vip,
                needs_reply: r.needs_reply,
            })
            .collect())
    }

    /// Thread ids for a run of list rows, without loading the rows: a
    /// selection of thousands costs a slice of the cached order.
    pub fn list_thread_ids(&self, query: String, sort: ListSort, offset: u32, limit: u32) -> Result<Vec<i64>> {
        Ok(slice(&self.order(&query, sort)?, offset, limit).iter().map(|(t, _)| t.0).collect())
    }

    /// Where these threads sit in the list now, in list order. Threads that
    /// left the list are skipped.
    pub fn list_positions(&self, query: String, sort: ListSort, thread_ids: Vec<i64>) -> Result<Vec<u32>> {
        let wanted: std::collections::HashSet<i64> = thread_ids.into_iter().collect();
        let order = self.order(&query, sort)?;
        Ok(order.iter().enumerate().filter(|(_, (t, _))| wanted.contains(&t.0)).map(|(i, _)| i as u32).collect())
    }

    pub async fn open_thread(&self, thread_id: i64, app_css: String, utc_offset: i32) -> Result<ThreadView> {
        let jolta = self.jolta.clone();
        let detail = self
            .runtime
            .spawn(async move { reader::open_thread(&jolta, ThreadId(thread_id), &app_css, utc_offset).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(ThreadView {
            thread_id,
            subject: detail.subject,
            message_count: detail.messages.len() as u32,
            html: detail.rendered.html,
            remote_images: detail.rendered.remote_images,
            image_sender: detail.image_sender.unwrap_or_default(),
            images_trusted: detail.images_trusted,
            mailing_list: self.jolta.store.read(|c| {
                Ok(c.query_row(
                    "SELECT EXISTS (SELECT 1 FROM message WHERE thread_id = ?1 AND list_unsubscribe IS NOT NULL)",
                    [thread_id],
                    |r| r.get(0),
                )?)
            })?,
        })
    }

    /// Leaves the mailing list behind a conversation: a one-click request,
    /// an unsubscribe email from the account that received it, or, when the
    /// list only offers a web page, that page for the app to open.
    pub async fn unsubscribe(&self, thread_id: i64) -> Result<UnsubscribeResult> {
        let jolta = self.jolta.clone();
        let engine = self.running_engine()?;
        self.runtime
            .spawn(async move {
                let found: Option<(MessageId, AccountId, String)> = jolta
                    .store
                    .read_async(move |c| {
                        let row = c.query_row(
                            "SELECT id, account_id, CASE from_name WHEN '' THEN from_addr ELSE from_name END FROM message
                             WHERE thread_id = ?1 ORDER BY list_unsubscribe IS NULL, date DESC LIMIT 1",
                            [thread_id],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        );
                        Ok(row.ok())
                    })
                    .await?;
                let Some((message, account, list)) = found else { return Ok(UnsubscribeResult::NotAList) };
                let raw = jolta_core::imap::message_source(&jolta, message).await?;
                Ok::<_, jolta_core::Error>(match jolta_core::unsubscribe::method(&raw) {
                    None => UnsubscribeResult::NotAList,
                    Some(jolta_core::unsubscribe::Method::OneClick(url)) => {
                        jolta_core::unsubscribe::one_click(&url).await?;
                        UnsubscribeResult::Unsubscribed { list }
                    }
                    Some(jolta_core::unsubscribe::Method::Mail { to, subject, body }) => {
                        engine.send(&Compose { account: Some(account), to: to.clone(), subject, body, ..Default::default() }, None, None)?;
                        UnsubscribeResult::Emailed { list, to }
                    }
                    Some(jolta_core::unsubscribe::Method::Page(url)) => UnsubscribeResult::OpenPage { list, url },
                })
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map_err(Into::into)
    }

    /// Starts syncing every account and keeps it up to date (IDLE, queued
    /// actions, periodic full syncs) until the app quits.
    pub fn start(&self) -> Result<()> {
        let mut engine = self.engine.lock().unwrap_or_else(|e| e.into_inner());
        if engine.is_some() {
            return Ok(());
        }
        let (order, listener) = (self.order.clone(), self.listener.clone());
        let (background, jolta) = (self.background.clone(), self.jolta.clone());
        let sink: jolta_core::engine::Sink = Arc::new(move |event| {
            let event = match event {
                Event::ListsChanged => {
                    *order.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    CoreEvent::ListsChanged
                }
                Event::SyncStarted(a) => CoreEvent::SyncStarted { account_id: a.0 },
                Event::SyncFinished(a) => CoreEvent::SyncFinished { account_id: a.0 },
                Event::SyncFailed(a, message) => CoreEvent::SyncFailed { account_id: a.0, message },
                Event::NewMail { account, thread, from, subject, vip } => {
                    background.new_mail(&jolta, thread);
                    CoreEvent::NewMail { account_id: account.0, thread_id: thread.0, from, subject, vip }
                }
                Event::ActionFailed(message) => CoreEvent::ActionFailed { message },
                Event::Sent { account, subject } => CoreEvent::Sent { account_id: account.0, subject },
                Event::NoReply { account, thread, subject } => {
                    CoreEvent::NoReply { account_id: account.0, thread_id: thread.0, subject }
                }
                Event::Woke { account, thread, subject } => {
                    CoreEvent::Woke { account_id: account.0, thread_id: thread.0, subject }
                }
                Event::AgentRequest { agent, name } => CoreEvent::AgentRequest { agent_id: agent, name },
                Event::AgentDraft { draft, subject } => CoreEvent::AgentDraft { draft_id: draft, subject },
                Event::AgentActivity => CoreEvent::AgentActivity,
                Event::SendFailed(message) => CoreEvent::SendFailed { message },
                Event::CalendarsChanged(ids) => CoreEvent::CalendarChanged { calendar_ids: ids.into_iter().map(|c| c.0).collect() },
                Event::CalendarSynced { calendar, error } => CoreEvent::CalendarSynced { calendar_id: calendar.0, error },
            };
            listener.on_event(event);
        });
        *self.engine_sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink.clone());
        *engine = Some(Engine::start(self.jolta.clone(), sink, self.runtime.handle().clone())?);
        self.background.catch_up(&self.jolta);
        Ok(())
    }

    /// Applies an action to a thread right away and sends it to the server.
    /// Returns a token for `undo`, or nothing when nothing changed.
    /// Acts on every thread at once. Returns undo tokens, empty when nothing changed.
    pub fn perform(&self, action: ThreadAction, thread_ids: Vec<i64>, context: ListContext) -> Result<Vec<i64>> {
        let engine = self.running_engine()?;
        let threads: Vec<ThreadId> = thread_ids.into_iter().map(ThreadId).collect();
        Ok(engine.perform(&threads, action.into(), context.into_core()?)?.into_iter().map(|op| op.0).collect())
    }

    /// Marks every unread message in the list read. Returns undo tokens.
    pub fn mark_all_read(&self, query: String) -> Result<Vec<i64>> {
        let q = search::parse(&format!("{query} is:unread"))?;
        Ok(self.running_engine()?.mark_all_read(&q)?.into_iter().map(|op| op.0).collect())
    }

    /// Deletes everything in these folders for good, here and on the server.
    pub fn empty_mailboxes(&self, mailbox_ids: Vec<i64>) -> Result<()> {
        let ids: Vec<MailboxId> = mailbox_ids.into_iter().map(MailboxId).collect();
        Ok(self.running_engine()?.empty(&ids)?)
    }

    pub fn undo(&self, tokens: Vec<i64>) -> Result<()> {
        let ops: Vec<OpId> = tokens.into_iter().map(OpId).collect();
        Ok(self.running_engine()?.undo(&ops)?)
    }

    /// A blank message from the first account that can send.
    pub fn new_compose(&self) -> Result<ComposeData> {
        let account = self
            .jolta
            .store
            .read(store::accounts)?
            .into_iter()
            .find(|a| !a.smtp_host.is_empty() && a.provider_label() != "Synthetic");
        let body = account.as_ref().map(|a| compose::opening(&a.signature, "")).unwrap_or_default();
        Ok(Compose { account: account.map(|a| a.id), body, ..Default::default() }.into())
    }

    /// The body with the old sender's signature swapped for the new one's.
    pub fn swap_signature(&self, body: String, from_account: Option<i64>, to_account: Option<i64>) -> Result<String> {
        let signature = |id: Option<i64>| -> Result<String> {
            Ok(match id {
                Some(id) => self.jolta.store.read(|c| store::account(c, AccountId(id)))?.signature,
                None => String::new(),
            })
        };
        Ok(compose::swap_signature(&body, &signature(from_account)?, &signature(to_account)?))
    }

    pub fn signature(&self, account_id: i64) -> Result<String> {
        Ok(self.jolta.store.read(|c| store::account(c, AccountId(account_id)))?.signature)
    }

    pub fn set_signature(&self, account_id: i64, signature: String) -> Result<()> {
        Ok(self.jolta.store.write(|tx| store::set_signature(tx, AccountId(account_id), &signature))?)
    }

    pub fn snippets(&self) -> Result<Vec<SnippetInfo>> {
        Ok(self.jolta.store.read(store::snippets)?.into_iter().map(|s| SnippetInfo { id: s.id, name: s.name, body: s.body }).collect())
    }

    /// Saves a snippet; id 0 adds one. Returns its id.
    pub fn save_snippet(&self, snippet: SnippetInfo) -> Result<i64> {
        let s = store::Snippet { id: snippet.id, name: snippet.name, body: snippet.body };
        Ok(self.jolta.store.write(|tx| store::save_snippet(tx, &s))?)
    }

    pub fn delete_snippet(&self, id: i64) -> Result<()> {
        Ok(self.jolta.store.write(|tx| store::delete_snippet(tx, id))?)
    }

    /// A snippet's body filled in for the message's first recipient.
    pub fn expand_snippet(&self, body: String, to: String) -> String {
        compose::expand_snippet(&body, &to)
    }

    /// A body as styled runs, for the editor.
    pub fn body_spans(&self, body: String) -> Vec<TextSpan> {
        rich::spans(&body).into_iter().map(Into::into).collect()
    }

    /// The editor's styled runs as a body.
    pub fn body_markdown(&self, spans: Vec<TextSpan>) -> String {
        rich::markdown(&spans.into_iter().map(Into::into).collect::<Vec<_>>())
    }

    /// A reply, reply-all or forward of the newest message in a thread.
    pub async fn reply_compose(&self, thread_id: i64, mode: ReplyMode, utc_offset: i32) -> Result<ComposeData> {
        let jolta = self.jolta.clone();
        self.runtime
            .spawn(async move {
                let message: MessageId = jolta
                    .store
                    .read_async(move |c| Ok(c.query_row("SELECT max(id) FROM message WHERE thread_id = ?1", [thread_id], |r| r.get(0))?))
                    .await?;
                let raw = jolta_core::imap::message_source(&jolta, message).await?;
                let kind = match mode {
                    ReplyMode::Reply => ReplyKind::Reply,
                    ReplyMode::ReplyAll => ReplyKind::ReplyAll,
                    ReplyMode::Forward => ReplyKind::Forward,
                };
                let raw = Arc::new(raw);
                let source = raw.clone();
                let mut c = jolta.store.read_async(move |c| compose::reply(c, message, &source, kind, utc_offset)).await?;
                if kind == ReplyKind::Forward {
                    let dir = jolta.data_dir.join("forwards").join(message.0.to_string());
                    c.attachments = compose::forward_attachments(&raw, &dir)?;
                }
                Ok::<_, jolta_core::Error>(c)
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queues a message; it leaves after the undo window, or at `send_at`.
    /// With `remind_at`, the thread comes back to the inbox then unless
    /// someone replies. Returns a token for `cancel_send`.
    pub fn send(&self, compose: ComposeData, send_at: Option<i64>, remind_at: Option<i64>) -> Result<i64> {
        let token = self.running_engine()?.send(&compose.into(), send_at, remind_at)?;
        if send_at.is_some() {
            self.listener.on_event(CoreEvent::ListsChanged);
        }
        // The people just written to rank higher once Sent has synced.
        self.people_stale();
        Ok(token)
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
        Ok(self.jolta.store.read(store::drafts)?.into_iter().map(Into::into).collect())
    }

    /// The part an inline `cid:` image names, for the reader.
    pub async fn inline_image(&self, message_id: i64, content_id: String) -> Result<AttachmentData> {
        let jolta = self.jolta.clone();
        self.runtime
            .spawn(async move {
                let raw = jolta_core::imap::message_source(&jolta, MessageId(message_id)).await?;
                jolta_core::render::inline_part(&raw, &content_id)
                    .ok_or_else(|| jolta_core::Error::NotFound(format!("inline image {content_id}")))
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map(|(mime_type, bytes)| AttachmentData { name: String::new(), mime_type, bytes })
            .map_err(Into::into)
    }

    /// Loads remote images from this sender from now on, or stops.
    pub fn set_sender_trusted(&self, address: String, trusted: bool) -> Result<()> {
        Ok(self.jolta.store.write(|tx| store::set_sender_trusted(tx, &address, trusted))?)
    }

    pub fn account_settings(&self, account_id: i64) -> Result<AccountSettingsInfo> {
        Ok(self.jolta.store.read(|c| store::account(c, AccountId(account_id)))?.settings().into())
    }

    /// Checks new settings against the servers, then saves them. Empty
    /// passwords keep the stored ones.
    pub async fn save_account_settings(
        &self,
        account_id: i64,
        settings: AccountSettingsInfo,
        password: Option<String>,
        smtp_password: Option<String>,
    ) -> Result<AccountInfo> {
        let (jolta, engine) = (self.jolta.clone(), self.engine.lock().unwrap_or_else(|e| e.into_inner()).clone());
        let id = AccountId(account_id);
        let account = self
            .runtime
            .spawn(async move { jolta_core::imap::update_account(&jolta, id, settings.into(), password, smtp_password).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        if let Some(engine) = engine {
            engine.settings_changed(id);
        }
        Ok(AccountInfo {
            id: account.id.0,
            kind: account.kind.to_string(),
            provider: account.provider_label().to_string(),
            address: account.address,
            display_name: account.display_name,
        })
    }

    /// Hides or shows several folders at once.
    pub fn set_mailboxes_hidden(&self, mailbox_ids: Vec<i64>, hidden: bool) -> Result<()> {
        self.jolta.store.write(|tx| {
            for id in &mailbox_ids {
                store::set_mailbox_hidden(tx, MailboxId(*id), hidden)?;
            }
            Ok(())
        })?;
        self.changed();
        Ok(())
    }

    /// Keeps a folder out of the sidebar, or brings it back.
    pub fn set_mailbox_hidden(&self, mailbox_id: i64, hidden: bool) -> Result<()> {
        self.jolta.store.write(|tx| store::set_mailbox_hidden(tx, MailboxId(mailbox_id), hidden))?;
        self.changed();
        Ok(())
    }

    /// The mailbox on screen, or none: it syncs before the others.
    pub fn focus_mailbox(&self, mailbox_id: Option<i64>) {
        if let Some(engine) = self.engine.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            engine.focus_mailbox(mailbox_id.map(MailboxId));
        }
    }

    /// The bytes of one attachment of a message, for opening or saving.
    pub async fn attachment(&self, message_id: i64, index: u32) -> Result<AttachmentData> {
        let jolta = self.jolta.clone();
        self.runtime
            .spawn(async move {
                let raw = jolta_core::imap::message_source(&jolta, MessageId(message_id)).await?;
                jolta_core::render::attachment_bytes(&raw, index as usize)
                    .ok_or_else(|| jolta_core::Error::NotFound(format!("attachment {index}")))
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map(|(name, mime_type, bytes)| AttachmentData { name, mime_type, bytes })
            .map_err(Into::into)
    }

    pub fn sections(&self) -> Result<Vec<SectionInfo>> {
        Ok(self
            .jolta
            .store
            .read(store::sections)?
            .into_iter()
            .map(|s| SectionInfo { id: s.id, name: s.name, query: s.query })
            .collect())
    }

    /// Replaces the split-inbox sections, in order. Each query must parse.
    pub fn set_sections(&self, sections: Vec<SectionInfo>) -> Result<()> {
        for s in &sections {
            search::parse(&s.query)?;
        }
        let list: Vec<(String, String)> = sections.into_iter().map(|s| (s.name, s.query)).collect();
        self.jolta.store.write(|tx| store::set_sections(tx, &list))?;
        self.changed();
        Ok(())
    }

    /// Marks these senders as VIPs, or stops.
    pub fn set_vips(&self, addresses: Vec<String>, vip: bool) -> Result<()> {
        self.jolta.store.write(|tx| addresses.iter().try_for_each(|a| store::set_vip(tx, a, vip)))?;
        self.changed();
        Ok(())
    }

    pub fn is_vip(&self, address: String) -> Result<bool> {
        Ok(self.jolta.store.read(|c| store::is_vip(c, &address))?)
    }

    /// Every VIP address, A to Z.
    pub fn vips(&self) -> Result<Vec<String>> {
        Ok(self.jolta.store.read(store::vips)?)
    }

    /// People to suggest for what was typed in a recipient field, best
    /// first. Mail sent from `account_id` ranks higher; addresses already
    /// in `recipients` (the fields as typed) are left out.
    pub fn suggest_addresses(&self, typed: String, account_id: Option<i64>, recipients: String) -> Result<Vec<PersonInfo>> {
        let people = self.people()?;
        let skip: Vec<String> = compose::parse_addresses(&recipients).into_iter().map(|(_, a)| a).collect();
        Ok(people
            .suggest(&typed, account_id.map(AccountId), &skip, 6)
            .into_iter()
            .map(|p| PersonInfo { name: p.name, address: p.address })
            .collect())
    }

    /// Stops suggesting an address until mail is sent to it again.
    pub fn forget_address(&self, address: String) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        self.jolta.store.write(|tx| people::forget(tx, &address, now))?;
        *self.people.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }

    /// People from an address book (the Mac's Contacts), suggested after
    /// the people you write to. An empty list turns the source off.
    pub fn set_address_book(&self, people: Vec<PersonInfo>) {
        *self.address_book.lock().unwrap_or_else(|e| e.into_inner()) = people.into_iter().map(|p| (p.name, p.address)).collect();
        self.people_stale();
    }

    /// Messages set to go out later, soonest first. Messages still in their
    /// undo window aren't listed.
    pub fn scheduled(&self) -> Result<Vec<ScheduledInfo>> {
        let after = now() + jolta_core::engine::UNDO_SEND_SECONDS;
        Ok(self
            .jolta
            .store
            .read(|c| store::scheduled(c, after))?
            .into_iter()
            .map(|s| ScheduledInfo { token: s.outbox, account_id: s.account.0, subject: s.subject, send_at: s.send_at })
            .collect())
    }

    /// Sleeping threads, soonest first.
    pub fn snoozed(&self) -> Result<Vec<SnoozeInfo>> {
        Ok(self
            .jolta
            .store
            .read(store::snoozed)?
            .into_iter()
            .map(|s| SnoozeInfo { thread_id: s.thread.0, account_id: s.account.0, wake_at: s.wake_at, subject: s.subject })
            .collect())
    }

    pub fn snooze_presets(&self, utc_offset: i32) -> Vec<SnoozeChoice> {
        jolta_core::when::presets(now(), utc_offset).into_iter().map(|(label, at)| SnoozeChoice { label, at }).collect()
    }

    /// A snooze time typed in plain words, or nothing if it makes no sense.
    pub fn parse_time(&self, text: String, utc_offset: i32) -> Option<i64> {
        jolta_core::when::parse(&text, now(), utc_offset)
    }

    /// Serves MCP on a Unix socket at `socket_path` for agents (through
    /// the jolta-mcp helper). Needs `start` first.
    pub fn start_mcp(&self, socket_path: String, utc_offset: i32) -> Result<()> {
        let engine = self.running_engine()?;
        jolta_core::mcp::set_local_offset(utc_offset);
        let (jolta, sink) = (self.jolta.clone(), self.engine_sink.lock().unwrap_or_else(|e| e.into_inner()).clone());
        let Some(sink) = sink else { return Err(CoreError::Failed { message: "the core has not been started".into() }) };
        self.runtime.spawn(async move {
            if let Err(e) = jolta_core::mcp::serve(std::path::Path::new(&socket_path), jolta, engine, sink).await {
                tracing::error!("MCP server stopped: {e}");
            }
        });
        Ok(())
    }

    pub fn agents(&self) -> Result<Vec<AgentInfo>> {
        Ok(self
            .jolta
            .store
            .read(store::agents)?
            .into_iter()
            .map(|a| AgentInfo {
                id: a.id,
                name: a.name,
                state: match a.state {
                    store::AgentState::Pending => "pending",
                    store::AgentState::Approved => "approved",
                    store::AgentState::Denied => "denied",
                }
                .into(),
                grants: a.grants.into_iter().map(|g| g.as_str().to_string()).collect(),
                last_seen: a.last_seen,
            })
            .collect())
    }

    /// Approves an agent with these grants (read, organize, draft) on every
    /// account, or denies it.
    pub fn decide_agent(&self, agent_id: i64, approve: bool, grants: Vec<String>) -> Result<()> {
        let grants: Vec<store::Grant> = grants.iter().filter_map(|g| store::Grant::parse(g)).collect();
        Ok(self.jolta.store.write(|tx| store::decide_agent(tx, agent_id, approve, &grants))?)
    }

    pub fn audit_log(&self, limit: u32) -> Result<Vec<AuditInfo>> {
        Ok(self
            .jolta
            .store
            .read(|c| store::audit_log(c, limit as usize))?
            .into_iter()
            .map(|e| AuditInfo { agent: e.agent, tool: e.tool, summary: e.summary, outcome: e.outcome, at: e.at })
            .collect())
    }

    pub fn agents_paused(&self) -> Result<bool> {
        Ok(self.jolta.store.read(store::agents_paused)?)
    }

    pub fn set_agents_paused(&self, paused: bool) -> Result<()> {
        Ok(self.jolta.store.write(|tx| store::set_agents_paused(tx, paused))?)
    }

    pub fn draft(&self, draft_id: i64) -> Result<Option<ComposeData>> {
        Ok(self.jolta.store.read(|c| store::draft(c, draft_id))?.map(Into::into))
    }

    pub fn remove_account(&self, account_id: i64) -> Result<()> {
        Ok(self.running_engine()?.remove_account(AccountId(account_id))?)
    }

    /// Adds an iCloud, Fastmail or other IMAP account after checking the
    /// password with the server. Empty hosts come from the address.
    pub async fn add_imap_account(
        &self,
        address: String,
        password: String,
        imap_host: String,
        smtp_host: String,
    ) -> Result<AccountInfo> {
        let (jolta, engine) = (self.jolta.clone(), self.engine.lock().unwrap_or_else(|e| e.into_inner()).clone());
        let setup = jolta_core::imap::ImapSetup {
            address,
            password,
            imap_host: Some(imap_host),
            smtp_host: Some(smtp_host),
            ..Default::default()
        };
        self.runtime
            .spawn(async move {
                let account = jolta_core::imap::add_account(&jolta, setup).await?;
                if let Some(engine) = engine {
                    engine.add_account(account.clone());
                }
                Ok::<_, jolta_core::Error>(AccountInfo {
                    id: account.id.0,
                    kind: account.kind.to_string(),
                    provider: account.provider_label().to_string(),
                    address: account.address,
                    display_name: account.display_name,
                })
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?
            .map_err(Into::into)
    }

    /// Whether Jolta knows the servers for this address (iCloud, Fastmail),
    /// so the add form can hide the server fields.
    pub fn knows_servers(&self, address: String) -> bool {
        jolta_core::imap::known_servers(&address).is_some()
    }

    /// Starts a browser sign-in for a Google or Microsoft account.
    pub async fn begin_sign_in(&self, provider: OAuthProvider) -> Result<Arc<SignIn>> {
        let provider = match provider {
            OAuthProvider::Google => jolta_core::oauth::Provider::Google,
            OAuthProvider::Microsoft => jolta_core::oauth::Provider::Microsoft,
        };
        let client = provider.client(&self.jolta.data_dir)?;
        let pending = self
            .runtime
            .spawn(jolta_core::oauth::begin(provider, client, 0))
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(Arc::new(SignIn {
            url: pending.url.clone(),
            provider,
            pending: Mutex::new(Some(pending)),
            jolta: self.jolta.clone(),
            runtime: self.runtime.handle().clone(),
            engine: self.engine.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }))
    }

    pub fn sync_now(&self) {
        if let Some(engine) = self.engine.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            engine.sync_now();
        }
    }
}

fn slice<T>(order: &[T], offset: u32, limit: u32) -> &[T] {
    let start = (offset as usize).min(order.len());
    &order[start..(start + limit as usize).min(order.len())]
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Core {
    /// Drops the cached list order and tells the app that lists changed.
    /// The suggestion list. Building it reads all sent mail (about 80 ms
    /// for 23,000 sent messages), so only the first build blocks; a stale
    /// list keeps answering while a new one builds in the background.
    fn people(&self) -> Result<Arc<People>> {
        let mut cache = self.people.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((built, people)) = cache.as_mut() {
            let people = people.clone();
            if built.elapsed() >= PEOPLE_FRESH {
                *built = Instant::now();
                let (jolta, book, slot) = (self.jolta.clone(), self.address_book.clone(), self.people.clone());
                std::thread::spawn(move || {
                    if let Ok(fresh) = build_people(&jolta, &book) {
                        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), Arc::new(fresh)));
                    }
                });
            }
            return Ok(people);
        }
        let people = Arc::new(build_people(&self.jolta, &self.address_book)?);
        *cache = Some((Instant::now(), people.clone()));
        Ok(people)
    }

    /// Rebuilds the suggestion list in the background at the next lookup.
    fn people_stale(&self) {
        if let Some((built, _)) = self.people.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            *built = Instant::now().checked_sub(PEOPLE_FRESH).unwrap_or(*built);
        }
    }

    fn changed(&self) {
        *self.order.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.listener.on_event(CoreEvent::ListsChanged);
    }

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
