//! A synthetic calendar for timing, the size the plan's budgets name: 5
//! calendars, 20,000 events of which 300 repeat, spread over the occurrence
//! window. Seeded, so every run builds the same calendar.

use chrono::{DateTime, Datelike, Duration, NaiveTime, Utc, Weekday};
use chrono_tz::Europe::Brussels;

use super::series::Horizon;
use super::{
    Access, Attendee, CalendarId, CalendarSource, EventData, Local, Meeting, MeetingProvider, Moment, Person, Recurrence,
    Response, Role, ShowAs, Span, Zone,
};
use crate::store::{self, RemoteCalendar};
use crate::synth::{Rng, word};
use crate::{Jolta, Result};

pub const CALENDARS: usize = 5;
pub const EVENTS: usize = 20_000;
pub const SERIES: usize = 300;
const REMOTE_PREFIX: &str = "synthetic-calendar-";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Built {
    pub events: usize,
    pub overrides: usize,
    pub occurrences: i64,
}

/// Replaces the synthetic calendars with freshly generated ones around `now`.
pub fn generate(jolta: &Jolta, now: DateTime<Utc>) -> Result<Built> {
    let horizon = Horizon::around(now, Brussels);
    let mut rng = Rng(0x00CA_1E4D);
    jolta.store.write(|tx| {
        tx.execute("DELETE FROM calendar WHERE source = 'local' AND remote_id LIKE ?1", [format!("{REMOTE_PREFIX}%")])?;
        let calendars: Vec<CalendarId> = (0..CALENDARS)
            .map(|i| {
                let remote = RemoteCalendar {
                    remote_id: format!("{REMOTE_PREFIX}{i}"),
                    name: format!("Synthetic {}", i + 1),
                    color: Some(["#1badf8", "#63da38", "#ff9500", "#cc73e1", "#ff2968"][i].into()),
                    zone: Some("Europe/Brussels".into()),
                    access: Access::Owner,
                    primary: true,
                    refresh: None,
                };
                Ok(store::add_calendar(tx, CalendarSource::Local, &remote)?.id)
            })
            .collect::<Result<_>>()?;
        let mut built = Built::default();
        let first_day = horizon.window.start.date_naive();
        let days = (horizon.window.end - horizon.window.start).num_days();
        for i in 0..EVENTS {
            let calendar = calendars[rng.below(CALENDARS)];
            let uid = format!("synthetic-{i}@example.com");
            let mut events = vec![if i < SERIES { series(&mut rng, &uid, now) } else { single(&mut rng, &uid, first_day, days) }];
            if i < SERIES / 3 {
                events.extend(overrides(&mut rng, &events[0], &horizon));
                built.overrides += events.len() - 1;
            }
            store::apply_item(tx, calendar, &uid, Some("1"), &events, None, &horizon)?;
            built.events += 1;
        }
        built.occurrences = tx.query_row(
            "SELECT count(*) FROM occurrence WHERE calendar_id IN (SELECT id FROM calendar WHERE remote_id LIKE ?1)",
            [format!("{REMOTE_PREFIX}%")],
            |r| r.get(0),
        )?;
        Ok(built)
    })
}

fn title(rng: &mut Rng) -> String {
    let n = 1 + rng.below(4);
    let words: Vec<String> = (0..n).map(|_| word(rng.below(2_000))).collect();
    let mut t = words.join(" ");
    if let Some(first) = t.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    t
}

fn at(day: chrono::NaiveDate, minutes: i64) -> Local {
    Local { wall: day.and_time(NaiveTime::MIN) + Duration::minutes(minutes), zone: Zone::Tz(Brussels) }
}

