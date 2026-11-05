//! SQLite store: one writer connection and a few readers, WAL mode.
//!
//! Calls are blocking. Async code runs them through `Store::write_async` and
//! `Store::read_async`, which move the work onto tokio's blocking pool.

mod accounts;
mod added;
mod agents;
mod calendar;
mod filing;
mod invitations;
mod messages;
mod ops;
mod outbox;
mod reminders;
mod sections;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, Transaction};

use crate::Result;

pub use accounts::*;
pub use added::*;
pub use agents::*;
pub use calendar::*;
pub use filing::*;
pub use invitations::*;
pub use messages::*;
pub use ops::*;
pub use outbox::*;
pub use reminders::*;
pub use sections::*;

const MIGRATIONS: &[&str] = &[include_str!("schema.sql"), include_str!("schema2.sql"), include_str!("schema3.sql"), include_str!("schema4.sql"), include_str!("schema5.sql"), include_str!("schema6.sql"), include_str!("schema7.sql"), include_str!("schema8.sql"), include_str!("schema9.sql"), include_str!("schema10.sql"), include_str!("schema11.sql"), include_str!("schema12.sql"), include_str!("schema13.sql"), include_str!("schema14.sql"), include_str!("schema15.sql"), include_str!("schema16.sql"), include_str!("schema17.sql"), include_str!("schema18.sql"), include_str!("schema19.sql"), include_str!("schema20.sql"), include_str!("schema21.sql"), include_str!("schema22.sql"), include_str!("schema23.sql"), include_str!("schema24.sql"), include_str!("schema25.sql"), include_str!("schema26.sql"), include_str!("schema27.sql")];
const READERS: usize = 4;
const MMAP_BYTES: i64 = 2 << 30;
const WRITER_CACHE_KIB: i64 = 8_000;
const READER_CACHE_KIB: i64 = 2_000;

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let mut writer = Connection::open(path)?;
        configure(&writer, WRITER_CACHE_KIB)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut writer)?;

        let readers = (0..READERS)
            .map(|_| {
                let conn = Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                configure(&conn, READER_CACHE_KIB)?;
                Ok(Mutex::new(conn))
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Store {
            inner: Arc::new(Inner {
                writer: Mutex::new(writer),
                readers,
                next_reader: AtomicUsize::new(0),
            }),
        })
    }

    pub fn write<T>(&self, f: impl FnOnce(&Transaction) -> Result<T>) -> Result<T> {
        let mut conn = self.inner.writer.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        for r in &self.inner.readers {
            if let Ok(conn) = r.try_lock() {
                return f(&conn);
            }
        }
        let i = self.inner.next_reader.fetch_add(1, Ordering::Relaxed) % self.inner.readers.len();
        let conn = self.inner.readers[i].lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    pub async fn write_async<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.write(f)).await?
    }

    pub async fn read_async<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.read(f)).await?
    }
}

/// Reads come from the database file mapped into memory, which macOS can
/// drop under pressure and doesn't count against the app, so each
/// connection's own page cache stays small: with five connections at the
/// old 16 MB each, the caches alone could reach 80 MB.
fn configure(conn: &Connection, cache_kib: i64) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "mmap_size", MMAP_BYTES)?;
    conn.pragma_update(None, "cache_size", -cache_kib)?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}
