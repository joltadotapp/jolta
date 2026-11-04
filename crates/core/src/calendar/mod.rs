//! Calendars: sync from Graph, Google, CalDAV and webcal feeds, time zones,
//! repeating events, invitations and meeting links.

pub mod card;
pub mod detail;
pub mod ical;
pub mod ical_edit;
pub mod invitation;
pub mod meeting;
mod model;
pub mod recur;
pub mod remind;
pub mod respond;
pub mod rsvp;
pub mod scheduler;
pub mod series;
pub mod sources;
pub mod sync;
pub mod synth;
pub mod tz;
pub mod update;

pub use model::*;
