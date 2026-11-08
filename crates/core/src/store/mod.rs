//! SQLite store: one writer connection and a few readers, WAL mode.
//!
//! Calls are blocking. Async code runs them through `Store::write_async` and
//! `Store::read_async`, which move the work onto tokio's blocking pool.

mod accounts;
mod added;
mod away;
mod rooms;
mod agents;
mod calendar;
mod calendar_ops;
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
pub use away::*;
pub use rooms::*;
pub use agents::*;
pub use calendar::*;
pub use calendar_ops::*;
pub use filing::*;
pub use invitations::*;
pub use messages::*;
pub use ops::*;
pub use outbox::*;
pub use reminders::*;
pub use sections::*;

const MIGRATIONS: &[&str] = &[include_str!("schema.sql"), include_str!("schema2.sql"), include_str!("schema3.sql"), include_str!("schema4.sql"), include_str!("schema5.sql"), include_str!("schema6.sql"), include_str!("schema7.sql"), include_str!("schema8.sql"), include_str!("schema9.sql"), include_str!("schema10.sql"), include_str!("schema11.sql"), include_str!("schema12.sql"), include_str!("schema13.sql"), include_str!("schema14.sql"), include_str!("schema15.sql"), include_str!("schema16.sql"), include_str!("schema17.sql"), include_str!("schema18.sql"), include_str!("schema19.sql"), include_str!("schema20.sql"), include_str!("schema21.sql"), include_str!("schema22.sql"), include_str!("schema23.sql"), include_str!("schema24.sql"), include_str!("schema25.sql"), include_str!("schema26.sql"), include_str!("schema27.sql"), include_str!("schema28.sql"), include_str!("schema29.sql"), include_str!("schema30.sql"), include_str!("schema31.sql"), include_str!("schema32.sql"), include_str!("schema33.sql"), include_str!("schema34.sql"), include_str!("schema35.sql")];
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Schema 30 rebuilds op, draft and outbox with AUTOINCREMENT. Their
    /// rows keep their ids, and new ids start above any still remembered
    /// elsewhere: a sent message's deleted draft, an answer's reply mail
    /// that was taken back.
    #[test]
    fn schema30_keeps_ids_and_counts_past_remembered_ones() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn, WRITER_CACHE_KIB).unwrap();
        let tx = conn.transaction().unwrap();
        for sql in &MIGRATIONS[..29] {
            tx.execute_batch(sql).unwrap();
        }
        tx.pragma_update(None, "user_version", 29).unwrap();
        tx.execute_batch(
            "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'imap', 'a@x', 'a@x', 'r', 0);
             INSERT INTO op (id, account_id, kind, messages, state, created, tried) VALUES (4, 1, 'flags', '1:5', 'pending', 0, 1);
             INSERT INTO draft (id, account_id, subject, updated) VALUES (2, 1, 'Kept', 0);
             INSERT INTO outbox (id, account_id, draft_id, blob, message_id, sender, recipients, send_after, created)
               VALUES (3, 1, 7, 'b', '<m@x>', 'a@x', 'b@x', 0, 0);
             INSERT INTO invitation_answer (account_id, uid, answer, reply) VALUES (1, 'u', 'accepted', 9);",
        )
        .unwrap();
        tx.commit().unwrap();

        migrate(&mut conn).unwrap();

        let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!(one("SELECT id || ' ' || messages || ' ' || tried FROM op"), "4 1:5 1");
        assert_eq!(one("SELECT id || ' ' || subject FROM draft"), "2 Kept");
        assert_eq!(one("SELECT id || ' ' || draft_id FROM outbox"), "3 7");
        let next = |sql: &str| {
            conn.execute(sql, []).unwrap();
            conn.last_insert_rowid()
        };
        assert_eq!(next("INSERT INTO op (account_id, kind, messages, created) VALUES (1, 'flags', '', 0)"), 5);
        assert_eq!(next("INSERT INTO draft (updated) VALUES (0)"), 8, "past the sent message's draft");
        assert_eq!(
            next("INSERT INTO outbox (account_id, blob, message_id, sender, recipients, send_after, created) VALUES (1, '', '', '', '', 0, 0)"),
            10,
            "past the answer's reply mail"
        );
        assert_eq!(one("SELECT group_concat(name, ' ') FROM (SELECT name FROM sqlite_master WHERE name IN ('op_pending', 'outbox_queued') ORDER BY name)"), "op_pending outbox_queued");
    }
}
