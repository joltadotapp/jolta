//! Outlook's automatic replies (`mailboxSettings.automaticRepliesSetting`),
//! which `MailboxSettings.ReadWrite` from the calendar sign-in reads and
//! changes.

use serde_json::Value;

use super::{Api, GraphCalendar, When, instant};
use crate::calendar::away::{Audience, AutoReply, Away};
use crate::{AccountId, Result};

const SETTING: &str = "/me/mailboxSettings/automaticRepliesSetting";

impl GraphCalendar {
    pub async fn automatic_replies(&self, account: AccountId) -> Result<Away> {
        read(self, account).await
    }

    /// Sends `body` to the mailbox settings and reads the replies back.
    pub async fn set_automatic_replies(&self, account: AccountId, body: &Value) -> Result<Away> {
        set(self, account, body).await
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

async fn set(api: &impl Api, account: AccountId, body: &Value) -> Result<Away> {
    let (status, reply) = api.request("PATCH", "/me/mailboxSettings", Some(body), &[]).await?;
    if !(200..300).contains(&status) {
        return Err(api.failure(status, &reply));
    }
    read(api, account).await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::{TimeZone, Utc};
    use serde_json::json;
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
                for (k, v) in body.unwrap()["automaticRepliesSetting"].as_object().unwrap() {
                    setting[k] = v.clone();
                }
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
    async fn a_change_is_sent_and_read_back() {
        let mailbox = Mailbox { setting: Mutex::new(setting("scheduled")), sent: Mutex::default() };
        let off = json!({"automaticRepliesSetting": {"status": "disabled"}});
        let away = set(&mailbox, AccountId(2), &off).await.unwrap();
        assert_eq!(away.reply, AutoReply::Off);
        assert_eq!(away.external, "<html><body>Away until Monday.</body></html>", "turning off keeps the messages for next time");
        let sent = mailbox.sent.lock().unwrap().clone();
        assert_eq!(sent, vec![("PATCH /me/mailboxSettings".into(), Some(off)), (format!("GET {SETTING}"), None)]);
    }
}
