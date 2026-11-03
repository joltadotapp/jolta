//! Online meetings: find the join link, meeting id, passcode and dial-in
//! numbers in an event.

use super::{Meeting, MeetingProvider};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Video,
    Phone,
}

/// A structured conference entry: Graph `onlineMeeting`, Google
/// `conferenceData.entryPoints`, iCalendar `CONFERENCE`,
/// `X-MICROSOFT-SKYPETEAMSMEETINGURL` or `X-GOOGLE-CONFERENCE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub uri: String,
    pub provider: Option<MeetingProvider>,
    pub meeting_id: Option<String>,
    pub passcode: Option<String>,
}

/// Looks at structured entries first, then the event's URL and location,
/// then known meeting links in the description (https only).
pub fn detect(entries: &[Entry], url: Option<&str>, location: &str, description: &str) -> Option<Meeting> {
    let _ = (entries, url, location, description);
    todo!()
}
