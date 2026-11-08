//! Other people's busy times and working hours on Microsoft 365
//! (`getSchedule`), and the user's working hours
//! (`mailboxSettings.workingHours`).

use chrono::{DateTime, NaiveTime, Utc, Weekday};
use serde_json::{Value, json};

use super::{Api, GraphCalendar, When, instant};
use crate::calendar::availability::{Availability, Busy, BusyKind, Schedule, Unknown, Weekdays, WorkingHours};
use crate::calendar::tz;
use crate::Result;

/// Schedules per request: "up to 20 entities at once", Microsoft Graph's
/// free/busy guide (outlook-get-free-busy-schedule).
const PER_REQUEST: usize = 20;

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
        Ok(working_hours(&reply, Some(Api::zone(self))))
    }
}

/// Graph answers in the order asked, each schedule with its `scheduleId`
/// and the person's `workingHours`.
fn schedules(reply: &Value, people: &[String]) -> Vec<Schedule> {
    let answers = reply["value"].as_array().map(Vec::as_slice).unwrap_or_default();
    people
        .iter()
        .map(|person| {
            let found = answers.iter().find(|s| s["scheduleId"].as_str().is_some_and(|id| id.eq_ignore_ascii_case(person)));
            let (availability, hours) = match found {
                None => (Availability::Unknown(Unknown::Failed("Microsoft gave no answer".into())), None),
                Some(s) if s["error"].is_object() => (Availability::Unknown(unknown(&s["error"])), None),
                Some(s) if no_data(s) => (Availability::Unknown(Unknown::Failed("Microsoft 365 has no free/busy for them".into())), None),
                Some(s) => (Availability::Known(s["scheduleItems"].as_array().into_iter().flatten().filter_map(busy).collect()), working_hours(&s["workingHours"], None)),
            };
            Schedule { person: person.clone(), availability, hours }
        })
        .collect()
}

/// An item of status `unknown`, or a merged view of only 4, Exchange's
/// "no data" (MergedFreeBusy), says nothing about when the person is free.
fn no_data(schedule: &Value) -> bool {
    let view = schedule["availabilityView"].as_str().unwrap_or_default();
    let items = schedule["scheduleItems"].as_array().map(Vec::as_slice).unwrap_or_default();
    (!view.is_empty() && view.bytes().all(|b| b == b'4')) || items.iter().any(|i| i["status"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("unknown")))
}

