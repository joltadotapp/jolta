//! Out of office for the app: the strip at the top of the mail list while
//! automatic replies are on, the Out of office section in Settings ›
//! Calendar, and the sheet that sets it for several accounts and turns it
//! off early.

use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use jolta_core::calendar::away::plan::{self, Absence, Blocked, Ended, Reach, Request};
use jolta_core::calendar::away::{AutoReply, Away};
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{AutoDecline, tz};
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

    /// The strip's Turn off: ends what is on now for the account, its
    /// replies on Microsoft 365 and its out-of-office events. Returns the toast.
    pub async fn turn_off_away(&self, account_id: i64) -> Result<String> {
        self.end(vec![account_id], Reach::Now).await
    }

    /// The sheet's and Settings' Turn off: ends what is on now for each
    /// account, else the next out of office set. Returns the toast.
    pub async fn end_away(&self, account_ids: Vec<i64>) -> Result<String> {
        self.end(account_ids, Reach::NowOrNext).await
    }

    /// The sheet's lines for the request: what each account will get.
    pub fn away_plan(&self, request: AwayRequest) -> Result<AwayPlan> {
        let here = tz::here();
        let now = Utc::now();
        let request = request.core(here)?;
        let (targets, aways, events) = self.jolta.store.read(|c| {
            let targets = plan::targets(c)?;
            let events = targets.iter().map(|t| plan::away_events(c, &t.account, now, here)).collect::<jolta_core::Result<Vec<_>>>()?;
            Ok((targets, store::aways(c)?, events))
        })?;
        let rows = plan::plan(&targets, &request, here)
            .into_iter()
            .zip(&targets)
            .zip(&events)
            .map(|((step, target), events)| {
                let replies = aways.iter().find(|w| w.account == step.account && w.editable).map(|w| w.reply);
                row(&target.account, step, plan::current(replies, events, now, here), now, here)
            })
            .collect();
        Ok(AwayPlan { rows, problem: request.problem(now, here).map(str::to_string) })
    }

    /// Sets out of office for the request's accounts. Returns the toast.
    pub async fn set_away(&self, request: AwayRequest) -> Result<String> {
        let here = tz::here();
        let request = request.core(here)?;
        if let Some(problem) = request.problem(Utc::now(), here) {
            return Err(CoreError::Failed { message: problem.into() });
        }
        let (targets, accounts) = self.jolta.store.read(|c| Ok((plan::targets(c)?, store::accounts(c)?)))?;
        let steps: Vec<plan::Step> = plan::plan(&targets, &request, here).into_iter().filter(|s| request.accounts.contains(&s.account)).collect();
        let jolta = self.jolta.clone();
        let (applied, calendars) = self
            .runtime
            .spawn(async move { plan::apply(&jolta, &steps, &Horizon::around(Utc::now(), here), here).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?;
        self.after_away(calendars);
        let name = |id: AccountId| accounts.iter().find(|a| a.id == id).map_or_else(|| format!("account {}", id.0), |a| a.address.clone());
        let set: Vec<String> = applied.iter().filter(|a| a.result.is_ok()).map(|a| name(a.account)).collect();
        let failed: Vec<String> = applied.iter().filter_map(|a| a.result.as_ref().err().map(|e| format!("Couldn't set it for {}: {e}", name(a.account)))).collect();
        let mut parts = Vec::new();
        if !set.is_empty() {
            parts.push(format!("Out of office is set for {}.", list(&set)));
        }
        parts.extend(failed);
        if parts.is_empty() {
            parts.push("Pick an account to set out of office for.".into());
        }
        Ok(parts.join(" "))
    }

    /// What the sheet starts from: the account's replies on Microsoft 365
    /// when it has some set, else tomorrow, with the last reply Jolta read.
    pub fn away_draft(&self, account_id: Option<i64>) -> Result<AwayRequest> {
        let here = tz::here();
        let now = Utc::now();
        let aways = self.jolta.store.read(store::aways)?;
        let set = account_id.and_then(|id| aways.iter().find(|w| w.account.0 == id && w.editable)).and_then(|w| match w.reply {
            AutoReply::On { from, until: Some(until) } if until > now => Some((w, from.unwrap_or(now), until)),
            _ => None,
        });
        let audience = |w: &Away| match w.audience {
            jolta_core::calendar::away::Audience::All => AwayAudience::Everyone,
            jolta_core::calendar::away::Audience::Contacts => AwayAudience::Contacts,
            jolta_core::calendar::away::Audience::Organization => AwayAudience::Organization,
        };
        let last = aways.iter().filter(|w| w.editable && !w.internal.is_empty()).max_by_key(|w| w.account.0);
        Ok(match set {
            Some((w, from, until)) => {
                let days = is_midnight(from, here) && is_midnight(until, here);
                let until = if days { tz::instant(last_day(until, here).and_time(NaiveTime::MIN), here) } else { until };
                AwayRequest {
                    account_ids: vec![w.account.0],
                    all_day: days,
                    from: from.timestamp(),
                    until: until.timestamp(),
                    message: plan::text(&w.internal),
                    audience: audience(w),
                    declines: AwayDeclines::NewInvitations,
                }
            }
            None => {
                let tomorrow = (now.with_timezone(&here).date_naive() + Duration::days(1)).and_time(NaiveTime::MIN);
                let start = tz::instant(tomorrow, here).timestamp();
                AwayRequest {
                    account_ids: account_id.into_iter().collect(),
                    all_day: true,
                    from: start,
                    until: start,
                    message: last.map(|w| plan::text(&w.internal)).unwrap_or_default(),
                    audience: last.map_or(AwayAudience::Everyone, audience),
                    declines: AwayDeclines::NewInvitations,
                }
            }
        })
    }
}

impl Core {
    async fn end(&self, account_ids: Vec<i64>, reach: Reach) -> Result<String> {
        let here = tz::here();
        let jolta = self.jolta.clone();
        let ids = account_ids.clone();
        let results = self
            .runtime
            .spawn(async move {
                let horizon = Horizon::around(Utc::now(), here);
                let mut out = Vec::new();
                for id in ids {
                    out.push((AccountId(id), plan::end(&jolta, AccountId(id), reach, &horizon, here).await));
                }
                out
            })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })?;
        let calendars = results.iter().filter_map(|(_, r)| r.as_ref().ok()).flat_map(|(_, c)| c.iter().copied()).collect();
        self.after_away(calendars);
        let (accounts, aways) = self.aways()?;
        let now = Utc::now();
        let mut parts = Vec::new();
        for (id, result) in results {
            let Some(a) = accounts.iter().find(|a| a.id == id) else { continue };
            let gmail_on = a.kind == AccountKind::Gmail && aways.iter().any(|w| w.account == id && w.reply.on_at(now));
            parts.push(match result {
                Err(e) => format!("Couldn't turn out of office off for {}: {e}", a.address),
                Ok((Ended { replies: false, events: 0 }, _)) if gmail_on => format!("Gmail's auto-reply is on for {}; turn it off in Gmail.", a.address),
                Ok((Ended { replies: false, events: 0 }, _)) => format!("Out of office was off already for {}.", a.address),
                Ok(_) if gmail_on => format!("Out of office is off for {}. Gmail's auto-reply is still on; turn it off in Gmail.", a.address),
                Ok(_) => format!("Out of office is off for {}.", a.address),
            });
        }
        Ok(parts.join(" "))
    }

    fn after_away(&self, calendars: Vec<jolta_core::calendar::CalendarId>) {
        if let Some(scheduler) = self.scheduler() {
            scheduler.ops_queued();
        }
        if !calendars.is_empty() {
            self.calendar_changed(calendars);
        }
        self.listener.on_event(crate::CoreEvent::AwayChanged);
    }
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AwayAudience {
    Everyone,
    /// People in the organization and the user's contacts.
    Contacts,
    Organization,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AwayDeclines {
    Nothing,
    NewInvitations,
    /// New invitations and the meetings already accepted.
    Everything,
}

/// Out of office as the sheet holds it.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct AwayRequest {
    /// The accounts ticked; empty while the sheet starts.
    pub account_ids: Vec<i64>,
    /// Whole days: `from` and `until` name the first and last day by any
    /// moment in it on the Mac's clock.
    pub all_day: bool,
    /// Unix seconds.
    pub from: i64,
    pub until: i64,
    pub message: String,
    pub audience: AwayAudience,
    pub declines: AwayDeclines,
}

