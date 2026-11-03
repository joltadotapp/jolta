//! Repeating events: expand a series into the occurrences inside a window.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use super::{EventData, Span};

/// Each series stops here, so a `FREQ=MINUTELY` rule from a stranger's
/// invitation can't fill the database.
pub const SERIES_CAP: usize = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Master,
    /// Index into the `overrides` passed to `expand`.
    Override(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub source: Source,
    pub span: Span,
}

/// The occurrences of `master` that overlap `window`, ordered by start. A
/// single event yields at most one. Rules, RDATEs and EXDATEs run on the
/// master's wall clock in its own zone; each override replaces the
/// occurrence its recurrence id names, and a cancelled override removes it.
/// Overrides moved into the window from outside it count, as do overrides
/// whose recurrence id matches no occurrence. At most `SERIES_CAP`.
pub fn expand(master: &EventData, overrides: &[EventData], window: Window, here: Tz) -> Vec<Occurrence> {
    let _ = (master, overrides, window, here);
    todo!()
}
