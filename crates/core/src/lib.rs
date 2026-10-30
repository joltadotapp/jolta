pub mod actions;
pub mod blobs;
pub mod engine;
pub mod imap;
pub mod import;
pub mod mime;
pub mod model;
pub mod reader;
pub mod render;
pub mod search;
pub mod secrets;
pub mod store;
pub mod synth;

use std::path::{Path, PathBuf};

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
    format!("Kite core {} is running", version())
}

/// Where Kite keeps its data when nothing else is configured. The CLI and the
/// sandboxed app share this folder; the app reaches it through a sandbox
/// exception because macOS keeps unsandboxed tools out of the app group
/// container (see decisions.md).
pub fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/Kite")
}

/// The open data directory: the SQLite store plus the blob files.
#[derive(Clone)]
pub struct Kite {
    pub store: store::Store,
    pub blobs: blobs::Blobs,
    pub data_dir: PathBuf,
}

impl Kite {
    pub fn open(data_dir: &Path) -> Result<Kite> {
        std::fs::create_dir_all(data_dir)?;
        Ok(Kite {
            store: store::Store::open(&data_dir.join("kite.sqlite"))?,
            blobs: blobs::Blobs::new(data_dir.join("blobs")),
            data_dir: data_dir.to_path_buf(),
        })
    }
}