impl AwayRequest {
    fn core(&self, here: Tz) -> Result<Request> {
        let at = |s: i64| DateTime::from_timestamp(s, 0).ok_or_else(|| CoreError::Failed { message: format!("{s} isn't a time") });
        let (from, until) = (at(self.from)?, at(self.until)?);
        let absence = if self.all_day {
            Absence::Days { first: from.with_timezone(&here).date_naive(), last: until.with_timezone(&here).date_naive() }
        } else {
            Absence::Times { from, until }
        };
        Ok(Request {
            accounts: self.account_ids.iter().map(|&id| AccountId(id)).collect(),
            absence,
            message: self.message.clone(),
            audience: match self.audience {
                AwayAudience::Everyone => jolta_core::calendar::away::Audience::All,
                AwayAudience::Contacts => jolta_core::calendar::away::Audience::Contacts,
                AwayAudience::Organization => jolta_core::calendar::away::Audience::Organization,
            },
            declines: match self.declines {
                AwayDeclines::Nothing => AutoDecline::None,
                AwayDeclines::NewInvitations => AutoDecline::New,
                AwayDeclines::Everything => AutoDecline::All,
            },
        })
    }
}

/// One account's line in the sheet.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct AwayRow {
    pub account_id: i64,
    pub address: String,
    /// "Microsoft 365", "Google Workspace", "Gmail", "iCloud".
    pub provider: String,
    /// What Turn on does for it.
    pub line: String,
    pub link_title: Option<String>,
    pub link_url: Option<String>,
    /// Ticked when the sheet opens.
    pub ticked: bool,
    /// False when Jolta can't set it: `calendar_off` says whether turning
    /// calendar on would help.
    pub enabled: bool,
    pub calendar_off: bool,
    /// "Out of office now, until Fri 13 Nov." when some is set, for Turn off.
    pub current: Option<String>,
    /// Turn on sets automatic replies, whose audience the sheet asks for.
    pub sets_replies: bool,
    /// Turn on adds an event that declines invitations as the sheet says.
    pub declines_invitations: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct AwayPlan {
    pub rows: Vec<AwayRow>,
    /// Why Turn on is unavailable: "The end is before the start."
    pub problem: Option<String>,
}

