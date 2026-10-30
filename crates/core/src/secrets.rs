//! Account secrets in the macOS Keychain, keyed by account id.

use std::sync::{Arc, Once};

use keyring_core::Entry;

use crate::{AccountId, Result};

const SERVICE: &str = "be.example.kite";

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let store = apple_native_keyring_store::keychain::Store::new()
            .expect("the login keychain store can always be created");
        keyring_core::set_default_store(store as Arc<keyring_core::CredentialStore>);
    });
}

fn entry(account: AccountId) -> Result<Entry> {
    init();
    Ok(Entry::new(SERVICE, &format!("account-{account}"))?)
}

pub fn set_password(account: AccountId, secret: &str) -> Result<()> {
    entry(account)?.set_password(secret)?;
    Ok(())
}

pub fn password(account: AccountId) -> Result<String> {
    Ok(entry(account)?.get_password()?)
}

pub fn delete(account: AccountId) -> Result<()> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
