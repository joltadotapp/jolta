//! Calendars: sync from Graph, Google, CalDAV and webcal feeds, time zones,
//! repeating events, invitations and meeting links. Plan: docs/calendar-plan.md.

pub mod ical;
pub mod meeting;
mod model;
pub mod recur;
pub mod tz;

pub use model::*;
