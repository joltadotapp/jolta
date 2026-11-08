//! Other people's busy times on Google: `freeBusy.query`, the one call the
//! `calendar.freebusy` scope allows. Google answers for people in the same
//! Workspace and for calendars shared with the user; for anyone else the
//! calendar carries an error instead of busy times. Google gives no working
//! hours, and says only busy, so a person's out of office is read from their
//! calendar's out-of-office events where Google lets the user see them.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use super::{BASE, Google, encode, failure, parse};
use crate::calendar::availability::{Availability, Busy, BusyKind, Schedule, Unknown};
use crate::Result;

/// Calendars per query: `calendarExpansionMax`'s maximum in the
/// [freeBusy reference](https://developers.google.com/workspace/calendar/api/v3/reference/freebusy/query), read 2026-11-05.
const PER_QUERY: usize = 50;

/// Whether `free_busy` also lists each person's out-of-office events. Google's
/// status events guide says free/busy access to a calendar is enough to read
/// them, but the test Workspace has one user, so it stays off until a second
/// user can verify it (C4 U2).
const READS_OUT_OF_OFFICE: bool = false;

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    calendars: std::collections::HashMap<String, Calendar>,
}

#[derive(Deserialize)]
struct Calendar {
    #[serde(default)]
    busy: Vec<Period>,
    #[serde(default)]
    errors: Vec<Problem>,
}

#[derive(Deserialize)]
struct Period {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Problem {
    #[serde(default)]
    reason: String,
}

/// An `events.list` page of out-of-office events. Google refuses all-day
/// ones, so each has a `dateTime`.
#[derive(Deserialize)]
struct AwayList {
    #[serde(default)]
    items: Vec<Away>,
}

#[derive(Deserialize)]
struct Away {
    start: AwayTime,
    end: AwayTime,
}

#[derive(Deserialize)]
struct AwayTime {
    #[serde(rename = "dateTime")]
    date_time: Option<DateTime<Utc>>,
}

impl AwayList {
    fn spans(self) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        self.items.into_iter().filter_map(|a| Some((a.start.date_time?, a.end.date_time?))).collect()
    }
}

