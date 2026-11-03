//! Calendars: sync from Graph, Google, CalDAV and webcal feeds, time zones,
//! repeating events, invitations and meeting links. Plan: docs/calendar-plan.md.

pub mod detail;
pub mod ical;
pub mod ical_edit;
pub mod meeting;
mod model;
pub mod recur;
pub mod remind;
pub mod scheduler;
pub mod series;
pub mod sources;
pub mod sync;
pub mod synth;
pub mod tz;

pub use model::*;
