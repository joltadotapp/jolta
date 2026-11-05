//! Where calendars come from. Each source lists its calendars and syncs one
//! of them from a cursor it alone understands.

pub mod caldav;
pub mod dav;
pub mod google;
pub mod graph;
pub mod webcal;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::rsvp::{Rsvp, Stored, Target};
use super::{Calendar, CalendarSource, EventData};
use crate::store::RemoteCalendar;
use crate::{Account, AccountKind, Error, Result};

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

pub enum Source {
    CalDav(caldav::CalDav),
    Webcal(webcal::Webcal),
    Google(google::Google),
    Graph(graph::GraphCalendar),
}

impl Source {
    pub fn kind(&self) -> CalendarSource {
        match self {
            Source::CalDav(_) => CalendarSource::CalDav,
            Source::Webcal(_) => CalendarSource::Webcal,
            Source::Google(_) => CalendarSource::Google,
            Source::Graph(_) => CalendarSource::Graph,
        }
    }

    pub async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        match self {
            Source::CalDav(s) => s.list_calendars().await,
            Source::Webcal(s) => s.list_calendars().await,
            Source::Google(s) => s.list_calendars().await,
            Source::Graph(s) => s.list_calendars().await,
        }
    }

    pub async fn sync(&self, calendar: &Calendar, known: &Known) -> Result<SyncBatch> {
        match self {
            Source::CalDav(s) => s.sync(calendar, known).await,
            Source::Webcal(s) => s.sync(calendar, known).await,
            Source::Google(s) => s.sync(calendar, known).await,
            Source::Graph(s) => s.sync(calendar, known).await,
        }
    }

    /// Sends an answer to an invitation. Returns the remote id of the
    /// object it landed on: the event, or the occurrence answered alone.
    /// The remote id the answer landed on, and for CalDAV the resource as
    /// written, read with every address of the user, to store as a sync would.
    pub async fn answer(&self, calendar: &Calendar, target: &Target, rsvp: &Rsvp) -> Result<(String, Option<Change>)> {
        match self {
            Source::CalDav(s) => s.answer(calendar, target, rsvp).await.map(|c| match &c {
                Change::Upsert { remote_id, .. } => (remote_id.clone(), Some(c)),
                Change::Remove { remote_id } => (remote_id.clone(), None),
            }),
            Source::Google(s) => s.answer(calendar, target, rsvp).await.map(|r| (r, None)),
            Source::Graph(s) => s.answer(target, rsvp).await.map(|r| (r, None)),
            Source::Webcal(_) => Err(Error::Other("This event is in a subscribed calendar, which is read-only, so it can't be answered.".into())),
        }
    }

    /// The user's addresses the server knows besides the account's own: a
    /// CalDAV principal's calendar-user-address-set.
    pub async fn addresses(&self) -> Vec<String> {
        match self {
            Source::CalDav(s) => s.me().await,
            Source::Google(_) | Source::Graph(_) | Source::Webcal(_) => Vec::new(),
        }
    }

    /// Takes a meeting its organizer cancelled out of the calendar,
    /// telling nobody: the whole event, or the occurrence `target` names.
    pub async fn remove(&self, calendar: &Calendar, target: &Target) -> Result<Gone> {
        match self {
            Source::CalDav(s) => s.remove(calendar, target).await,
            Source::Google(s) => s.remove(calendar, target).await,
            Source::Graph(s) => s.remove(target).await,
            Source::Webcal(_) => Err(Error::Other("This event is in a subscribed calendar, which is read-only.".into())),
        }
    }
}