fn provider(a: &Account) -> String {
    let consumer = ["@gmail.com", "@googlemail.com"].iter().any(|d| a.address.to_ascii_lowercase().ends_with(d));
    match a.kind {
        AccountKind::Gmail if consumer => "Gmail".into(),
        AccountKind::Gmail => "Google Workspace".into(),
        _ => a.provider_label().into(),
    }
}

fn row(a: &Account, step: plan::Step, current: Option<(DateTime<Utc>, DateTime<Utc>)>, now: DateTime<Utc>, here: Tz) -> AwayRow {
    let (link_title, link_url) = step.link.map_or((None, None), |(t, u)| (Some(t), Some(u)));
    let declines_invitations = step.event.as_ref().is_some_and(|p| matches!(p.event.kind, jolta_core::calendar::EventKind::OutOfOffice { .. }));
    AwayRow {
        sets_replies: step.replies.is_some(),
        declines_invitations,
        account_id: a.id.0,
        address: a.address.clone(),
        provider: provider(a),
        line: step.line,
        link_title,
        link_url,
        ticked: step.ticked && step.blocked.is_none(),
        enabled: step.blocked.is_none(),
        calendar_off: step.blocked == Some(Blocked::CalendarOff),
        current: current.map(|w| window_text(w, now, here)),
    }
}

