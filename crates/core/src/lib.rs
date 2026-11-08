pub mod actions;
pub mod ai;
pub mod blobs;
pub mod calendar;
pub mod compose;
pub mod engine;
pub mod file_icon;
pub mod gmail_settings;
pub mod graph;
pub mod imap;
pub mod mcp;
pub mod import;
pub mod mime;
pub mod model;
pub mod oauth;
pub mod people;
pub mod reader;
mod rename;
pub mod rich;
pub mod render;
pub mod search;
pub mod secrets;
pub mod smime;
pub mod smtp;
pub mod store;
pub mod synth;
pub mod unsubscribe;
pub mod when;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use model::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("IMAP: {0}")]
    Imap(#[from] async_imap::error::Error),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS: {0}")]
    Tls(#[from] tokio_rustls::rustls::Error),
    #[error("keychain: {0}")]
    Secret(#[from] keyring_core::Error),
    #[error("sign-in failed: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid query: {0}")]
    Query(String),
    /// The request never reached the server: no network, no name lookup or
    /// no connection. Sending it again later can't do anything twice.
    #[error("{0}")]
    Offline(String),
    #[error("{0}")]
    Other(String),
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::Other(format!("background task failed: {e}"))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn greeting() -> String {
    format!("Jolta core {} is running", version())
}

/// Where Jolta keeps its data when nothing else is configured. The CLI and the
/// sandboxed app share this folder; the app reaches it through a sandbox
/// exception because macOS keeps unsandboxed tools out of the app group
/// container.
pub fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/Jolta")
}

/// The open data directory: the SQLite store plus the blob files.
#[derive(Clone)]
pub struct Jolta {
    pub store: store::Store,
    pub blobs: blobs::Blobs,
    pub data_dir: PathBuf,
    /// The mailbox on screen. A sync takes it before the others.
    pub focus: Arc<Mutex<Option<MailboxId>>>,
}

impl Jolta {
    pub fn open(data_dir: &Path) -> Result<Jolta> {
        rename::adopt_kite_data(data_dir)?;
        std::fs::create_dir_all(data_dir)?;
        oauth::set_data_dir(data_dir);
        Ok(Jolta {
            store: store::Store::open(&data_dir.join("jolta.sqlite"))?,
            blobs: blobs::Blobs::new(data_dir.join("blobs")),
            data_dir: data_dir.to_path_buf(),
            focus: Arc::default(),
        })
    }

    /// Deletes blob files no message, outbox or event row names any more: left
    /// by removed accounts and by mail and events deleted on the server.
    pub fn collect_blobs(&self, min_age: std::time::Duration) -> Result<blobs::Collected> {
        let keep: std::collections::HashSet<String> = self.store.read(|c| {
            let mut stmt = c.prepare("SELECT blob FROM message WHERE blob IS NOT NULL UNION SELECT blob FROM outbox UNION SELECT blob FROM event WHERE blob IS NOT NULL")?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })?;
        self.blobs.collect(|hash| keep.contains(hash), min_age)
    }

    /// Takes the next mailbox to sync from `queue`: the one on screen if it
    /// is still waiting, else the first.
    pub fn next_mailbox(&self, queue: &mut Vec<Mailbox>) -> Option<Mailbox> {
        let focus = *self.focus.lock().unwrap_or_else(|e| e.into_inner());
        let i = focus.and_then(|f| queue.iter().position(|m| m.id == f)).unwrap_or(0);
        (i < queue.len()).then(|| queue.remove(i))
    }
}
