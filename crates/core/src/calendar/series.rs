//! A series: the master and the overrides that share one UID in a calendar.
//! Sources deliver them as separate objects, so the occurrences of a series
//! are always worked out from all of its events together.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;

use super::recur::{self, Occurrence, Source, Window};
use super::{Bounds, CalendarId, CalendarSource, EventData, EventStatus, Local, Moment, Response, ShowAs, Span};

/// How far the occurrence table reaches, and the zone floating times and
/// all-day events are placed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Horizon {
    pub window: Window,
    pub here: Tz,
}

impl Horizon {
    /// From 1 year back to 2 years ahead of `now`, in the Mac's zone.
    pub fn around(now: DateTime<Utc>, here: Tz) -> Horizon {
        Horizon {
            window: Window {
                start: now - Duration::days(365),
                end: now + Duration::days(2 * 365),
            },
            here,
        }
    }

    pub fn now() -> Horizon {
        Horizon::around(Utc::now(), super::tz::here())
    }
}

/// The events of one object or feed grouped by UID, in first-seen order.
pub fn group(events: Vec<EventData>) -> Vec<(String, Vec<EventData>)> {
    let mut out: Vec<(String, Vec<EventData>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for e in events {
        match index.get(&e.uid) {
            Some(&i) => out[i].1.push(e),
            None => {
                index.insert(e.uid.clone(), out.len());
                out.push((e.uid.clone(), vec![e]));
            }
        }
    }
    out
}

/// The occurrences inside the horizon, without cancelled ones. Without a
/// master, each override stands alone: an invitation to one occurrence of
/// someone else's series arrives that way.
pub fn place(master: Option<&EventData>, overrides: &[EventData], horizon: &Horizon) -> Vec<Occurrence> {
    match master {
        Some(m) => recur::expand(m, overrides, horizon.window, horizon.here)
            .into_iter()
            .filter(|o| !(o.source == Source::Master && m.status == EventStatus::Cancelled))
            .collect(),
        None => overrides
            .iter()
            .enumerate()
            .filter(|(_, o)| o.status != EventStatus::Cancelled)
            .flat_map(|(i, o)| {
                let alone = EventData { recurrence: None, ..o.clone() };
                recur::expand(&alone, &[], horizon.window, horizon.here)
                    .into_iter()
                    .map(move |occ| Occurrence { source: Source::Override(i), ..occ })
            })
            .collect(),
    }
}

/// The occurrence of a series that an invitation shows: the next one that
/// hasn't ended at `now`, or the last when all have. An all-day occurrence
/// ends at the start of the day after it, here.
pub fn shown_occurrence(master: &EventData, overrides: &[EventData], now: DateTime<Utc>, here: Tz) -> Option<Occurrence> {
    let placed = place(Some(master), overrides, &Horizon::around(now, here));
    let ended = |o: &Occurrence| match o.span.bounds(here) {
        Bounds::Utc { end, .. } => end <= now,
        Bounds::Days { end, .. } => end <= super::tz::day_number(now.with_timezone(&here).date_naive()),
    };
    placed.iter().find(|o| !ended(o)).or(placed.last()).copied()
}

/// Whether the event blocks time, before the calendar's own "counts as busy".
pub fn busy(e: &EventData) -> bool {
    e.show_as != ShowAs::Free && e.status != EventStatus::Cancelled && e.my_response() != Some(Response::Declined)
}

/// Which copy of a meeting found in several calendars Jolta keeps; lower
/// wins. First the copy the user organizes the meeting in, so a meeting Jolta
/// made, whose UID names no provider, opens where the user can change it.
/// Then the organizer's copy by the UID's provider: the other copies are
/// converted invitations, whose titles can be the invitation mail's subject.
/// Then the default calendar, then the calendar added first, so the choice
/// never flips.
pub fn copy_rank(uid: &str, organizer_is_me: bool, source: CalendarSource, is_default: bool, calendar: CalendarId) -> (bool, bool, bool, i64) {
    let origin = if uid.to_ascii_lowercase().ends_with("@google.com") {
        Some(CalendarSource::Google)
    } else if uid.len() >= 32 && uid[..32].eq_ignore_ascii_case(OUTLOOK_ID) {
        Some(CalendarSource::Graph)
    } else {
        None
    };
    (!organizer_is_me, origin != Some(source), !is_default, calendar.0)
}

/// How every Outlook Global Object ID starts ([MS-OXCICAL] 2.1.3.1.1.20.26).
const OUTLOOK_ID: &str = "040000008200E00074C5B7101A82E008";

#[cfg(test)]
mod copy_tests {
    use super::*;

    #[test]
    fn the_organizers_copy_then_the_default_calendar_then_the_oldest() {
        let pick = |uid: &str, copies: &[(CalendarSource, bool, i64)]| {
            copies.iter().min_by_key(|(s, d, c)| copy_rank(uid, false, *s, *d, CalendarId(*c))).unwrap().2
        };
        let both = [(CalendarSource::Graph, false, 1), (CalendarSource::Google, false, 6)];
        assert_eq!(pick("1c7g18fd92ki@google.com", &both), 6, "made in Google Calendar");
        assert_eq!(pick("040000008200E00074C5B7101A82E00800000000ABCDEF", &both), 1, "made in Outlook");
        let unknown = [(CalendarSource::Graph, false, 1), (CalendarSource::Google, true, 6)];
        assert_eq!(pick("5F1A2B3C-ICLOUD", &unknown), 6, "neither: the default calendar");
        assert_eq!(pick("5F1A2B3C-ICLOUD", &both), 1, "then the calendar added first");
    }
}

/// The occurrence `recurrence_id` names as these events of a series have
/// it: its changed occurrence, else the repeating master's moved there.
pub fn occurrence(events: &[EventData], recurrence_id: &Moment, here: Tz) -> Option<EventData> {
    if let Some(changed) = events.iter().find(|e| e.recurrence_id.is_some_and(|r| r.names(recurrence_id, here))) {
        return Some(changed.clone());
    }
    let master = events.iter().find(|e| e.recurrence_id.is_none() && e.recurrence.is_some())?;
    let span = occurrence_span(&master.span, recurrence_id, here)?;
    Some(EventData { recurrence_id: Some(*recurrence_id), recurrence: None, span, ..master.clone() })
}

/// The span of the unchanged occurrence `recurrence_id` names in a series
/// whose master spans `master`: the master's, moved to start there on the
/// master's own clock. None when one is all-day and the other isn't.
pub fn occurrence_span(master: &Span, recurrence_id: &Moment, here: Tz) -> Option<Span> {
    match (master, recurrence_id) {
        (Span::Timed { start, end }, Moment::Time(at)) => {
            let wall = super::tz::wall(at.instant(here), start.zone, here);
            let shift = wall - start.wall;
            Some(Span::Timed { start: Local { wall, zone: start.zone }, end: Local { wall: end.wall + shift, zone: end.zone } })
        }
        (Span::AllDay { start, end }, Moment::Date(day)) => Some(Span::AllDay { start: *day, end: *day + (*end - *start) }),
        _ => None,
    }
}
