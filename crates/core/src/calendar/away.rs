//! Automatic replies: whether each Google and Microsoft 365 account answers
//! mail with an out-of-office reply, as its provider last said, so the mail
//! list can say so while they are on.
//!
//! Microsoft 365 keeps them in `mailboxSettings.automaticRepliesSetting`,
//! which the calendar sign-in may read and change. Gmail keeps its vacation
//! responder in `settings/vacation`, which Jolta's mail scope may read but
//! not change, so Gmail's are changed in Gmail. Other accounts have none
//! Jolta can see.
//!
//! A watcher reads every account's state every 30 minutes and when asked,
//! as when the app becomes active, and says when what is on now changes.
//! A data folder holding `away.json` (the demo and UI tests) answers from
//! that file in place of the providers.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::sources::graph::GraphCalendar;
use crate::engine::{Event, Sink};
use crate::model::text_enum;
use crate::{Account, AccountId, AccountKind, Error, Jolta, Result, gmail_settings, store};

const EVERY: chrono::Duration = chrono::Duration::minutes(30);
/// Short, because tokio's clock stops while the Mac sleeps.
const TICK: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoReply {
    Off,
    /// Replies go out from `from` until `until`, either end open when None.
    On { from: Option<DateTime<Utc>>, until: Option<DateTime<Utc>> },
}

impl AutoReply {
    /// Whether replies go out at `now`: the start counts, the end doesn't.
    pub fn on_at(&self, now: DateTime<Utc>) -> bool {
        match *self {
            AutoReply::Off => false,
            AutoReply::On { from, until } => from.is_none_or(|f| f <= now) && until.is_none_or(|u| now < u),
        }
    }
}

// Who outside the organization gets the reply. People inside it always do.
text_enum!(Audience {
    All => "all",
    Contacts => "contacts",
    Organization => "organization",
});

/// One account's automatic replies as its provider said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Away {
    pub account: AccountId,
    pub reply: AutoReply,
    /// The reply to people in the organization, and to everyone else, as
    /// the provider keeps them: HTML on Microsoft 365, text on Gmail.
    pub internal: String,
    pub external: String,
    pub audience: Audience,
    /// Jolta can change them: Microsoft 365. Gmail's are changed in Gmail.
    pub editable: bool,
}

/// Gmail's vacation responder (`users.settings.vacation`). Its times are
/// milliseconds, sent as JSON strings.
pub fn from_gmail(account: AccountId, v: &Value) -> Away {
    let ms = |k: &str| match &v[k] {
        Value::String(s) => s.parse::<i64>().ok(),
        n => n.as_i64(),
    };
    let at = |k: &str| ms(k).and_then(DateTime::from_timestamp_millis);
    let reply = match v["enableAutoReply"].as_bool() {
        Some(true) => AutoReply::On { from: at("startTime"), until: at("endTime") },
        _ => AutoReply::Off,
    };
    let text = [&v["responseBodyPlainText"], &v["responseBodyHtml"]].into_iter().filter_map(Value::as_str).find(|t| !t.is_empty()).unwrap_or_default();
    let audience = if v["restrictToDomain"].as_bool() == Some(true) {
        Audience::Organization
    } else if v["restrictToContacts"].as_bool() == Some(true) {
        Audience::Contacts
    } else {
        Audience::All
    };
    Away { account, reply, internal: text.to_string(), external: text.to_string(), audience, editable: false }
}

/// Reads the account's automatic replies from its provider. None when Jolta
/// can't know them: a Microsoft 365 account with calendar off has no
/// mailbox settings grant, and other accounts keep none Jolta can reach.
pub async fn read(jolta: &Jolta, account: &Account) -> Result<Option<Away>> {
    if let Some(file) = recorded(&jolta.data_dir)? {
        return Ok(file.get(&account.address.to_lowercase()).map(|v| recorded_away(account.id, v)));
    }
    match account.kind {
        AccountKind::Graph if account.calendar => GraphCalendar::new(account)?.automatic_replies(account.id).await.map(Some),
        AccountKind::Gmail => Ok(Some(from_gmail(account.id, &gmail_settings::vacation(account).await?))),
        AccountKind::Graph | AccountKind::Imap => Ok(None),
    }
}

