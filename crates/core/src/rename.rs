//! Takes over data from the app's previous name. Until 2026-11-03 it was
//! Kite: the data folder was `~/Library/Application Support/Kite` and the
//! database `kite.sqlite`.

use std::path::Path;

use rusqlite::Connection;

use crate::{Error, Result};

/// Runs before the store opens. Moves everything from a sibling `Kite`
/// folder into a `Jolta` data folder that has no database yet, then renames
/// `kite.sqlite` in the data folder. If a name exists in both folders,
/// nothing moves: the error names it, and nothing is overwritten.
pub(crate) fn adopt_kite_data(data_dir: &Path) -> Result<()> {
    let kite_dir = data_dir.parent().map(|p| p.join("Kite"));
    if let Some(kite_dir) = kite_dir.filter(|_| data_dir.file_name() == Some("Jolta".as_ref()))
        && kite_dir.join("kite.sqlite").exists()
        && !data_dir.join("jolta.sqlite").exists()
    {
        if let Some(clash) = std::fs::read_dir(&kite_dir)?.flatten().find(|e| data_dir.join(e.file_name()).exists()) {
            return Err(Error::Other(format!(
                "Both {} and {} hold {}; move Kite's data by hand.",
                kite_dir.display(),
                data_dir.display(),
                clash.file_name().to_string_lossy()
            )));
        }
        leave_wal(&kite_dir.join("kite.sqlite"))?;
        std::fs::create_dir_all(data_dir)?;
        for entry in std::fs::read_dir(&kite_dir)?.flatten() {
            std::fs::rename(entry.path(), data_dir.join(entry.file_name()))?;
        }
        let _ = std::fs::remove_dir(&kite_dir);
    }

    let (old, new) = (data_dir.join("kite.sqlite"), data_dir.join("jolta.sqlite"));
    if old.exists() && !new.exists() {
        leave_wal(&old)?;
        std::fs::rename(&old, &new)?;
        let _ = std::fs::remove_file(data_dir.join("kite.sqlite-shm"));
    }
    Ok(())
}

/// Folds the write-ahead log into the database file and switches to a plain
/// journal, so the database is one file that can be renamed. SQLite refuses
/// while another process, such as Kite still running, has it open.
fn leave_wal(db: &Path) -> Result<()> {
    let in_use = || Error::Other("Kite is still using its data. Quit Kite, then open Jolta again.".into());
    let conn = Connection::open(db)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).map_err(|_| in_use())?;
    conn.close().map_err(|(_, e)| e)?;
    let wal = db.with_file_name(format!("{}-wal", db.file_name().unwrap_or_default().to_string_lossy()));
    if mode != "delete" || wal.exists() {
        return Err(in_use());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jolta-rename-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Kite/blobs/ab")).unwrap();
        std::fs::write(dir.join("Kite/blobs/ab/abcd"), "raw message").unwrap();
        dir
    }

    /// A Kite database whose last write is still only in the WAL, as after
    /// a crash: the database and WAL files are copied while a connection
    /// holds them open.
    fn kite_database(dir: &Path) {
        let live = dir.join("live.sqlite");
        let conn = Connection::open(&live).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES ('kept');").unwrap();
        std::fs::copy(&live, dir.join("Kite/kite.sqlite")).unwrap();
        std::fs::copy(dir.join("live.sqlite-wal"), dir.join("Kite/kite.sqlite-wal")).unwrap();
        assert!(std::fs::metadata(dir.join("Kite/kite.sqlite-wal")).unwrap().len() > 0);
    }

    #[test]
    fn kite_folder_and_database_move_with_the_log_folded_in() {
        let dir = scratch("move");
        kite_database(&dir);
        let jolta = dir.join("Jolta");
        adopt_kite_data(&jolta).unwrap();

        assert!(!dir.join("Kite").exists(), "the Kite folder is gone once empty");
        assert_eq!(std::fs::read_to_string(jolta.join("blobs/ab/abcd")).unwrap(), "raw message");
        assert!(!jolta.join("kite.sqlite").exists() && !jolta.join("kite.sqlite-wal").exists());
        let conn = Connection::open(jolta.join("jolta.sqlite")).unwrap();
        let x: String = conn.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, "kept", "the write that was only in the WAL survived");

        adopt_kite_data(&jolta).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_running_kite_stops_the_move_before_anything_moves() {
        let dir = scratch("busy");
        let conn = Connection::open(dir.join("Kite/kite.sqlite")).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch("CREATE TABLE t (x); BEGIN; SELECT * FROM t;").unwrap();

        let err = adopt_kite_data(&dir.join("Jolta")).unwrap_err();
        assert!(err.to_string().contains("Quit Kite"), "{err}");
        assert!(dir.join("Kite/kite.sqlite").exists() && dir.join("Kite/blobs/ab/abcd").exists());
        assert!(!dir.join("Jolta").exists());
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_jolta_folder_without_a_database_still_takes_kite_in() {
        let dir = scratch("existing");
        kite_database(&dir);
        std::fs::create_dir_all(dir.join("Jolta/UITest-x")).unwrap();
        adopt_kite_data(&dir.join("Jolta")).unwrap();

        assert!(dir.join("Jolta/jolta.sqlite").exists() && dir.join("Jolta/blobs/ab/abcd").exists());
        assert!(dir.join("Jolta/UITest-x").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_in_both_folders_moves_nothing() {
        let dir = scratch("clash");
        kite_database(&dir);
        std::fs::create_dir_all(dir.join("Jolta/blobs")).unwrap();
        let err = adopt_kite_data(&dir.join("Jolta")).unwrap_err();
        assert!(err.to_string().contains("blobs"), "{err}");
        assert!(dir.join("Kite/kite.sqlite").exists() && dir.join("Kite/blobs/ab/abcd").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
