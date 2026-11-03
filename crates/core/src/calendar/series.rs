//! A series: the master and the overrides that share one UID in a calendar.
//! Sources deliver them as separate objects, so the occurrences of a series
//! are always worked out from all of its events together.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;

use super::recur::{self, Occurrence, Source, Window};
use super::{EventData, EventStatus, Response, ShowAs};

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
    for e in events {
        match out.iter_mut().find(|(uid, _)| *uid == e.uid) {
            Some((_, list)) => list.push(e),
            None => out.push((e.uid.clone(), vec![e])),
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

/// Whether the event blocks time, before the calendar's own "counts as busy".
pub fn busy(e: &EventData) -> bool {
    e.show_as != ShowAs::Free && e.status != EventStatus::Cancelled && e.my_response() != Some(Response::Declined)
}