/// What Turn off did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnedOff {
    Now,
    /// The provider said they weren't on any more, so nothing was changed.
    Already,
}

/// Turns the account's automatic replies off, where Jolta can: Microsoft
/// 365. It reads them first and changes nothing unless they are on now,
/// so a stale strip never turns off a window set elsewhere since.
pub async fn turn_off(jolta: &Jolta, account: AccountId) -> Result<TurnedOff> {
    let a = jolta.store.read_async(move |c| store::account(c, account)).await?;
    let (away, outcome) = match recorded(&jolta.data_dir)? {
        Some(file) => turn_off_recorded(&jolta.data_dir, file, &a)?,
        None if a.kind == AccountKind::Graph && a.calendar => GraphCalendar::new(&a)?.turn_off_automatic_replies(account, Utc::now()).await?,
        None if a.kind == AccountKind::Gmail => return Err(Error::Other("Jolta can't change Gmail's vacation responder. Turn it off in Gmail's settings.".into())),
        None => return Err(Error::Other(format!("Jolta can't change automatic replies for {}.", a.address))),
    };
    let fetched = Utc::now().timestamp_millis();
    jolta.store.write_async(move |tx| store::save_away(tx, &away, fetched)).await?;
    Ok(outcome)
}

/// Reads every account's state and stores it. Returns whether anything
/// stored changed. One account failing leaves its last state stored.
pub async fn refresh(jolta: &Jolta) -> Result<bool> {
    let accounts = jolta.store.read_async(store::accounts).await?;
    let mut changed = false;
    for a in accounts {
        let fetched = Utc::now().timestamp_millis();
        let id = a.id;
        let wrote = match read(jolta, &a).await {
            Ok(Some(away)) => jolta.store.write_async(move |tx| store::save_away(tx, &away, fetched)).await,
            Ok(None) => jolta.store.write_async(move |tx| store::forget_away(tx, id)).await,
            Err(e) => {
                tracing::info!("automatic replies of account {} not read: {e}", id.0);
                Ok(false)
            }
        };
        changed |= wrote.unwrap_or_else(|e| {
            tracing::info!("automatic replies of account {} not stored: {e}", id.0);
            false
        });
    }
    Ok(changed)
}

/// Keeps the stored states current while the app runs.
pub struct Watcher {
    jolta: Jolta,
    sink: Sink,
    wake: Notify,
}

impl Watcher {
    pub fn start(jolta: Jolta, sink: Sink, runtime: &tokio::runtime::Handle) -> Arc<Watcher> {
        let watcher = Arc::new(Watcher { jolta, sink, wake: Notify::new() });
        runtime.spawn(watcher.clone().run());
        watcher
    }

    /// Reads every account's state now.
    pub fn check(&self) {
        self.wake.notify_one();
    }

