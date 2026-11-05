//! Other people's busy times on Google: `freeBusy.query`, the one call the
//! `calendar.freebusy` scope allows. Google answers for people in the same
//! Workspace and for calendars shared with the user; for anyone else the
//! calendar carries an error instead of busy times.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use super::{BASE, Google, failure, parse};
use crate::calendar::availability::{Availability, Busy, BusyKind, Schedule, Unknown};
use crate::Result;

/// Calendars per query: `calendarExpansionMax`'s maximum in the
/// [freeBusy reference](https://developers.google.com/workspace/calendar/api/v3/reference/freebusy/query), read 2026-11-05.
const PER_QUERY: usize = 50;

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
            let (status, reply) = self.request(reqwest::Method::POST, &format!("{BASE}/freeBusy"), Some(&body), None).await?;
            if !(200..300).contains(&status) {
                return Err(failure(status, &reply));
            }
            out.extend(schedules(parse(&reply)?, chunk));
        }
        Ok(out)
    }
}

/// Google keys its answer by the id asked for; a person it left out is
/// unknown, never free.
fn schedules(reply: Reply, people: &[String]) -> Vec<Schedule> {
    people
        .iter()
        .map(|person| {
            let found = reply.calendars.iter().find(|(id, _)| id.eq_ignore_ascii_case(person)).map(|(_, c)| c);
            let availability = match found {
                None => Availability::Unknown(Unknown::Failed("Google gave no answer".into())),
                Some(c) => match c.errors.first() {
                    Some(p) if p.reason == "notFound" => Availability::Unknown(Unknown::NotFound),
                    Some(p) => Availability::Unknown(Unknown::Failed(p.reason.clone())),
                    None => Availability::Known(c.busy.iter().map(|b| Busy { start: b.start, end: b.end, kind: BusyKind::Busy }).collect()),
                },
            };
            Schedule { person: person.clone(), availability }
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
        let got = schedules(reply, &asked);
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
}