impl Source {
    /// Puts a copy of an invitation in the calendar, telling nobody, and
    /// returns the remote id of the object created. CalDAV also gives the
    /// resource as written, to store as a sync would.
    /// The bool is false when the copy lacks part of the version: Graph
    /// refused a changed occurrence because the series changed meanwhile.
    pub async fn create(&self, calendar: &Calendar, copy: &NewCopy<'_>) -> Result<(String, Option<Change>, bool)> {
        match self {
            Source::CalDav(s) => s.create(calendar, copy).await.map(|c| match &c {
                Change::Upsert { remote_id, .. } => (remote_id.clone(), Some(c), true),
                Change::Remove { remote_id } => (remote_id.clone(), None, true),
            }),
            Source::Google(s) => s.create(calendar, copy).await.map(|id| (id, None, true)),
            Source::Graph(s) => s.create(calendar, copy).await.map(|(id, complete)| (id, None, complete)),
            Source::Webcal(_) => Err(Error::Other("A subscribed calendar is read-only.".into())),
        }
    }

    /// Writes a newer version of the invitation over the copy `target`
    /// holds, telling nobody, only while the copy is the version Jolta read:
    /// its whole series, also the occurrences it holds unchanged. `held` are
    /// the changed occurrences Jolta holds of it, which Google keeps as
    /// events of their own.
    pub async fn replace(&self, calendar: &Calendar, target: &Target, held: &[Stored], copy: &NewCopy<'_>) -> Result<Replaced> {
        match self {
            Source::CalDav(s) => s.replace(calendar, target, copy).await,
            Source::Google(s) => s.replace(calendar, target, held, copy).await,
            Source::Graph(s) => s.replace(target, copy).await,
            Source::Webcal(_) => Err(Error::Other("A subscribed calendar is read-only.".into())),
        }
    }

    /// Gives a copy the user added the answer they sent by mail, telling
    /// nobody. Graph's copy is the user's own event without guests, so the
    /// answer shows as its free, tentative or busy time.
    pub async fn follow(&self, calendar: &Calendar, target: &Target, rsvp: &Rsvp) -> Result<Option<Change>> {
        let quiet = Rsvp { notify: false, note: None, ..rsvp.clone() };
        match self {
            Source::Graph(s) => s.follow(target, rsvp.answer).await.map(|_| None),
            _ => self.answer(calendar, target, &quiet).await.map(|(_, change)| change),
        }
    }
}

/// An invitation as Add to calendar or Update in your calendar writes it.
pub struct NewCopy<'a> {
    pub uid: &'a str,
    /// The invitation's calendar part as sent.
    pub text: &'a str,
    /// Its events for `uid`: the series or single event first when the
    /// message has it, then changed occurrences.
    pub events: &'a [EventData],
    /// The address the invitation went to, whose attendee entry is the
    /// user's, and the answer it gets when the user gave one that holds.
    pub me: &'a str,
    pub answer: Option<super::rsvp::Answer>,
}

impl NewCopy<'_> {
    pub fn master(&self) -> Option<&EventData> {
        self.events.iter().find(|e| e.recurrence_id.is_none())
    }

    pub fn overrides(&self) -> impl Iterator<Item = &EventData> {
        self.events.iter().filter(|e| e.recurrence_id.is_some())
    }
}

/// What writing a newer version over an added copy did.
#[derive(Clone, Debug, PartialEq)]
pub enum Replaced {
    /// Written. CalDAV gives the resource as written, to store.
    Written(Option<Change>),
    /// The copy changed in the calendar since Jolta read it, so Jolta left it.
    Changed,
}

/// What removing a cancelled meeting did on the server.
#[derive(Clone, Debug, PartialEq)]
pub enum Gone {
    /// The server had already removed it or marked it cancelled, so Jolta
    /// sent nothing.
    Already,
    /// Jolta deleted it. A CalDAV occurrence leaves through a new version
    /// of its series, given here to store.
    Deleted(Option<Change>),
    /// The copy changed on the server since Jolta read it, so Jolta left it.
    Changed,
}

