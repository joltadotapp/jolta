//! Account secrets in the macOS Keychain. Each account has its own
//! `secret_ref`, so data folders never share or overwrite an entry.

use std::sync::{Arc, Once};

use keyring_core::Entry;

use sha2::{Digest, Sha256};

use crate::Result;

const SERVICE: &str = "be.example.kite";

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let store = apple_native_keyring_store::keychain::Store::new()
            .expect("the login keychain store can always be created");
        keyring_core::set_default_store(store as Arc<keyring_core::CredentialStore>);
    });
}

/// A fresh Keychain account name: the address plus a random-enough suffix.
pub fn new_ref(address: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let seed = format!("{address}:{}:{}", nanos.as_nanos(), std::process::id());
    format!("{address}:{}", &hex::encode(Sha256::digest(seed))[..12])
}

fn entry(secret_ref: &str) -> Result<Entry> {
    init();
    Ok(Entry::new(SERVICE, secret_ref)?)
}

pub fn set_password(secret_ref: &str, secret: &str) -> Result<()> {
    entry(secret_ref)?.set_password(secret)?;
    Ok(())
}

pub fn password(secret_ref: &str) -> Result<String> {
    Ok(entry(secret_ref)?.get_password()?)
}

pub fn delete(secret_ref: &str) -> Result<()> {
    match entry(secret_ref)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
