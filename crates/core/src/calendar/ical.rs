//! iCalendar (RFC 5545): read events from CalDAV resources, webcal feeds and
//! mail invitations.

use super::EventData;
use crate::Result;

/// Every VEVENT in `text`. `me` lists the user's own addresses, which mark
/// the attendee whose answer is the user's. VTODO and other components are
/// skipped.
pub fn read(text: &str, me: &[String]) -> Result<Vec<EventData>> {
    let _ = (text, me);
    todo!()
}
