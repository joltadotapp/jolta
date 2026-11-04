//! What the event detail shows for one occurrence.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params};

use super::{Attendee, Bounds, Calendar, EventData, EventId, Person, Response, Role, Span, Zone, meeting, recur};
use crate::{Error, Result, render, store};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum When {
    /// Both days included.
    AllDay { first: NaiveDate, last: NaiveDate },
    Timed {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        /// The event's own zone, when its clock differs from here at the start.
        zone: Option<Tz>,
        /// "10:00 to 11:00 New York (16:00 to 17:00 here)", or "16:00 to 17:00".
        text: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    pub event_id: EventId,
    pub calendar: Calendar,
    pub event: EventData,
    pub when: When,
    /// The series' rule in words, "Custom repeat" when it has none.
    pub repeat: Option<String>,
    /// A full HTML document, or empty when there is no description.
    pub description_html: String,
    /// People by response, in the order `RESPONSE_ORDER` gives. Rooms and
    /// resources are in `rooms` instead.
    pub attendees: Vec<(Response, Vec<Attendee>)>,
    pub rooms: Vec<Person>,
    /// "4 accepted, 1 maybe, 2 no answer". Empty without attendees.
    pub responses: String,
    pub app_url: Option<String>,
}

const RESPONSE_ORDER: [(Response, &str); 5] = [
    (Response::Accepted, "accepted"),
    (Response::Tentative, "maybe"),
    (Response::NeedsAction, "no answer"),
    (Response::Declined, "declined"),
    (Response::Delegated, "delegated"),
];

/// The detail of `event_id` at the occurrence `at`, or at the event's own
/// time without one. `here` is the zone the views follow.
pub fn open(conn: &Connection, event_id: EventId, at: Option<Bounds>, here: Tz, app_css: &str) -> Result<Detail> {
    let (calendar_id, uid, is_override, data): (super::CalendarId, String, bool, String) = conn
        .query_row(
            "SELECT calendar_id, uid, recurrence_id <> '', data FROM event WHERE id = ?1",
            [event_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("event {event_id}")))?;
    let event = parse(event_id, &data)?;
    let calendar = store::calendar(conn, calendar_id)?;
    let master = if is_override {
        let data: Option<(EventId, String)> = conn
            .query_row(
                "SELECT id, data FROM event WHERE calendar_id = ?1 AND uid = ?2 AND recurrence_id = '' ORDER BY id LIMIT 1",
                params![calendar_id, uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        data.map(|(id, d)| parse(id, &d)).transpose()?
    } else {
        None
    };
    let series = master.as_ref().unwrap_or(&event);
    let repeat = series
        .recurrence
        .as_ref()
        .map(|r| recur::describe(r, &series.span).unwrap_or_else(|| "Custom repeat".into()));

    let mut attendees: Vec<(Response, Vec<Attendee>)> = RESPONSE_ORDER.iter().map(|(r, _)| (*r, Vec::new())).collect();
    let mut rooms = Vec::new();
    for a in &event.attendees {
        if matches!(a.role, Role::Room | Role::Resource) {
            rooms.push(a.person.clone());
        } else if let Some((_, group)) = attendees.iter_mut().find(|(r, _)| *r == a.response) {
            group.push(a.clone());
        }
    }
    let responses = attendees
        .iter()
        .zip(RESPONSE_ORDER)
        .filter(|((_, people), _)| !people.is_empty())
        .map(|((_, people), (_, word))| format!("{} {word}", people.len()))
        .collect::<Vec<_>>()
        .join(", ");
    attendees.retain(|(_, people)| !people.is_empty());

    Ok(Detail {
        event_id,
        when: when(&event.span, at.unwrap_or_else(|| event.span.bounds(here)), here),
        repeat,
        description_html: render::render_description(&event.description, event.description_html, app_css),
        attendees,
        rooms,
        responses,
        app_url: event.meeting.as_ref().and_then(meeting::app_url),
        calendar,
        event,
    })
}

fn parse(id: EventId, data: &str) -> Result<EventData> {
    serde_json::from_str(data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))
}

/// The occurrence's times. A timed event set in a zone whose clock differs
/// from here says both.
fn when(span: &Span, at: Bounds, here: Tz) -> When {
    match at {
        Bounds::Days { start, end } => {
            let day = |n: i32| NaiveDate::default() + Duration::days(n.into());
            When::AllDay { first: day(start), last: day((end - 1).max(start)) }
        }
        Bounds::Utc { start, end } => {
            let own = match span {
                Span::Timed { start: own_start, .. } => match own_start.zone {
                    Zone::Tz(tz) if offset(tz, start) != offset(here, start) => Some(tz),
                    _ => None,
                },
                Span::AllDay { .. } => None,
            };
            let wall = |tz: Tz| (start.with_timezone(&tz).naive_local(), end.with_timezone(&tz).naive_local());
            let (here_start, here_end) = wall(here);
            let text = match own {
                Some(tz) => {
                    let (s, e) = wall(tz);
                    format!("{} {} ({} here)", range(s, e), city(tz), range(here_start, here_end))
                }
                None => range(here_start, here_end),
            };
            When::Timed { start, end, zone: own, text }
        }
    }
}

fn offset(tz: Tz, at: DateTime<Utc>) -> i32 {
    tz.offset_from_utc_datetime(&at.naive_utc()).fix().local_minus_utc()
}

/// "10:00 to 11:00", with days when it ends on another day than it starts.
fn range(start: NaiveDateTime, end: NaiveDateTime) -> String {
    let next_midnight = (start.date() + Duration::days(1)).and_time(chrono::NaiveTime::MIN);
    if end.date() == start.date() || end == next_midnight {
        format!("{} to {}", start.format("%H:%M"), end.format("%H:%M"))
    } else {
        format!("{} to {}", start.format("%a %-d %b %H:%M"), end.format("%a %-d %b %H:%M"))
    }
}

/// "America/New_York" as "New York".
fn city(tz: Tz) -> String {
    tz.name().rsplit('/').next().unwrap_or(tz.name()).replace('_', " ")
}

#[cfg(test)]
mod tests {
    use chrono_tz::{America::New_York, Europe::Brussels, Europe::Paris};

    use super::*;
    use crate::calendar::Local;

    fn timed(zone: Tz, start: &str, minutes: i64) -> Span {
        let wall = NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M").unwrap();
        let start = Local { wall, zone: Zone::Tz(zone) };
        Span::Timed { start, end: Local { wall: wall + Duration::minutes(minutes), ..start } }
    }

    fn text(span: Span) -> String {
        match when(&span, span.bounds(Brussels), Brussels) {
            When::Timed { text, .. } => text,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn times_in_the_events_zone_and_here() {
        assert_eq!(text(timed(New_York, "2026-11-09 10:00", 60)), "10:00 to 11:00 New York (16:00 to 17:00 here)");
        assert_eq!(text(timed(Paris, "2026-11-09 10:00", 60)), "10:00 to 11:00", "the same clock needs no second time");
        assert_eq!(text(timed(Brussels, "2026-11-09 23:00", 60)), "23:00 to 00:00");
        assert_eq!(text(timed(Brussels, "2026-11-09 23:00", 120)), "Mon 9 Nov 23:00 to Tue 10 Nov 01:00");
        let trip = Span::AllDay {
            start: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            end: NaiveDate::from_ymd_opt(2026, 11, 10).unwrap(),
        };
        assert_eq!(
            when(&trip, trip.bounds(New_York), Brussels),
            When::AllDay { first: NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(), last: NaiveDate::from_ymd_opt(2026, 11, 9).unwrap() }
        );
    }
}
