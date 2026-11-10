//! The other addresses an account sends from, and who new messages are
//! from by default.

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::*;
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AliasSource {
    /// Added by the user in Settings.
    Manual,
    /// Read from Gmail or Microsoft 365, and replaced on each read.
    Provider,
}

impl ToSql for AliasSource {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(match self {
            AliasSource::Manual => "manual",
            AliasSource::Provider => "provider",
        }))
    }
}

impl FromSql for AliasSource {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_str()? {
            "manual" => Ok(AliasSource::Manual),
            "provider" => Ok(AliasSource::Provider),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alias {
    pub id: i64,
    pub account: AccountId,
    pub address: String,
    /// Empty sends with the account's name.
    pub display_name: String,
    pub source: AliasSource,
}

/// An address a provider says the account may send from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderAlias {
    pub address: String,
    pub display_name: String,
}

/// Who a message is from: an account, and one of its aliases or None for
/// its own address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SenderChoice {
    pub account: AccountId,
    pub alias: Option<String>,
}

/// The account's aliases the user sends from, without the ones removed.
pub fn aliases(conn: &Connection, account: AccountId) -> Result<Vec<Alias>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, account_id, address, display_name, source FROM account_alias
         WHERE account_id = ?1 AND NOT hidden ORDER BY address COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([account], |r| {
        Ok(Alias { id: r.get(0)?, account: r.get(1)?, address: r.get(2)?, display_name: r.get(3)?, source: r.get(4)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Adds an address the account sends from. One the user removed from the
/// provider's list comes back.
pub fn add_alias(conn: &Connection, account: AccountId, address: &str, display_name: &str) -> Result<Alias> {
    let _ = (conn, account, address, display_name);
    todo!()
}

pub fn rename_alias(conn: &Connection, id: i64, display_name: &str) -> Result<()> {
    let _ = (conn, id, display_name);
    todo!()
}

/// Removes an alias. A provider's stays hidden, so its next list doesn't
/// bring it back.
pub fn remove_alias(conn: &Connection, id: i64) -> Result<()> {
    let _ = (conn, id);
    todo!()
}

/// Replaces the account's provider aliases with the provider's list. The
/// user's own stay, and so do the ones they removed.
pub fn set_provider_aliases(conn: &Connection, account: AccountId, list: &[ProviderAlias]) -> Result<()> {
    let _ = (conn, account, list);
    todo!()
}

/// The sender new messages get in views that mix accounts, as stored. It
/// may name an account or alias that has gone since.
pub fn default_sender(conn: &Connection) -> Result<Option<SenderChoice>> {
    let raw: Option<String> = conn.query_row("SELECT value FROM setting WHERE key = 'default_sender'", [], |r| r.get(0)).optional()?;
    Ok(raw.and_then(|raw| {
        let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
        Some(SenderChoice {
            account: AccountId(value["account"].as_i64()?),
            alias: value["alias"].as_str().map(str::to_string),
        })
    }))
}

pub fn set_default_sender(conn: &Connection, sender: &SenderChoice) -> Result<()> {
    let value = serde_json::json!({ "account": sender.account.0, "alias": sender.alias });
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('default_sender', ?1) ON CONFLICT (key) DO UPDATE SET value = ?1",
        params![value.to_string()],
    )?;
    Ok(())
}

/// Every address that is the user's: each account's own and its aliases.
pub fn own_addresses(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached("SELECT address FROM own_address")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn invalid(address: &str) -> Error {
    Error::Other(format!("“{address}” is not an email address."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewAccount, Store, insert_account};

    pub(crate) fn store_with_account(name: &str) -> (Store, AccountId, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("jolta-aliases-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("jolta.sqlite")).unwrap();
        let account = store
            .write(|tx| {
                insert_account(tx, &NewAccount {
                    kind: AccountKind::Gmail,
                    address: "me@example.com".into(),
                    display_name: "Me".into(),
                    username: "me@example.com".into(),
                    imap_host: "imap.gmail.com".into(),
                    imap_port: 993,
                    smtp_host: "smtp.gmail.com".into(),
                    smtp_port: 465,
                })
            })
            .unwrap()
            .id;
        (store, account, dir)
    }

    fn listed(store: &Store, account: AccountId) -> Vec<(String, String, AliasSource)> {
        store
            .read(|c| aliases(c, account))
            .unwrap()
            .into_iter()
            .map(|a| (a.address, a.display_name, a.source))
            .collect()
    }

    fn provider(list: &[(&str, &str)]) -> Vec<ProviderAlias> {
        list.iter().map(|(a, n)| ProviderAlias { address: a.to_string(), display_name: n.to_string() }).collect()
    }

    #[test]
    fn a_provider_list_replaces_the_last_one_and_keeps_the_users_own() {
        let (store, account, dir) = store_with_account("refresh");
        store.write(|tx| add_alias(tx, account, " Mine@Example.org ", "Me at home").map(|_| ())).unwrap();
        store.write(|tx| set_provider_aliases(tx, account, &provider(&[("sales@example.com", "Sales"), ("old@example.com", "")]))).unwrap();
        let old = store.read(|c| aliases(c, account)).unwrap().into_iter().find(|a| a.address == "sales@example.com").unwrap();
        store.write(|tx| remove_alias(tx, old.id)).unwrap();

        store
            .write(|tx| {
                set_provider_aliases(tx, account, &provider(&[("sales@example.com", "Sales team"), ("new@example.com", "New"), ("mine@example.org", "Gmail's name")]))
            })
            .unwrap();
        assert_eq!(
            listed(&store, account),
            [
                ("Mine@Example.org".to_string(), "Me at home".to_string(), AliasSource::Manual),
                ("new@example.com".to_string(), "New".to_string(), AliasSource::Provider),
            ],
            "the removed one stays removed, the one the provider dropped goes, and the user's own keeps its name"
        );

        store.write(|tx| add_alias(tx, account, "sales@example.com", "Back").map(|_| ())).unwrap();
        assert!(listed(&store, account).contains(&("sales@example.com".to_string(), "Back".to_string(), AliasSource::Manual)), "adding a removed one brings it back");
        store.write(|tx| set_provider_aliases(tx, account, &[])).unwrap();
        assert_eq!(listed(&store, account).len(), 2, "only the user's own are left");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removing_and_renaming_an_alias() {
        let (store, account, dir) = store_with_account("remove");
        let alias = store.write(|tx| add_alias(tx, account, "team@example.com", "")).unwrap();
        store.write(|tx| rename_alias(tx, alias.id, "  The Team ")).unwrap();
        assert_eq!(listed(&store, account), [("team@example.com".to_string(), "The Team".to_string(), AliasSource::Manual)]);
        assert!(store.read(own_addresses).unwrap().contains(&"team@example.com".to_string()), "an alias is the user's");
        store.write(|tx| remove_alias(tx, alias.id)).unwrap();
        assert!(listed(&store, account).is_empty());
        assert!(!store.read(own_addresses).unwrap().contains(&"team@example.com".to_string()));
        store.write(|tx| set_provider_aliases(tx, account, &provider(&[("team@example.com", "")]))).unwrap();
        assert_eq!(listed(&store, account).len(), 1, "a manual one removed is gone for good, so a provider may list it again");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_alias_must_be_another_address() {
        let (store, account, dir) = store_with_account("invalid");
        for bad in ["", "not an address", "me@example.com", "ME@example.com", "a b@example.com"] {
            assert!(store.write(|tx| add_alias(tx, account, bad, "")).is_err(), "{bad:?} was taken");
        }
        store.write(|tx| add_alias(tx, account, "two@example.com", "")).unwrap();
        assert!(store.write(|tx| add_alias(tx, account, "Two@example.com", "")).is_err(), "already there");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