fn unknown(error: &Value) -> Unknown {
    let code = error["responseCode"].as_str().unwrap_or_default();
    match code {
        // 5009: "Unable to resolve e-mail address … to an Active Directory
        // object", for an address outside the organization (2026-11-05).
        "5009" | "ErrorMailRecipientNotFound" | "ErrorInvalidSmtpAddress" => Unknown::NotFound,
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

/// `here` stands in for a zone Graph names in a way Jolta can't map; without
/// it such hours are none. Hours that end before they start, or have no day,
/// are none too.
fn working_hours(v: &Value, here: Option<chrono_tz::Tz>) -> Option<WorkingHours> {
    let time = |k: &str| v[k].as_str().and_then(|t| NaiveTime::parse_from_str(t.get(..8)?, "%H:%M:%S").ok());
    let days = Weekdays::of(v["daysOfWeek"].as_array()?.iter().filter_map(|d| d.as_str()?.parse::<Weekday>().ok()));
    let zone = v["timeZone"]["name"].as_str().and_then(tz::zone).or(here)?;
    let (start, end) = (time("startTime")?, time("endTime")?);
    (start < end && days.0 != 0).then_some(WorkingHours { days, start, end, zone })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe;

    use super::*;

    #[test]
    fn schedule_items_by_status_and_errors_per_schedule() {
        // The shapes the Microsoft 365 test account returned on 2026-11-05.
        let reply = json!({"value": [
            {"scheduleId": "me@example.com", "availabilityView": "0022", "scheduleItems": [
                {"isPrivate": false, "isMeeting": false, "isRecurring": false, "status": "busy", "subject": "Planning", "location": "",
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
            {"scheduleId": "outside@example.org", "error": {"message": "Unable to resolve e-mail address outside@example.org to an Active Directory object.", "responseCode": "5009"}},
            {"scheduleId": "far@example.com", "error": {"message": "Free/busy isn't available.", "responseCode": "ErrorFreeBusyGenerationFailed"}},
            {"scheduleId": "odd@example.com", "error": {"message": "Something else.", "responseCode": "ErrorSomethingNew"}}
        ]});
        let asked: Vec<String> =
            ["me@example.com", "nobody@example.com", "outside@example.org", "far@example.com", "odd@example.com", "silent@example.com"].map(String::from).into();
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
        assert_eq!(got[2].availability, Availability::Unknown(Unknown::NotFound), "an address outside the organization, as Microsoft 365 answered it");
        assert_eq!(got[3].availability, Availability::Unknown(Unknown::NotShared));
        assert_eq!(got[4].availability, Availability::Unknown(Unknown::Failed("Something else.".into())));
        assert!(matches!(got[5].availability, Availability::Unknown(_)), "a schedule Graph left out is never free");
    }

    #[test]
    fn an_unknown_item_or_a_view_of_no_data_is_never_free() {
        let item = |status: &str| json!({"status": status, "start": {"dateTime": "2026-11-09T09:00:00.0000000", "timeZone": "UTC"},
                                          "end": {"dateTime": "2026-11-09T10:00:00.0000000", "timeZone": "UTC"}});
        let reply = json!({"value": [
            {"scheduleId": "half@example.com", "availabilityView": "0420", "scheduleItems": [item("busy"), item("unknown")]},
            {"scheduleId": "blank@example.com", "availabilityView": "4444", "scheduleItems": []},
            {"scheduleId": "free@example.com", "availabilityView": "0000", "scheduleItems": []}
        ]});
        let asked: Vec<String> = ["half@example.com", "blank@example.com", "free@example.com"].map(String::from).into();
        let got = schedules(&reply, &asked);
        assert!(matches!(got[0].availability, Availability::Unknown(_)), "an item of unknown status leaves the person unknown");
        assert!(matches!(got[1].availability, Availability::Unknown(_)), "4 in availabilityView is Exchange's no data");
        assert_eq!(got[2].availability, Availability::Known(Vec::new()));
    }

    #[test]
    fn schedules_go_twenty_a_request() {
        // "getSchedule can support looking up free/busy information for up to
        // 20 entities at once", Microsoft Graph's free/busy guide.
        assert_eq!(PER_REQUEST, 20);
    }

    #[test]
    fn working_hours_from_mailbox_settings() {
        let v = json!({"daysOfWeek": ["monday", "tuesday", "wednesday", "thursday"], "startTime": "08:30:00.0000000",
                       "endTime": "16:00:00.0000000", "timeZone": {"name": "Romance Standard Time"}});
        let h = working_hours(&v, Some(chrono_tz::UTC)).unwrap();
        assert_eq!(h.days, Weekdays::of([Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu]));
        assert_eq!((h.start, h.end), (NaiveTime::from_hms_opt(8, 30, 0).unwrap(), NaiveTime::from_hms_opt(16, 0, 0).unwrap()));
        assert_eq!(h.zone, Europe::Paris, "where CLDR places Romance Standard Time");
        let custom = json!({"daysOfWeek": ["friday"], "startTime": "09:00:00.0000000", "endTime": "12:00:00.0000000",
                            "timeZone": {"@odata.type": "#microsoft.graph.customTimeZone", "name": "Customized Time Zone"}});
        assert_eq!(working_hours(&custom, Some(Europe::Brussels)).unwrap().zone, Europe::Brussels);
        assert_eq!(working_hours(&custom, None), None, "someone else's hours on a clock Jolta can't place constrain nothing");
        let overnight = json!({"daysOfWeek": ["friday"], "startTime": "22:00:00.0000000", "endTime": "06:00:00.0000000", "timeZone": {"name": "UTC"}});
        assert_eq!(working_hours(&overnight, None), None);
    }

    #[test]
    fn each_schedule_carries_the_persons_working_hours() {
        let reply = json!({"value": [
            {"scheduleId": "ann@example.com", "availabilityView": "00", "scheduleItems": [],
             "workingHours": {"daysOfWeek": ["monday", "tuesday", "wednesday", "thursday", "friday"], "startTime": "09:00:00.0000000",
                              "endTime": "15:00:00.0000000", "timeZone": {"name": "Pacific Standard Time"}}},
            {"scheduleId": "bo@example.com", "availabilityView": "00", "scheduleItems": []},
            {"scheduleId": "outside@example.org", "error": {"message": "Unable to resolve e-mail address.", "responseCode": "5009"}}
        ]});
        let asked: Vec<String> = ["ann@example.com", "bo@example.com", "outside@example.org"].map(String::from).into();
        let got = schedules(&reply, &asked);
        let ann = got[0].hours.expect("Ann's hours");
        assert_eq!(ann.zone, chrono_tz::America::Los_Angeles);
        assert_eq!((ann.start, ann.end, ann.days), (NaiveTime::from_hms_opt(9, 0, 0).unwrap(), NaiveTime::from_hms_opt(15, 0, 0).unwrap(), Weekdays::MONDAY_TO_FRIDAY));
        assert_eq!(got[1].hours, None, "a schedule without workingHours");
        assert_eq!((&got[2].availability, got[2].hours), (&Availability::Unknown(Unknown::NotFound), None));
    }
}
