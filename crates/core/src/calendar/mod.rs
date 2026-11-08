//! Calendars: sync from Graph, Google, CalDAV and webcal feeds, time zones,
//! repeating events, invitations and meeting links.

pub mod add;
pub mod availability;
pub mod away;
pub mod caps;
pub mod card;
pub mod change;
pub mod description;
pub mod detail;
pub mod draft;
pub mod filing;
pub mod freebusy;
pub mod ical;
pub mod ical_edit;
pub mod invitation;
pub mod invite;
pub mod meeting;
pub mod merge;
pub mod moving;
mod model;
pub mod ops;
pub mod organizer;
pub mod emailing;
pub mod proposal;
pub mod quick;
pub mod recur;
pub mod remind;
pub mod remove;
pub mod repeat;
pub mod respond;
pub mod rooms;
pub mod rsvp;
pub mod scheduler;
pub mod series;
pub mod split;
pub mod sources;
pub mod sync;
pub mod synth;
pub mod tz;
pub mod update;

pub use model::*;
