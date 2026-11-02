//! Raw RFC 822 messages as files, named by their SHA-256.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::Result;

#[derive(Clone)]
pub struct Blobs {
    root: PathBuf,
}

impl Blobs {
    pub fn new(root: PathBuf) -> Blobs {
        Blobs { root }
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[..2]).join(hash)
    }

    /// Writes the bytes unless a blob with the same hash exists, and returns the hash.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = hex::encode(Sha256::digest(bytes));
        let path = self.path(&hash);
        if !path.exists() {
            std::fs::create_dir_all(path.parent().expect("blob path has a parent"))?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(hash)
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.path(hash))?)
    }

    /// Deletes the blobs `keep` rejects, and leftover temporary files. Only
    /// files older than `min_age` go: sync writes a blob before the row that
    /// names it, so a young blob may be about to get its row.
    pub fn collect(&self, keep: impl Fn(&str) -> bool, min_age: Duration) -> Result<Collected> {
        let mut out = Collected::default();
        let Ok(dirs) = std::fs::read_dir(&self.root) else { return Ok(out) };
        let now = SystemTime::now();
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
            for file in files.flatten() {
                let name = file.file_name();
                let name = name.to_string_lossy();
                let Ok(meta) = file.metadata() else { continue };
                let young = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_none_or(|age| age < min_age);
                let temporary = name.ends_with(".tmp");
                if young || (!temporary && keep(&name)) {
                    continue;
                }
                if std::fs::remove_file(file.path()).is_ok() {
                    out.files += 1;
                    out.bytes += meta.len();
                }
            }
            let _ = std::fs::remove_dir(dir.path());
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collected {
    pub files: u64,
    pub bytes: u64,
}