impl Google {
    /// Each person's busy times from `from` to `to`, in the order asked.
    pub async fn free_busy(&self, people: &[String], from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Schedule>> {
        let mut out = Vec::with_capacity(people.len());
        for chunk in people.chunks(PER_QUERY) {
            let body = json!({
                "timeMin": from.to_rfc3339(),
                "timeMax": to.to_rfc3339(),
                "items": chunk.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            });
            let url = format!("{BASE}/freeBusy");
            let asked = self.request(reqwest::Method::POST, &url, Some(&body), None);
            let away = futures::future::join_all(chunk.iter().filter(|_| READS_OUT_OF_OFFICE).map(|p| self.out_of_office(p, from, to)));
            let (answer, away) = futures::future::join(asked, away).await;
            let (status, reply) = answer?;
            if !(200..300).contains(&status) {
                return Err(failure(status, &reply));
            }
            out.extend(schedules(parse(&reply)?, chunk, &away));
        }
        Ok(out)
    }

    /// The out-of-office events on `person`'s calendar from `from` to `to`;
    /// none where Google doesn't let the user read them (403 or 404).
    async fn out_of_office(&self, person: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        let url = format!(
            "{BASE}/calendars/{}/events?eventTypes=outOfOffice&singleEvents=true&maxResults=250&timeMin={}&timeMax={}",
            encode(person),
            encode(&from.to_rfc3339()),
            encode(&to.to_rfc3339())
        );
        match self.request(reqwest::Method::GET, &url, None, None).await {
            Ok((200, body)) => parse::<AwayList>(&body).map(AwayList::spans).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

/// Google keys its answer by the id asked for; a person it left out is
/// unknown, never free. `away` holds each person's out-of-office times, in
/// the order asked, when they were read: a busy block over one is out of
/// office.
fn schedules(reply: Reply, people: &[String], away: &[Vec<(DateTime<Utc>, DateTime<Utc>)>]) -> Vec<Schedule> {
    people
        .iter()
        .enumerate()
        .map(|(i, person)| {
            let away = away.get(i).map(Vec::as_slice).unwrap_or_default();
            let kind = |b: &Period| if away.iter().any(|(s, e)| *s < b.end && *e > b.start) { BusyKind::OutOfOffice } else { BusyKind::Busy };
            let found = reply.calendars.iter().find(|(id, _)| id.eq_ignore_ascii_case(person)).map(|(_, c)| c);
            let availability = match found {
                None => Availability::Unknown(Unknown::Failed("Google gave no answer".into())),
                Some(c) => match c.errors.first() {
                    Some(p) if p.reason == "notFound" => Availability::Unknown(Unknown::NotFound),
                    Some(p) => Availability::Unknown(Unknown::Failed(p.reason.clone())),
                    None => Availability::Known(c.busy.iter().map(|b| Busy { start: b.start, end: b.end, kind: kind(b) }).collect()),
                },
            };
            Schedule::new(person.clone(), availability)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    #[test]
    fn busy_times_errors_and_people_left_out() {
        // The shapes the Google Workspace test account returned on 2026-11-05: an
        // address outside the Workspace that shares nothing is notFound.
        let reply: Reply = serde_json::from_value(json!({
            "kind": "calendar#freeBusy",
            "timeMin": "2026-11-09T00:00:00.000Z",
            "timeMax": "2026-11-14T00:00:00.000Z",
            "calendars": {
                "me@example.com": {"busy": [
                    {"start": "2026-11-09T09:00:00Z", "end": "2026-11-09T10:00:00Z"},
                    {"start": "2026-11-10T13:30:00+01:00", "end": "2026-11-10T14:00:00+01:00"}
                ]},
                "Ann@Example.com": {"busy": []},
                "nobody@example.com": {"errors": [{"domain": "global", "reason": "notFound"}], "busy": []},
                "broken@example.com": {"errors": [{"domain": "calendar", "reason": "internalError"}], "busy": []}
            }
        }))
        .unwrap();
        let asked: Vec<String> =
            ["me@example.com", "ann@example.com", "nobody@example.com", "broken@example.com", "silent@example.com"].map(String::from).into();
        let got = schedules(reply, &asked, &[]);
        let utc = |d, h, m| Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
        assert_eq!(
            got[0].availability,
            Availability::Known(vec![
                Busy { start: utc(9, 9, 0), end: utc(9, 10, 0), kind: BusyKind::Busy },
                Busy { start: utc(10, 12, 30), end: utc(10, 13, 0), kind: BusyKind::Busy },
            ])
        );
        assert_eq!(got[1].availability, Availability::Known(vec![]), "matched without regard to case");
        assert_eq!(got[2].availability, Availability::Unknown(Unknown::NotFound));
        assert_eq!(got[3].availability, Availability::Unknown(Unknown::Failed("internalError".into())));
        assert!(matches!(got[4].availability, Availability::Unknown(_)), "a person Google left out is never free");
        assert_eq!(got.iter().map(|s| s.person.as_str()).collect::<Vec<_>>(), asked, "in the order asked");
    }

    #[test]
    fn busy_blocks_over_an_out_of_office_event_are_out_of_office() {
        let reply: Reply = serde_json::from_value(json!({"calendars": {
            "ann@example.com": {"busy": [
                {"start": "2026-11-12T08:00:00Z", "end": "2026-11-12T09:00:00Z"},
                {"start": "2026-11-13T00:00:00Z", "end": "2026-11-14T00:00:00Z"}
            ]},
            "bo@example.com": {"busy": [{"start": "2026-11-13T08:00:00Z", "end": "2026-11-13T09:00:00Z"}]}
        }}))
        .unwrap();
        let away = serde_json::from_value::<AwayList>(json!({"items": [
            {"eventType": "outOfOffice", "start": {"dateTime": "2026-11-13T01:00:00+01:00"}, "end": {"dateTime": "2026-11-14T01:00:00+01:00"}}
        ]}))
        .unwrap()
        .spans();
        let asked: Vec<String> = ["ann@example.com", "bo@example.com"].map(String::from).into();
        let got = schedules(reply, &asked, &[away, Vec::new()]);
        let kinds = |s: &Schedule| match &s.availability {
            Availability::Known(b) => b.iter().map(|b| b.kind).collect::<Vec<_>>(),
            Availability::Unknown(_) => panic!("known"),
        };
        assert_eq!(kinds(&got[0]), [BusyKind::Busy, BusyKind::OutOfOffice]);
        assert_eq!(kinds(&got[1]), [BusyKind::Busy], "Bo's calendar answered no out-of-office events");
    }
}