/// The last day a window that ends at midnight on `here` covers: the day
/// before, by the calendar, which is 23 or 25 hours earlier when the clocks change.
fn last_day(until: DateTime<Utc>, here: Tz) -> chrono::NaiveDate {
    until.with_timezone(&here).date_naive() - Duration::days(1)
}

fn is_midnight(t: DateTime<Utc>, here: Tz) -> bool {
    t.with_timezone(&here).time() == NaiveTime::MIN
}

/// "Out of office now, until Fri 13 Nov." or "Out of office from Wed 11
/// Nov to Fri 13 Nov.": days when the window runs midnight to midnight.
fn window_text((from, until): (DateTime<Utc>, DateTime<Utc>), now: DateTime<Utc>, here: Tz) -> String {
    let days = is_midnight(from, here) && is_midnight(until, here);
    let day = |t: DateTime<Utc>| t.with_timezone(&here).format("%a %-d %b").to_string();
    let end = if until == DateTime::<Utc>::MAX_UTC {
        None
    } else if days {
        Some(last_day(until, here).format("%a %-d %b").to_string())
    } else {
        Some(moment(until))
    };
    let start = if days { day(from) } else { moment(from) };
    match (from <= now, end) {
        (true, Some(end)) => format!("Out of office now, until {end}."),
        (true, None) => "Out of office now, with no end set.".into(),
        (false, Some(end)) if end == start => format!("Out of office on {start}."),
        (false, Some(end)) => format!("Out of office from {start} to {end}."),
        (false, None) => format!("Out of office from {start}, with no end set."),
    }
}

/// "a@x", "a@x and b@y", "a@x, b@y and c@z".
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
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
            oauth_user_id: None,
        }
    }

    fn away(reply: AutoReply) -> Away {
        Away { account: AccountId(4), reply, internal: String::new(), external: String::new(), audience: Audience::All, editable: true }
    }

    #[test]
    fn the_sheet_says_when_out_of_office_is_set() {
        let here = chrono_tz::Europe::Brussels;
        let at = |d, h| Utc.with_ymd_and_hms(2026, 11, d, h, 0, 0).unwrap();
        let now = at(9, 9);
        assert_eq!(window_text((at(10, 23), at(13, 23)), now, here), "Out of office from Wed 11 Nov to Fri 13 Nov.");
        assert_eq!(window_text((at(10, 23), at(11, 23)), now, here), "Out of office on Wed 11 Nov.");
        assert_eq!(window_text((at(8, 23), at(13, 23)), now, here), "Out of office now, until Fri 13 Nov.");
        assert_eq!(window_text((at(9, 8), at(9, 16)), now, here), "Out of office now, until Mon 9 Nov, 17:00.");
        assert_eq!(window_text((at(9, 8), DateTime::<Utc>::MAX_UTC), now, here), "Out of office now, with no end set.");
        assert_eq!(list(&["a".into(), "b".into(), "c".into()]), "a, b and c");
        // Brussels moves to summer time on Sunday 28 March 2027.
        let spring = |d, h| Utc.with_ymd_and_hms(2027, 3, d, h, 0, 0).unwrap();
        assert_eq!(window_text((spring(26, 23), spring(28, 22)), now, here), "Out of office from Sat 27 Mar to Sun 28 Mar.");
    }

    #[test]
    fn whole_days_from_the_sheet() {
        let here = chrono_tz::Europe::Brussels;
        let r = AwayRequest {
            account_ids: vec![2],
            all_day: true,
            from: Utc.with_ymd_and_hms(2026, 11, 10, 23, 0, 0).unwrap().timestamp(),
            until: Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap().timestamp(),
            message: String::new(),
            audience: AwayAudience::Contacts,
            declines: AwayDeclines::Everything,
        };
        let core = r.core(here).unwrap();
        let day = |d| chrono::NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        assert_eq!(core.absence, Absence::Days { first: day(11), last: day(13) }, "each day on the Mac's clock");
        assert_eq!((core.declines, core.accounts), (AutoDecline::All, vec![AccountId(2)]));
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
