//! Other people's busy times on Microsoft 365 (`getSchedule`) and the
//! user's working hours (`mailboxSettings.workingHours`).

use chrono::{DateTime, NaiveTime, Utc, Weekday};
use serde_json::{Value, json};

use super::{Api, GraphCalendar, When, instant};
use crate::calendar::availability::{Availability, Busy, BusyKind, Schedule, Unknown, Weekdays, WorkingHours};
use crate::calendar::tz;
use crate::Result;

/// Schedules per request. The getSchedule reference names no limit;
/// Exchange's availability service takes 100 mailboxes.
const PER_REQUEST: usize = 100;

impl GraphCalendar {
    /// Each person's busy times from `from` to `to`, in the order asked.
    pub async fn free_busy(&self, people: &[String], from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Schedule>> {
        let at = |t: DateTime<Utc>| json!({ "dateTime": t.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" });
        let mut out = Vec::with_capacity(people.len());
        for chunk in people.chunks(PER_REQUEST) {
            let body = json!({ "schedules": chunk, "startTime": at(from), "endTime": at(to), "availabilityViewInterval": 15 });
            let (status, reply) = Api::request(self, "POST", "/me/calendar/getSchedule", Some(&body), &[]).await?;
            if status != 200 {
                return Err(Api::failure(self, status, &reply));
            }
            out.extend(schedules(&reply, chunk));
        }
        Ok(out)
    }

    /// The user's working hours as Outlook keeps them, when set.
    pub async fn working_hours(&self) -> Result<Option<WorkingHours>> {
        let (status, reply) = Api::request(self, "GET", "/me/mailboxSettings/workingHours", None, &[]).await?;
        if status != 200 {
            return Err(Api::failure(self, status, &reply));
        }
        Ok(working_hours(&reply, Api::zone(self)))
    }
}

/// Graph answers in the order asked, each schedule with its `scheduleId`.
fn schedules(reply: &Value, people: &[String]) -> Vec<Schedule> {
    let answers = reply["value"].as_array().map(Vec::as_slice).unwrap_or_default();
    people
        .iter()
        .map(|person| {
            let found = answers.iter().find(|s| s["scheduleId"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(person)));
            let availability = match found {
                None => Availability::Unknown(Unknown::Failed("Microsoft gave no answer".into())),
                Some(s) if s["error"].is_object() => Availability::Unknown(unknown(&s["error"])),
                Some(s) => Availability::Known(s["scheduleItems"].as_array().into_iter().flatten().filter_map(busy).collect()),
            };
            Schedule { person: person.clone(), availability }
        })
        .collect()
}

fn unknown(error: &Value) -> Unknown {
    let code = error["responseCode"].as_str().unwrap_or_default();
    match code {
        "ErrorMailRecipientNotFound" | "ErrorInvalidSmtpAddress" => Unknown::NotFound,
        "ErrorNoFreeBusyAccess" | "ErrorFreeBusyGenerationFailed" | "ErrorProxyRequestNotAllowed" => Unknown::NotShared,
        _ => Unknown::Failed(error["message"].as_str().filter(|m| !m.is_empty()).unwrap_or(code).to_string()),
    }
}

fn busy(item: &Value) -> Option<Busy> {
    let kind = match item["status"].as_str()? {
        "busy" => BusyKind::Busy,
        "tentative" => BusyKind::Tentative,
        "oof" => BusyKind::OutOfOffice,
        "workingElsewhere" => BusyKind::Elsewhere,
        _ => return None,
    };
    let when = |v: &Value| serde_json::from_value::<When>(v.clone()).ok().and_then(|w| instant(&w));
    Some(Busy { start: when(&item["start"])?, end: when(&item["end"])?, kind })
}

/// `here` stands in for a zone Graph names in a way Jolta can't map.
fn working_hours(v: &Value, here: chrono_tz::Tz) -> Option<WorkingHours> {
    let time = |k: &str| v[k].as_str().and_then(|t| NaiveTime::parse_from_str(t.get(..8)?, "%H:%M:%S").ok());
    let days = v["daysOfWeek"].as_array()?.iter().filter_map(|d| d.as_str()?.parse::<Weekday>().ok());
    let zone = v["timeZone"]["name"].as_str().and_then(tz::zone).unwrap_or(here);
    Some(WorkingHours { days: Weekdays::of(days), start: time("startTime")?, end: time("endTime")?, zone })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe;

    use super::*;

    #[test]
    fn schedule_items_by_status_and_errors_per_schedule() {
        let reply = json!({"value": [
            {"scheduleId": "me@example.com", "availabilityView": "0022", "scheduleItems": [
                {"isPrivate": false, "status": "busy", "subject": "Planning", "location": "",
                 "start": {"dateTime": "2026-11-09T09:00:00.0000000", "timeZone": "UTC"},
                 "end": {"dateTime": "2026-11-09T10:00:00.0000000", "timeZone": "UTC"}},
                {"status": "free", "start": {"dateTime": "2026-11-09T10:00:00.0000000", "timeZone": "UTC"},
                 "end": {"dateTime": "2026-11-09T11:00:00.0000000", "timeZone": "UTC"}},
                {"status": "tentative", "start": {"dateTime": "2026-11-09T11:00:00.0000000", "timeZone": "UTC"},
                 "end": {"dateTime": "2026-11-09T11:30:00.0000000", "timeZone": "UTC"}},
                {"status": "oof", "start": {"dateTime": "2026-11-10T00:00:00.0000000", "timeZone": "UTC"},
                 "end": {"dateTime": "2026-11-11T00:00:00.0000000", "timeZone": "UTC"}},
                {"status": "workingElsewhere", "start": {"dateTime": "2026-11-11T08:00:00.0000000", "timeZone": "UTC"},
                 "end": {"dateTime": "2026-11-11T16:00:00.0000000", "timeZone": "UTC"}}
            ], "workingHours": {"daysOfWeek": ["monday"], "startTime": "08:00:00.0000000", "endTime": "17:00:00.0000000", "timeZone": {"name": "UTC"}}},
            {"scheduleId": "nobody@example.com", "availabilityView": "",
             "error": {"message": "The specified mailbox couldn't be found.", "responseCode": "ErrorMailRecipientNotFound"}},
            {"scheduleId": "far@example.com", "error": {"message": "Free/busy isn't available.", "responseCode": "ErrorFreeBusyGenerationFailed"}},
            {"scheduleId": "odd@example.com", "error": {"message": "Something else.", "responseCode": "ErrorSomethingNew"}}
        ]});
        let asked: Vec<String> = ["me@example.com", "nobody@example.com", "far@example.com", "odd@example.com", "silent@example.com"].map(String::from).into();
        let got = schedules(&reply, &asked);
        let utc = |d, h, m| Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
        assert_eq!(
            got[0].availability,
            Availability::Known(vec![
                Busy { start: utc(9, 9, 0), end: utc(9, 10, 0), kind: BusyKind::Busy },
                Busy { start: utc(9, 11, 0), end: utc(9, 11, 30), kind: BusyKind::Tentative },
                Busy { start: utc(10, 0, 0), end: utc(11, 0, 0), kind: BusyKind::OutOfOffice },
                Busy { start: utc(11, 8, 0), end: utc(11, 16, 0), kind: BusyKind::Elsewhere },
            ]),
            "free time is dropped"
        );
        assert_eq!(got[1].availability, Availability::Unknown(Unknown::NotFound));
        assert_eq!(got[2].availability, Availability::Unknown(Unknown::NotShared));
        assert_eq!(got[3].availability, Availability::Unknown(Unknown::Failed("Something else.".into())));
        assert!(matches!(got[4].availability, Availability::Unknown(_)), "a schedule Graph left out is never free");
    }

    #[test]
    fn working_hours_from_mailbox_settings() {
        let v = json!({"daysOfWeek": ["monday", "tuesday", "wednesday", "thursday"], "startTime": "08:30:00.0000000",
                       "endTime": "16:00:00.0000000", "timeZone": {"name": "Romance Standard Time"}});
        let h = working_hours(&v, chrono_tz::UTC).unwrap();
        assert_eq!(h.days, Weekdays::of([Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu]));
        assert_eq!((h.start, h.end), (NaiveTime::from_hms_opt(8, 30, 0).unwrap(), NaiveTime::from_hms_opt(16, 0, 0).unwrap()));
        assert_eq!(h.zone, Europe::Paris, "where CLDR places Romance Standard Time");
        let custom = json!({"daysOfWeek": ["friday"], "startTime": "09:00:00.0000000", "endTime": "12:00:00.0000000",
                            "timeZone": {"@odata.type": "#microsoft.graph.customTimeZone", "name": "Customized Time Zone"}});
        assert_eq!(working_hours(&custom, Europe::Brussels).unwrap().zone, Europe::Brussels);
    }
}
