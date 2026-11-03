//! Account secrets in the macOS Keychain. Each account has its own
//! `secret_ref`, so data folders never share or overwrite an entry.

use std::sync::{Arc, Once};

use keyring_core::Entry;

use sha2::{Digest, Sha256};

use crate::Result;

const SERVICE: &str = "app.jolta.mac";
/// Where secrets lived while the app was called Kite (until 2026-11-03).
/// Jolta moves each one to `SERVICE` the first time it reads it.
const KITE_SERVICE: &str = "be.example.kite";

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

/// Reads a secret, moving it from Kite's entry first if Jolta has none yet.
/// macOS asks once for that read, because the old entry belongs to an app
/// with another identity. The old entry goes only after the copy reads back.
fn lookup(secret_ref: &str) -> std::result::Result<String, keyring_core::Error> {
    init();
    let new = Entry::new(SERVICE, secret_ref)?;
    match new.get_password() {
        Err(keyring_core::Error::NoEntry) => {}
        found => return found,
    }
    let old = Entry::new(KITE_SERVICE, secret_ref)?;
    let secret = old.get_password()?;
    new.set_password(&secret)?;
    if new.get_password()? == secret {
        let _ = old.delete_credential();
    }
    Ok(secret)
}

/// The Keychain name of an account's separate SMTP password.
pub fn smtp_ref(secret_ref: &str) -> String {
    format!("{secret_ref}#smtp")
}

pub fn delete(secret_ref: &str) -> Result<()> {
    init();
    for service in [SERVICE, KITE_SERVICE] {
        match Entry::new(service, secret_ref)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kite_secrets_move_to_jolta_on_first_read() {
        use_memory_store();
        Entry::new(KITE_SERVICE, "ann@example.com:abc").unwrap().set_password("hunter2").unwrap();

        assert!(exists("ann@example.com:abc").unwrap());
        assert_eq!(password("ann@example.com:abc").unwrap(), "hunter2");
        assert_eq!(Entry::new(SERVICE, "ann@example.com:abc").unwrap().get_password().unwrap(), "hunter2");
        assert!(matches!(
            Entry::new(KITE_SERVICE, "ann@example.com:abc").unwrap().get_password(),
            Err(keyring_core::Error::NoEntry)
        ));

        Entry::new(KITE_SERVICE, "bob@example.com:def").unwrap().set_password("old").unwrap();
        delete("bob@example.com:def").unwrap();
        assert!(!exists("bob@example.com:def").unwrap(), "removing an account clears Kite's entry too");
        assert!(!exists("nobody@example.com:000").unwrap());
    }
}