/// One request with an account's calendar sign-in, for `jolta dev http`:
/// the OAuth token for Google and Microsoft accounts, which goes only to
/// their HTTPS APIs, the password for CalDAV, which goes only to the
/// servers discovery would start from. Any other address is refused.
/// Returns the status, the ETag and the body.
pub async fn signed_request(
    account: &Account,
    caldav_url: Option<&str>,
    method: &str,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, Option<String>, String)> {
    let json = || -> Result<Option<serde_json::Value>> {
        (!body.trim().is_empty())
            .then(|| serde_json::from_str(body).map_err(|e| Error::Other(format!("the body isn't JSON: {e}"))))
            .transpose()
    };
    match account.kind {
        AccountKind::Graph => {
            let (status, reply) = graph::GraphCalendar::new(account)?.request(method, url, json()?.as_ref(), headers).await?;
            Ok((status, reply["@odata.etag"].as_str().map(str::to_string), reply.to_string()))
        }
        AccountKind::Gmail => {
            let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| Error::Other(format!("HTTP method {method}: {e}")))?;
            let if_match = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("If-Match")).map(|(_, v)| *v);
            let (status, reply) = google::Google::new(account)?.request(method, url, json()?.as_ref(), if_match).await?;
            Ok((status, None, String::from_utf8_lossy(&reply).into_owned()))
        }
        AccountKind::Imap => {
            let url = reqwest::Url::parse(url).map_err(|e| Error::Other(format!("{url}: {e}")))?;
            let caldav = caldav::CalDav::new(account, caldav_url)?;
            if !caldav.trusted(&url) {
                return Err(Error::Other(format!("Jolta sends this account's password only to its own CalDAV server, not to {url}.")));
            }
            let reply = caldav.exchange(method, &url, headers, body).await?;
            Ok((reply.status, reply.etag, reply.body))
        }
    }
}

/// `url` when it is HTTPS on `host` itself, the only place the account's
/// OAuth token may go. Checked before the token is fetched.
fn provider_url(url: &str, host: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Error::Other(format!("{url}: {e}")))?;
    if parsed.scheme() == "https" && parsed.host_str() == Some(host) && parsed.port().is_none() {
        return Ok(parsed);
    }
    Err(Error::Other(format!("Jolta sends this account's sign-in only to https://{host}, not to {url}.")))
}

/// The event's description, with its meeting's join link on a line of its
/// own when the description doesn't hold it, for a copy whose provider
/// keeps no other place for it.
fn with_join_link(e: &EventData) -> String {
    match &e.meeting {
        Some(m) if !e.description.contains(&m.join_url) => {
            if e.description.trim().is_empty() {
                m.join_url.clone()
            } else if e.description_html {
                format!("<p><a href=\"{0}\">{0}</a></p>{1}", m.join_url.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;"), e.description)
            } else {
                format!("{}\n\n{}", m.join_url, e.description)
            }
        }
        _ => e.description.clone(),
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

/// Percent-encodes everything but unreserved characters, for an id in a
/// path or a token in a query. Google calendar ids hold `#` and `@`.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_goes_only_to_its_providers_https_api() {
        let ok = |url: &str, host: &str| provider_url(url, host).map(|u| u.to_string());
        assert_eq!(ok("https://graph.microsoft.com/v1.0/me/events", "graph.microsoft.com").unwrap(), "https://graph.microsoft.com/v1.0/me/events");
        assert!(ok("https://graph.microsoft.com:443/v1.0/me", "graph.microsoft.com").is_ok(), "the default port written out");
        assert!(ok("https://www.googleapis.com/calendar/v3/users/me/calendarList", "www.googleapis.com").is_ok());
        for refused in [
            "https://attacker.example/collect",
            "http://graph.microsoft.com/v1.0/me",
            "https://graph.microsoft.com.attacker.example/v1.0/me",
            "https://graph.microsoft.com@attacker.example/v1.0/me",
            "https://graph.microsoft.com:8443/v1.0/me",
            "https://www.googleapis.com/calendar/v3/users/me",
            "/me/events",
        ] {
            assert!(ok(refused, "graph.microsoft.com").is_err(), "{refused}");
        }
    }
}
