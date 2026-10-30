//! The Swift-facing API: thin wrappers over `kite_core`. Lists cross the
//! bridge as pages; sync runs on the core's own tokio runtime and reports
//! through `CoreListener`.

use std::sync::{Arc, Mutex};

use kite_core::imap::{self, SyncOptions};
use kite_core::{AccountId, Kite, MessageId, ThreadId, reader, search, store};

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
    SyncFinished { account_id: i64, new_messages: u32 },
    SyncFailed { account_id: i64, message: String },
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
    syncing: Arc<Mutex<bool>>,
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
            syncing: Arc::new(Mutex::new(false)),
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

    /// Syncs every account in the background unless a sync is already running.
    pub fn sync_all(&self) {
        {
            let mut running = self.syncing.lock().unwrap_or_else(|e| e.into_inner());
            if *running {
                return;
            }
            *running = true;
        }
        let (kite, syncing, order) = (self.kite.clone(), self.syncing.clone(), self.order.clone());
        let notify = {
            let listener = self.listener.clone();
            move |event: CoreEvent| {
                if matches!(event, CoreEvent::ListsChanged) {
                    *order.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                listener.on_event(event);
            }
        };
        let listener = notify;
        self.runtime.spawn(async move {
            let accounts = kite.store.read_async(store::accounts).await.unwrap_or_default();
            for a in accounts.into_iter().filter(|a| !a.imap_host.is_empty()) {
                listener(CoreEvent::SyncStarted { account_id: a.id.0 });
                let mut last_event = std::time::Instant::now();
                let result = imap::sync_account(&kite, &a, SyncOptions { bodies: true }, |_| {
                    if last_event.elapsed().as_millis() > 500 {
                        last_event = std::time::Instant::now();
                        listener(CoreEvent::ListsChanged);
                    }
                })
                .await;
                listener(match result {
                    Ok(r) => CoreEvent::SyncFinished { account_id: a.id.0, new_messages: r.new as u32 },
                    Err(e) => CoreEvent::SyncFailed { account_id: a.id.0, message: e.to_string() },
                });
                listener(CoreEvent::ListsChanged);
            }
            *syncing.lock().unwrap_or_else(|e| e.into_inner()) = false;
        });
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
