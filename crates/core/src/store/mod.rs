//! SQLite store: one writer connection and a few readers, WAL mode.
//!
//! Calls are blocking. Async code runs them through `Store::write_async` and
//! `Store::read_async`, which move the work onto tokio's blocking pool.

mod accounts;
mod messages;
mod ops;
mod outbox;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, Transaction};

use crate::Result;

pub use accounts::*;
pub use messages::*;
pub use ops::*;
pub use outbox::*;

const MIGRATIONS: &[&str] = &[include_str!("schema.sql"), include_str!("schema2.sql"), include_str!("schema3.sql")];
const READERS: usize = 4;

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
        configure(&writer)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut writer)?;

        let readers = (0..READERS)
            .map(|_| {
                let conn = Connection::open_with_flags(
                    path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                configure(&conn)?;
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

fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "cache_size", -16000)?;
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
