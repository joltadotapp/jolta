//! The addresses a provider says an account may send from: Gmail's Send
//! mail as list and Microsoft 365's proxy addresses, read with the scopes
//! the mail sign-in already has. Other accounts have none to read.

use serde_json::Value;

use crate::store::{self, ProviderAlias};
use crate::{Account, AccountId, AccountKind, Jolta, Result};

/// `users.settings.sendAs.list`, without the account's own address and
/// the ones Gmail hasn't verified.
pub fn gmail_aliases(body: &Value) -> Vec<ProviderAlias> {
    let _ = body;
    Vec::new()
}

/// `proxyAddresses` from Graph's `/me`: the `smtp:` ones, without the
/// primary (`SMTP:`) and the tenant's `onmicrosoft.com` routing addresses.
pub fn graph_aliases(body: &Value) -> Vec<ProviderAlias> {
    let _ = body;
    Vec::new()
}

/// The provider's aliases for the account, or None when it has no list.
pub async fn fetch(account: &Account) -> Result<Option<Vec<ProviderAlias>>> {
    let _ = account;
    Ok(None)
}

/// Reads the provider's aliases and stores them in place of the last list.
pub async fn refresh(jolta: &Jolta, account: AccountId) -> Result<()> {
    let account = jolta.store.read_async(move |c| store::account(c, account)).await?;
    if account.kind == AccountKind::Imap {
        return Ok(());
    }
    if let Some(list) = fetch(&account).await? {
        jolta.store.write_async(move |tx| store::set_provider_aliases(tx, account.id, &list)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(list: Vec<ProviderAlias>) -> Vec<(String, String)> {
        list.into_iter().map(|a| (a.address, a.display_name)).collect()
    }

    #[test]
    fn gmail_lists_its_verified_send_as_addresses() {
        let body: Value = serde_json::from_str(include_str!("../tests/fixtures/send-as/gmail-sendas.json")).unwrap();
        assert_eq!(
            listed(gmail_aliases(&body)),
            [("sales@example.com".into(), "Example Sales".into()), ("me@example.org".into(), String::new())],
            "not the primary address, and not one waiting for verification"
        );
        assert!(gmail_aliases(&serde_json::json!({})).is_empty(), "an account with only its own address");
    }

    #[test]
    fn microsoft_365_lists_its_secondary_smtp_addresses() {
        let body: Value = serde_json::from_str(include_str!("../tests/fixtures/send-as/graph-proxy-addresses.json")).unwrap();
        assert_eq!(
            listed(graph_aliases(&body)),
            [("sales@example.com".into(), String::new()), ("Me.Second@Example.org".into(), String::new())],
            "not the primary, the routing address or other kinds"
        );
        assert!(graph_aliases(&serde_json::json!({ "proxyAddresses": [] })).is_empty());
    }
}
