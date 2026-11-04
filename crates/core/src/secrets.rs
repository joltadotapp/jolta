//! Account secrets in the macOS Keychain. Each account has its own
//! `secret_ref`, so data folders never share or overwrite an entry.

use std::sync::{Arc, Once};

use keyring_core::Entry;

use sha2::{Digest, Sha256};

use crate::Result;

const SERVICE: &str = "app.jolta.mac";

static ONCE: Once = Once::new();

fn init() {
    ONCE.call_once(|| {
        let store = apple_native_keyring_store::keychain::Store::new()
            .expect("the login keychain store can always be created");
        keyring_core::set_default_store(store as Arc<keyring_core::CredentialStore>);
    });
}

/// Keeps secrets in memory for this process instead of the Keychain, for
/// tests: the Mac mini is headless, and a Keychain dialog there never closes.
/// Must run before the first secret is read or written.
pub fn use_memory_store() {
    ONCE.call_once(|| {
        let store = keyring_core::mock::Store::new().expect("the mock store can always be created");
        keyring_core::set_default_store(store as Arc<keyring_core::CredentialStore>);
    });
}

pub fn exists(secret_ref: &str) -> Result<bool> {
    match lookup(secret_ref) {
        Ok(_) => Ok(true),
        Err(keyring_core::Error::NoEntry) => Ok(false),
        Err(e) => Err(e.into()),
    }
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
    Ok(lookup(secret_ref)?)
}

fn lookup(secret_ref: &str) -> std::result::Result<String, keyring_core::Error> {
    init();
    Entry::new(SERVICE, secret_ref)?.get_password()
}

/// The Keychain name of an account's separate SMTP password.
pub fn smtp_ref(secret_ref: &str) -> String {
    format!("{secret_ref}#smtp")
}

pub fn delete(secret_ref: &str) -> Result<()> {
    init();
    match Entry::new(SERVICE, secret_ref)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