/// Details most meetings have: people, a call link, sometimes "free".
fn dress(rng: &mut Rng, mut e: EventData) -> EventData {
    e.title = title(rng);
    if rng.chance(0.3) {
        e.location = format!("Room {}", word(rng.below(50)));
    }
    if rng.chance(0.5) {
        e.attendees = (0..2 + rng.below(6))
            .map(|k| Attendee {
                person: Person { address: format!("person{k}@example.com"), name: String::new() },
                role: Role::Required,
                response: Response::Accepted,
                comment: String::new(),
                is_me: k == 0,
            })
            .collect();
    }
    if rng.chance(0.3) {
        let provider = [MeetingProvider::Teams, MeetingProvider::Zoom, MeetingProvider::Meet][rng.below(3)];
        e.meeting = Some(Meeting {
            provider,
            join_url: format!("https://meet.example.com/{}", rng.next() % 1_000_000_000),
            meeting_id: None,
            passcode: None,
            phones: Vec::new(),
            other_links: Vec::new(),
        });
    }
    if rng.chance(0.05) {
        e.show_as = ShowAs::Free;
    }
    e
}

/// Mostly 30 to 120 minutes in working hours, some all-day, a few long.
fn single(rng: &mut Rng, uid: &str, first_day: chrono::NaiveDate, days: i64) -> EventData {
    let day = first_day + Duration::days(rng.below(days as usize) as i64);
    let span = match rng.below(100) {
        0..10 => Span::AllDay { start: day, end: day + Duration::days(1 + rng.below(3) as i64) },
        10 => {
            let start = at(day, 9 * 60);
            Span::Timed { start, end: Local { wall: start.wall + Duration::days(1 + rng.below(13) as i64), ..start } }
        }
        _ => {
            let start = at(day, 8 * 60 + 15 * rng.below(40) as i64);
            Span::Timed { start, end: Local { wall: start.wall + Duration::minutes(30 * (1 + rng.below(4) as i64)), ..start } }
        }
    };
    dress(rng, EventData::new(uid, span))
}

/// Weekly meetings, weekday standups, monthly reviews and yearly birthdays,
/// started in the past year, some ending.
fn series(rng: &mut Rng, uid: &str, now: DateTime<Utc>) -> EventData {
    let mut day = now.date_naive() - Duration::days(rng.below(365) as i64);
    while matches!(day.weekday(), Weekday::Sat | Weekday::Sun) {
        day += Duration::days(1);
    }
    let (rule, span) = match rng.below(100) {
        0..55 => ("FREQ=WEEKLY".to_string(), 60),
        55..70 => ("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".to_string(), 15),
        70..80 => ("FREQ=WEEKLY;INTERVAL=2".to_string(), 45),
        80..92 => (format!("FREQ=MONTHLY;BYMONTHDAY={}", day.day().min(28)), 90),
        _ => ("FREQ=YEARLY".to_string(), 0),
    };
    let rule = if rng.chance(0.3) { format!("{rule};COUNT={}", 5 + rng.below(60)) } else { rule };
    let span = if span == 0 {
        Span::AllDay { start: day, end: day + Duration::days(1) }
    } else {
        let start = at(day, 8 * 60 + 30 * rng.below(18) as i64);
        Span::Timed { start, end: Local { wall: start.wall + Duration::minutes(span), ..start } }
    };
    let mut e = dress(rng, EventData::new(uid, span));
    e.recurrence = Some(Recurrence { rules: vec![rule], ..Recurrence::default() });
    e
}

/// Two occurrences of the series moved by an hour, as people do.
fn overrides(rng: &mut Rng, master: &EventData, horizon: &Horizon) -> Vec<EventData> {
    let occurrences = super::series::place(Some(master), &[], horizon);
    (0..2)
        .filter_map(|_| {
            let occ = occurrences.get(rng.below(occurrences.len().max(1)))?;
            let Span::Timed { start, end } = occ.span else { return None };
            let shift = Duration::hours(1);
            Some(EventData {
                recurrence_id: Some(Moment::Time(start)),
                recurrence: None,
                span: Span::Timed {
                    start: Local { wall: start.wall + shift, ..start },
                    end: Local { wall: end.wall + shift, ..end },
                },
                sequence: 1,
                ..master.clone()
            })
        })
        .collect()
}
