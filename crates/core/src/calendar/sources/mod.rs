//! Where calendars come from. Each source lists its calendars and syncs one
//! of them from a cursor it alone understands.

pub mod caldav;
pub mod dav;
pub mod webcal;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::{Calendar, CalendarSource, EventData};
use crate::store::RemoteCalendar;
use crate::{Error, Result};

/// The remote objects a calendar holds, with their ETags, by remote id.
pub type Known = BTreeMap<String, Option<String>>;

/// One remote object that changed: a CalDAV resource, or one UID of a feed.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// Replaces every event stored under `remote_id`.
    Upsert { remote_id: String, etag: Option<String>, events: Vec<EventData>, raw: Option<Vec<u8>> },
    Remove { remote_id: String },
}

/// What one sync brings. Sources compare against `Known` themselves and
/// name what disappeared, so a full resync fetches only what changed.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncBatch {
    pub changes: Vec<Change>,
    /// Where the next sync starts. Saved with the changes, in one transaction.
    pub cursor: Option<String>,
    /// Seconds until the next poll, when the source says.
    pub refresh: Option<i64>,
}

/// Google and Microsoft Graph join as variants once calendar consent is
/// asked for on those accounts.
pub enum Source {
    CalDav(caldav::CalDav),
    Webcal(webcal::Webcal),
}

impl Source {
    pub fn kind(&self) -> CalendarSource {
        match self {
            Source::CalDav(_) => CalendarSource::CalDav,
            Source::Webcal(_) => CalendarSource::Webcal,
        }
    }

    pub async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        match self {
            Source::CalDav(s) => s.list_calendars().await,
            Source::Webcal(s) => s.list_calendars().await,
        }
    }

    pub async fn sync(&self, calendar: &Calendar, known: &Known) -> Result<SyncBatch> {
        match self {
            Source::CalDav(s) => s.sync(calendar, known).await,
            Source::Webcal(s) => s.sync(calendar, known).await,
        }
    }
}

/// Removals for what `known` holds and the server no longer lists.
fn vanished(known: &Known, present: &BTreeSet<String>) -> Vec<Change> {
    known
        .keys()
        .filter(|id| !present.contains(*id))
        .map(|id| Change::Remove { remote_id: id.clone() })
        .collect()
}

fn http_client(follow_redirects: bool) -> Result<reqwest::Client> {
    crate::oauth::init_tls();
    let redirects = if follow_redirects { reqwest::redirect::Policy::limited(8) } else { reqwest::redirect::Policy::none() };
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .redirect(redirects)
        .user_agent(concat!("Jolta/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client: {e}")))
}

fn http_error(e: reqwest::Error) -> Error {
    Error::Other(format!("calendar request failed: {e}"))
}
