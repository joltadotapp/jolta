//! Outlook's automatic replies (`mailboxSettings.automaticRepliesSetting`),
//! which `MailboxSettings.ReadWrite` from the calendar sign-in reads and
//! changes.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::{Api, GraphCalendar, When, instant};
use crate::calendar::away::{Audience, AutoReply, Away, TurnedOff};
use crate::{AccountId, Result};

const SETTING: &str = "/me/mailboxSettings/automaticRepliesSetting";

impl GraphCalendar {
    pub async fn automatic_replies(&self, account: AccountId) -> Result<Away> {
        read(self, account).await
    }

    pub async fn turn_off_automatic_replies(&self, account: AccountId, now: DateTime<Utc>) -> Result<(Away, TurnedOff)> {
        turn_off(self, account, now).await
    }

    /// An `automaticRepliesSetting` as Graph gives it.
    pub fn away_of(account: AccountId, v: &Value) -> Away {
        let when = |k: &str| serde_json::from_value::<When>(v[k].clone()).ok().and_then(|w| instant(&w));
        let reply = match v["status"].as_str() {
            Some("alwaysEnabled") => AutoReply::On { from: None, until: None },
            Some("scheduled") => AutoReply::On { from: when("scheduledStartDateTime"), until: when("scheduledEndDateTime") },
            _ => AutoReply::Off,
        };
        let audience = match v["externalAudience"].as_str() {
            Some("none") => Audience::Organization,
            Some("contactsOnly") => Audience::Contacts,
            _ => Audience::All,
        };
        let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        Away { account, reply, internal: text("internalReplyMessage"), external: text("externalReplyMessage"), audience, editable: true }
    }
}

async fn read(api: &impl Api, account: AccountId) -> Result<Away> {
    let (status, reply) = api.request("GET", SETTING, None, &[]).await?;
    if status != 200 {
        return Err(api.failure(status, &reply));
    }
    Ok(GraphCalendar::away_of(account, &reply))
}

/// Sets only the status, so Outlook keeps the window and the messages for
/// the next time.
async fn turn_off(api: &impl Api, account: AccountId, now: DateTime<Utc>) -> Result<(Away, TurnedOff)> {
    let before = read(api, account).await?;
    if !before.reply.on_at(now) {
        return Ok((before, TurnedOff::Already));
    }
    let body = json!({ "automaticRepliesSetting": { "status": "disabled" } });
    let (status, reply) = api.request("PATCH", "/me/mailboxSettings", Some(&body), &[]).await?;
    if !(200..300).contains(&status) {
        return Err(api.failure(status, &reply));
    }
    Ok((Away { reply: AutoReply::Off, ..before }, TurnedOff::Now))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::TimeZone;
    use chrono_tz::{Europe, Tz};

    use super::*;
    use crate::Error;

    fn setting(status: &str) -> Value {
        // The shape the Microsoft 365 test account returned on 2026-11-07.
        json!({
            "status": status, "externalAudience": "contactsOnly",
            "internalReplyMessage": "<html><body>Back on Monday.</body></html>",
            "externalReplyMessage": "<html><body>Away until Monday.</body></html>",
            "scheduledStartDateTime": {"dateTime": "2027-01-04T00:00:00.0000000", "timeZone": "UTC"},
            "scheduledEndDateTime": {"dateTime": "2027-01-05T00:00:00.0000000", "timeZone": "UTC"}
        })
    }

    #[test]
    fn each_status() {
        let id = AccountId(2);
        assert_eq!(GraphCalendar::away_of(id, &setting("disabled")).reply, AutoReply::Off);
        assert_eq!(GraphCalendar::away_of(id, &setting("alwaysEnabled")).reply, AutoReply::On { from: None, until: None });
        let scheduled = GraphCalendar::away_of(id, &setting("scheduled"));
        assert_eq!(
            scheduled.reply,
            AutoReply::On { from: Some(Utc.with_ymd_and_hms(2027, 1, 4, 0, 0, 0).unwrap()), until: Some(Utc.with_ymd_and_hms(2027, 1, 5, 0, 0, 0).unwrap()) }
        );
        assert_eq!((scheduled.audience, scheduled.editable), (Audience::Contacts, true));
        assert_eq!(scheduled.external, "<html><body>Away until Monday.</body></html>");
        let mut zoned = setting("scheduled");
        zoned["scheduledStartDateTime"] = json!({"dateTime": "2027-01-04T09:00:00.0000000", "timeZone": "Romance Standard Time"});
        let AutoReply::On { from, .. } = GraphCalendar::away_of(id, &zoned).reply else { panic!("on") };
        assert_eq!(from, Some(Utc.with_ymd_and_hms(2027, 1, 4, 8, 0, 0).unwrap()), "a window in Outlook's own zone");
    }

    /// Outlook's mailbox settings, recording what was sent.
    struct Mailbox {
        setting: Mutex<Value>,
        sent: Mutex<Vec<(String, Option<Value>)>>,
    }

    impl Api for Mailbox {
        async fn request(&self, method: &str, url: &str, body: Option<&Value>, _: &[(&str, &str)]) -> Result<(u16, Value)> {
            self.sent.lock().unwrap().push((format!("{method} {url}"), body.cloned()));
            let mut setting = self.setting.lock().unwrap();
            if method == "PATCH" {
                setting["status"] = body.unwrap()["automaticRepliesSetting"]["status"].clone();
                return Ok((200, json!({})));
            }
            Ok((200, setting.clone()))
        }

        fn failure(&self, status: u16, _: &Value) -> Error {
            Error::Other(format!("Microsoft Graph {status}"))
        }

        fn zone(&self) -> Tz {
            Europe::Brussels
        }
    }

    #[tokio::test]
    async fn turning_off_changes_only_the_status_and_only_while_on() {
        let mailbox = Mailbox { setting: Mutex::new(setting("scheduled")), sent: Mutex::default() };
        let during = Utc.with_ymd_and_hms(2027, 1, 4, 12, 0, 0).unwrap();
        let (away, outcome) = turn_off(&mailbox, AccountId(2), during).await.unwrap();
        assert_eq!((away.reply, outcome), (AutoReply::Off, TurnedOff::Now));
        let sent = mailbox.sent.lock().unwrap().clone();
        assert_eq!(sent[1], ("PATCH /me/mailboxSettings".into(), Some(json!({"automaticRepliesSetting": {"status": "disabled"}}))));

        let (_, again) = turn_off(&mailbox, AccountId(2), during).await.unwrap();
        assert_eq!(again, TurnedOff::Already, "twice changes nothing more");
        assert_eq!(mailbox.sent.lock().unwrap().len(), 3, "the second Turn off only reads");

        let later = Mailbox { setting: Mutex::new(setting("scheduled")), sent: Mutex::default() };
        let before = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap();
        let (away, outcome) = turn_off(&later, AccountId(2), before).await.unwrap();
        assert_eq!(outcome, TurnedOff::Already, "a window set for later stays");
        assert!(matches!(away.reply, AutoReply::On { .. }));
        assert_eq!(later.sent.lock().unwrap().len(), 1);
    }
}
