//! Raw RFC 822 messages as files, named by their SHA-256.

use std::path::PathBuf;

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
}
