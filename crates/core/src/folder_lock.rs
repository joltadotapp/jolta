use std::path::Path;

use crate::{Error, Result};

/// The right to write a data folder's mail and calendars to their servers
/// and to save sync progress. Two holders would both send the outbox and
/// the calendar queue, and one's older sync state could overwrite the
/// other's, so one process holds it at a time: the engine for its life, a
/// one-shot `jolta` command for its run.
///
/// It is an exclusive flock on `engine.lock`. The lock belongs to the open
/// file, so a second `take` fails in the same process too, and the OS drops
/// it when the process ends, so a crash leaves no stale lock.
pub struct FolderLock {
    _file: std::fs::File,
}

impl FolderLock {
    /// Fails with `EngineBusy` while another holder has the folder.
    pub fn take(data_dir: &Path) -> Result<FolderLock> {
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(data_dir.join("engine.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(FolderLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::EngineBusy),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}