    async fn run(self: Arc<Self>) {
        let mut shown = BTreeSet::new();
        let mut read_at: Option<DateTime<Utc>> = None;
        loop {
            let now = Utc::now();
            let mut changed = false;
            if read_at.is_none_or(|t| now - t >= EVERY) {
                changed = refresh(&self.jolta).await.unwrap_or_else(|e| {
                    tracing::info!("automatic replies not read: {e}");
                    false
                });
                read_at = Some(now);
            }
            // A window that starts or ends shows within the minute.
            let aways = self.jolta.store.read_async(store::aways).await.unwrap_or_default();
            let now = Utc::now();
            let on: BTreeSet<AccountId> = aways.iter().filter(|a| a.reply.on_at(now)).map(|a| a.account).collect();
            if changed || on != shown {
                shown = on;
                (self.sink)(Event::AwayChanged);
            }
            tokio::select! {
                _ = self.wake.notified() => read_at = None,
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }
}

/// `away.json`: by address, Microsoft 365's `automaticRepliesSetting` or
/// Gmail's `vacation`, as the provider gives them.
fn recorded(dir: &Path) -> Result<Option<BTreeMap<String, Value>>> {
    match std::fs::read(dir.join("away.json")) {
        Ok(bytes) => {
            let file: BTreeMap<String, Value> = serde_json::from_slice(&bytes).map_err(|e| Error::Other(format!("away.json: {e}")))?;
            Ok(Some(file.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn recorded_away(account: AccountId, v: &Value) -> Away {
    match v.get("automaticRepliesSetting") {
        Some(setting) => GraphCalendar::away_of(account, setting),
        None => from_gmail(account, &v["vacation"]),
    }
}

fn turn_off_recorded(dir: &Path, mut file: BTreeMap<String, Value>, a: &Account) -> Result<(Away, TurnedOff)> {
    let key = a.address.to_lowercase();
    let entry = file.get_mut(&key).filter(|v| v.get("automaticRepliesSetting").is_some());
    let Some(entry) = entry else {
        return Err(Error::Other(format!("Jolta can't change automatic replies for {}.", a.address)));
    };
    if !recorded_away(a.id, entry).reply.on_at(Utc::now()) {
        return Ok((recorded_away(a.id, entry), TurnedOff::Already));
    }
    entry["automaticRepliesSetting"]["status"] = json!("disabled");
    let away = recorded_away(a.id, entry);
    std::fs::write(dir.join("away.json"), serde_json::to_vec_pretty(&file).map_err(|e| Error::Other(e.to_string()))?)?;
    Ok((away, TurnedOff::Now))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        Utc.from_utc_datetime(&chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    #[test]
    fn on_now_includes_the_start_and_excludes_the_end() {
        let window = AutoReply::On { from: Some(t("2026-11-11 08:00")), until: Some(t("2026-11-13 17:00")) };
        assert!(!window.on_at(t("2026-11-11 07:59")));
        assert!(window.on_at(t("2026-11-11 08:00")), "the start counts");
        assert!(window.on_at(t("2026-11-13 16:59")));
        assert!(!window.on_at(t("2026-11-13 17:00")), "the end doesn't");
        assert!(AutoReply::On { from: None, until: None }.on_at(t("2030-01-01 00:00")));
        assert!(AutoReply::On { from: Some(t("2026-11-11 08:00")), until: None }.on_at(t("2027-01-01 00:00")));
        assert!(!AutoReply::Off.on_at(t("2026-11-12 00:00")));
    }

    #[test]
    fn gmail_vacation_with_and_without_an_end() {
        // The shape the Google Workspace test account returned on 2026-11-07.
        let off = json!({"enableAutoReply": false, "responseBodyPlainText": "", "restrictToContacts": false, "restrictToDomain": false});
        assert_eq!(from_gmail(AccountId(3), &off).reply, AutoReply::Off);

        let open = json!({"enableAutoReply": true, "responseSubject": "Away", "responseBodyPlainText": "Back on Monday.",
                          "restrictToContacts": false, "restrictToDomain": true, "startTime": "1762819200000"});
        let away = from_gmail(AccountId(3), &open);
        assert_eq!(away.reply, AutoReply::On { from: Some(t("2025-11-11 00:00")), until: None });
        assert_eq!((away.internal.as_str(), away.audience, away.editable), ("Back on Monday.", Audience::Organization, false));

        let window = json!({"enableAutoReply": true, "responseBodyHtml": "<p>Back on Monday.</p>", "restrictToContacts": true,
                            "startTime": "1762819200000", "endTime": 1763078400000_i64});
        let away = from_gmail(AccountId(3), &window);
        assert_eq!(away.reply, AutoReply::On { from: Some(t("2025-11-11 00:00")), until: Some(t("2025-11-14 00:00")) });
        assert_eq!((away.external.as_str(), away.audience), ("<p>Back on Monday.</p>", Audience::Contacts));
    }
}
