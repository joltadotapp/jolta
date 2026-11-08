//! Automatic replies for the app: the strip at the top of the mail list
//! while they are on, the Out of office section in Settings › Calendar,
//! and Turn off where Jolta can change them.

use chrono::{DateTime, Utc};
use jolta_core::calendar::away::{self, AutoReply, Away, TurnedOff};
use jolta_core::calendar::tz;
use jolta_core::{Account, AccountId, AccountKind, store};

use crate::{Core, CoreError, Result};

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum AwayState {
    Off,
    /// On now; `until` in unix seconds when they end by themselves.
    On { until: Option<i64> },
    /// Set to start later.
    Later { from: i64 },
    /// A Microsoft 365 account with calendar off: Jolta has no grant to read them.
    CalendarOff,
    /// Not read yet, or the last read failed.
    Unknown,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct AwayInfo {
    pub account_id: i64,
    pub address: String,
    pub state: AwayState,
    /// "Fri 13 Nov, 18:00", when the replies on now end by themselves.
    pub until_text: Option<String>,
    /// The Settings line: "On until Fri 13 Nov, 18:00.", "Off."
    pub detail: String,
    /// Turn off changes them in Jolta: Microsoft 365. Elsewhere the
    /// provider's own setting opens.
    pub editable: bool,
    /// The provider's own setting, for Edit, and Turn off on Gmail.
    pub settings_url: String,
}

fn moment(t: DateTime<Utc>) -> String {
    t.with_timezone(&tz::here()).format("%a %-d %b, %H:%M").to_string()
}

fn settings_url(a: &Account) -> String {
    match a.kind {
        AccountKind::Gmail => format!("https://mail.google.com/mail/u/?authuser={}#settings/general", a.address),
        _ => "https://outlook.office.com/mail/options/mail/automaticReplies".into(),
    }
}

fn info(a: &Account, stored: Option<&Away>, now: DateTime<Utc>) -> AwayInfo {
    let state = match stored.map(|s| s.reply) {
        None if a.kind == AccountKind::Graph && !a.calendar => AwayState::CalendarOff,
        None => AwayState::Unknown,
        Some(reply) if reply.on_at(now) => AwayState::On { until: match reply {
            AutoReply::On { until, .. } => until.map(|t| t.timestamp()),
            AutoReply::Off => None,
        } },
        Some(AutoReply::On { from: Some(from), .. }) if from > now => AwayState::Later { from: from.timestamp() },
        Some(_) => AwayState::Off,
    };
    let until_text = match (&state, stored.map(|s| s.reply)) {
        (AwayState::On { .. }, Some(AutoReply::On { until: Some(u), .. })) => Some(moment(u)),
        _ => None,
    };
    let detail = match (&state, stored.map(|s| s.reply)) {
        (AwayState::On { .. }, _) => until_text.as_ref().map_or("On, with no end set.".into(), |u| format!("On until {u}.")),
        (AwayState::Later { .. }, Some(AutoReply::On { from: Some(f), until })) => match until {
            Some(u) => format!("Set for {} to {}.", moment(f), moment(u)),
            None => format!("Set to start {}.", moment(f)),
        },
        (AwayState::CalendarOff, _) => "Turn on calendar for this account, and Jolta can read them.".into(),
        (AwayState::Unknown, _) => "Jolta hasn't read them yet.".into(),
        _ => "Off.".into(),
    };
    AwayInfo {
        account_id: a.id.0,
        address: a.address.clone(),
        state,
        until_text,
        detail,
        editable: stored.is_some_and(|s| s.editable),
        settings_url: settings_url(a),
    }
}

impl Core {
    fn aways(&self) -> Result<(Vec<Account>, Vec<Away>)> {
        Ok(self.jolta.store.read(|c| Ok((store::accounts(c)?, store::aways(c)?)))?)
    }
}

#[uniffi::export]
impl Core {
    /// The accounts whose automatic replies are on now, for the strip.
    pub fn away_states(&self) -> Result<Vec<AwayInfo>> {
        let (accounts, aways) = self.aways()?;
        let now = Utc::now();
        Ok(aways
            .iter()
            .filter(|w| w.reply.on_at(now))
            .filter_map(|w| accounts.iter().find(|a| a.id == w.account).map(|a| info(a, Some(w), now)))
            .collect())
    }

    /// Every Google and Microsoft 365 account's automatic replies, for
    /// Settings › Calendar.
    pub fn away_accounts(&self) -> Result<Vec<AwayInfo>> {
        let (accounts, aways) = self.aways()?;
        let now = Utc::now();
        Ok(accounts
            .iter()
            .map(|a| (a, aways.iter().find(|w| w.account == a.id)))
            .filter(|(a, stored)| stored.is_some() || matches!(a.kind, AccountKind::Gmail | AccountKind::Graph))
            .map(|(a, stored)| info(a, stored, now))
            .collect())
    }

    /// Reads every account's automatic replies again, as when the app
    /// becomes active. `AwayChanged` says when something changed.
    pub fn check_away(&self) {
        if let Some(scheduler) = self.scheduler() {
            scheduler.check_away();
        }
    }

    /// Turns the account's automatic replies off on Microsoft 365. Returns
    /// the toast.
    pub async fn turn_off_away(&self, account_id: i64) -> Result<String> {
        let jolta = self.jolta.clone();
        let outcome = self
            .runtime
            .spawn(async move { away::turn_off(&jolta, AccountId(account_id)).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        self.listener.on_event(crate::CoreEvent::AwayChanged);
        Ok(match outcome {
            TurnedOff::Now => "Automatic replies are off.".into(),
            TurnedOff::Already => "Automatic replies were off already.".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use jolta_core::calendar::away::Audience;

    use super::*;

    fn account(kind: AccountKind, calendar: bool) -> Account {
        use jolta_core::{Security, SmtpAuth};
        Account {
            id: AccountId(4),
            kind,
            address: "me@example.com".into(),
            display_name: String::new(),
            username: String::new(),
            imap_host: String::new(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 587,
            secret_ref: String::new(),
            imap_security: Security::Tls,
            smtp_security: Security::Tls,
            smtp_auth: SmtpAuth::SameAsIncoming,
            smtp_username: String::new(),
            verify_certs: true,
            signature: String::new(),
            calendar,
        }
    }

    fn away(reply: AutoReply) -> Away {
        Away { account: AccountId(4), reply, internal: String::new(), external: String::new(), audience: Audience::All, editable: true }
    }

    #[test]
    fn states_for_the_strip_and_settings() {
        let now = Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap();
        let until = Utc.with_ymd_and_hms(2026, 11, 13, 17, 0, 0).unwrap();
        let graph = account(AccountKind::Graph, true);
        let on = info(&graph, Some(&away(AutoReply::On { from: None, until: Some(until) })), now);
        assert_eq!(on.state, AwayState::On { until: Some(until.timestamp()) });
        assert!(on.until_text.is_some() && on.editable);
        let later = info(&graph, Some(&away(AutoReply::On { from: Some(until), until: None })), now);
        assert_eq!(later.state, AwayState::Later { from: until.timestamp() });
        let ended = info(&graph, Some(&away(AutoReply::On { from: None, until: Some(now) })), now);
        assert_eq!((ended.state, ended.until_text), (AwayState::Off, None));
        assert_eq!(info(&account(AccountKind::Graph, false), None, now).state, AwayState::CalendarOff);
        let gmail = info(&account(AccountKind::Gmail, false), None, now);
        assert_eq!(gmail.state, AwayState::Unknown);
        assert_eq!(gmail.settings_url, "https://mail.google.com/mail/u/?authuser=me@example.com#settings/general");
    }
}
