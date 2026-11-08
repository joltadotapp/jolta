//! Where calendars come from. Each source lists its calendars and syncs one
//! of them from a cursor it alone understands.

pub mod caldav;
pub mod dav;
pub mod google;
pub mod graph;
pub mod webcal;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::availability::{Availability, Schedule, Unknown, WorkingHours};
use super::change::{FIELDS, Field};
use super::ops::{Extra, Notify};
use super::rsvp::{Rsvp, Stored, Target};
use super::{Calendar, CalendarSource, EventData};
use crate::store::{RemoteCalendar, Removal};
use crate::{Account, AccountKind, Error, Result};

/// The remote objects a calendar holds, with their ETags, by remote id.
pub type Known = BTreeMap<String, Option<String>>;

/// One remote object that changed: a CalDAV resource, or one UID of a feed.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// Replaces every event stored under `remote_id`.
    Upsert { remote_id: String, etag: Option<String>, events: Vec<EventData>, raw: Option<Vec<u8>> },
    Remove { remote_id: String, removal: Removal },
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
                Change::Remove { remote_id, .. } => (remote_id.clone(), None),
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

    /// Each person's busy times from `from` to `to`, in the order asked.
    /// CalDAV's scheduling outbox (RFC 6638) isn't asked yet, so there
    /// everyone is unknown.
    pub async fn free_busy(&self, people: &[String], from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Schedule>> {
        match self {
            Source::Google(s) => s.free_busy(people, from, to).await,
            Source::Graph(s) => s.free_busy(people, from, to).await,
            Source::CalDav(_) | Source::Webcal(_) => Ok(people
                .iter()
                .map(|p| Schedule { person: p.clone(), availability: Availability::Unknown(Unknown::NotSupported) })
                .collect()),
        }
    }

    /// The user's working hours as the provider keeps them: only Microsoft
    /// 365 says.
    pub async fn working_hours(&self) -> Result<Option<WorkingHours>> {
        match self {
            Source::Graph(s) => s.working_hours().await,
            Source::Google(_) | Source::CalDav(_) | Source::Webcal(_) => Ok(None),
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
                Change::Remove { remote_id, .. } => (remote_id.clone(), None, true),
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

impl Source {
    /// Sends one calendar op's write. Every update and delete is guarded by
    /// the version Jolta holds; a create lands on the object `key` names, so
    /// a retry after a lost answer finds the first try's event. `tell` says
    /// whom the server mails about it.
    pub async fn write(&self, calendar: &Calendar, write: &OpWrite<'_>, tell: Tell<'_>) -> Result<Written> {
        match self {
            Source::CalDav(s) => s.write(calendar, write).await,
            Source::Google(s) => s.write(calendar, write, tell).await,
            Source::Graph(s) => s.write(calendar, write, tell).await,
            Source::Webcal(_) => Err(Error::Other("A subscribed calendar is read-only.".into())),
        }
    }

    /// The object `remote_id` as the server holds it now, None when it is
    /// gone. `uid` and `alarms` are the stored event's, for what the
    /// server's answer leaves out.
    /// The occurrence `rid` of the series `series` as the server holds it
    /// now: Google's and Microsoft 365's instance, changed or not, or the
    /// CalDAV resource the series and its changed occurrences share. None
    /// when it is gone.
    pub async fn fetch_occurrence(&self, calendar: &Calendar, series: &Stored, rid: &super::Moment, alarms: &[super::Alarm]) -> Result<Option<Version>> {
        match self {
            Source::CalDav(s) => s.fetch(calendar, &series.remote_id).await,
            Source::Google(s) => s.fetch_occurrence(calendar, &series.remote_id, rid, alarms).await,
            Source::Graph(s) => s.fetch_occurrence(series, &series.data.uid, rid).await,
            Source::Webcal(_) => Err(Error::Other("A subscribed calendar is read-only.".into())),
        }
    }

    pub async fn fetch(&self, calendar: &Calendar, remote_id: &str, uid: &str, alarms: &[super::Alarm]) -> Result<Option<Version>> {
        match self {
            Source::CalDav(s) => s.fetch(calendar, remote_id).await,
            Source::Google(s) => s.fetch(calendar, remote_id, alarms).await,
            Source::Graph(s) => s.fetch(remote_id, uid).await,
            Source::Webcal(_) => Err(Error::Other("A subscribed calendar is read-only.".into())),
        }
    }
}

/// Whether the provider can write `write` as it stands: the request it
/// would send can be built. Checked when the op is queued, so an edit the
/// provider can't hold is refused at once instead of failing five times.
pub fn writable(source: CalendarSource, write: &OpWrite<'_>) -> Result<()> {
    let here = super::tz::here();
    match source {
        CalendarSource::Google => google::writable(write, here),
        CalendarSource::Graph => graph::writable(write, here),
        CalendarSource::CalDav => caldav::writable(write),
        CalendarSource::Local => Ok(()),
        CalendarSource::Webcal | CalendarSource::EventKit => Err(Error::Other("This calendar is read-only.".into())),
    }
}

/// Whom the server mails about one write of a calendar op, and the user's
/// words with a cancellation. A CalDAV server with scheduling mails the
/// guests of every change itself, so only Google and Microsoft 365 read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tell<'a> {
    pub notify: Notify,
    pub message: Option<&'a str>,
}

impl Tell<'_> {
    pub const NOBODY: Tell<'static> = Tell { notify: Notify::Nobody, message: None };
}

/// One write of a calendar op.
#[derive(Clone, Copy, Debug)]
pub enum OpWrite<'a> {
    /// A new event, or a series with its changed occurrences, at `key`:
    /// Google's event id, Graph's transactionId, the CalDAV resource path.
    Create { events: &'a [EventData], key: &'a str, extra: Option<&'a Extra> },
    /// `event` over `held`, the version Jolta holds, only while the server
    /// still has that version. `base` is the event the user started from:
    /// Google and Graph get only the fields that changed from it, CalDAV
    /// gets them written into the held resource. `dropped` are the series'
    /// changed occurrences its new time or rule no longer holds: Google and
    /// Microsoft 365 drop them themselves, a CalDAV resource loses them.
    Update { held: &'a Stored, base: &'a EventData, event: &'a EventData, extra: Option<&'a Extra>, dropped: &'a [EventData] },
    /// One occurrence of the series `series` that has no object of its own
    /// yet: Google and Microsoft 365 change it as an instance of the series,
    /// CalDAV adds its VEVENT to the series' resource. Guarded by the
    /// series' version, which its instances share.
    Occurrence { series: &'a Stored, base: &'a EventData, event: &'a EventData, extra: Option<&'a Extra> },
    /// The whole object `held`, only while the server still has that version.
    Delete { held: &'a Stored },
    /// One occurrence taken out of its series: `held` is its own object
    /// when it was `changed`, else the series'.
    Cancel { held: &'a Stored, occurrence: &'a EventData, changed: bool },
    /// The whole object `held` moved to `to`, another calendar of the same
    /// account, only while the server still has that version, telling
    /// nobody: Google `events.move`, CalDAV MOVE. A retry whose first try
    /// moved it finds it in `to`.
    Move { held: &'a Stored, to: &'a Calendar },
}

/// An object as the server holds it, to store as a sync would.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub remote_id: String,
    pub etag: Option<String>,
    pub events: Vec<EventData>,
    pub raw: Option<Vec<u8>>,
    /// After a series was created with its changed occurrences, the
    /// instances written for them, each an object of its own on Google and
    /// Microsoft 365.
    pub instances: Vec<Version>,
}

impl From<Version> for Change {
    fn from(v: Version) -> Change {
        Change::Upsert { remote_id: v.remote_id, etag: v.etag, events: v.events, raw: v.raw }
    }
}

/// What the server made of one write.
#[derive(Clone, Debug, PartialEq)]
pub enum Written {
    /// Done: the object as it is now, none after a delete.
    Saved(Option<Version>),
    /// The object changed since the version Jolta holds (412), or Jolta
    /// holds no version to guard the write with, so nothing was written.
    Stale,
    /// The object is gone (404, 410).
    Gone,
}

/// The provider fields an edit passes through as a JSON object, merged into
/// `body` without replacing anything Jolta set there.
fn merge_json(body: &mut serde_json::Value, extra: Option<&Extra>, provider: &str) -> Result<()> {
    match extra {
        None => Ok(()),
        Some(Extra::Json(fields)) => {
            let object = body.as_object_mut().expect("a request body is an object");
            for (k, v) in fields {
                object.entry(k.clone()).or_insert_with(|| v.clone());
            }
            Ok(())
        }
        Some(Extra::Lines(_)) => Err(Error::Other(format!("{provider} takes extra fields as JSON, not iCalendar lines"))),
    }
}

/// The fields a new event sets: those that differ from a blank event,
/// and its time.
fn created_fields(e: &EventData) -> Vec<Field> {
    let blank = EventData::new(e.uid.clone(), e.span);
    std::iter::once(Field::Time).chain(FIELDS.into_iter().filter(|f| f.differs(&blank, e))).collect()
}

/// The fields `e` changed from `base`.
fn changed_fields(base: &EventData, e: &EventData) -> Vec<Field> {
    FIELDS.into_iter().filter(|f| f.differs(base, e)).collect()
}

/// Whether the instance `current` already has every field the changed
/// occurrence `changed` of the new series `master` sets: a create retried
/// after the first try wrote it.
fn already_written(master: &EventData, changed: &EventData, current: &EventData, here: chrono_tz::Tz) -> bool {
    let Some(rid) = changed.recurrence_id.as_ref() else { return false };
    let Some(base) = crate::calendar::series::occurrence(std::slice::from_ref(master), rid, here) else { return false };
    // Google and Graph hold a room as a resource and read it back as one.
    let resource = |e: &EventData| EventData {
        attendees: e.attendees.iter().map(|a| super::Attendee { role: if a.role == super::Role::Room { super::Role::Resource } else { a.role }, ..a.clone() }).collect(),
        ..e.clone()
    };
    let (current, changed) = (resource(current), resource(changed));
    changed_fields(&base, &changed).into_iter().all(|f| !f.differs(&current, &changed))
}

/// The error for a change the provider can't hold.
fn cannot(provider: &str, what: &str) -> Error {
    Error::Other(format!("{provider} can't {what}"))
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
        .map(|id| Change::Remove { remote_id: id.clone(), removal: Removal::Unlisted })
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
    let text = format!("calendar request failed: {e}");
    if e.is_connect() { Error::Offline(text) } else { Error::Other(text) }
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

    /// Check 4: a create retried after its first try wrote a changed
    /// occurrence of the new series leaves that occurrence alone, as each
    /// write mails the guests again.
    #[test]
    fn a_retried_create_knows_the_occurrences_it_wrote() {
        use chrono_tz::Europe::Brussels;
        use crate::calendar::{Local, Moment, Recurrence, Span, Zone};
        let at = |s: &str| Local { wall: chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) };
        let master = EventData {
            title: "Standup".into(),
            recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=4".into()], ..Recurrence::default() }),
            ..EventData::new("u1", Span::Timed { start: at("2026-12-01 09:00"), end: at("2026-12-01 09:30") })
        };
        let rid = Moment::Time(at("2026-12-08 09:00"));
        let plain = crate::calendar::series::occurrence(std::slice::from_ref(&master), &rid, Brussels).unwrap();
        let moved = EventData { span: Span::Timed { start: at("2026-12-08 11:00"), end: at("2026-12-08 11:30") }, ..plain.clone() };
        assert!(!already_written(&master, &moved, &plain, Brussels), "the first try writes it");
        let as_written = EventData { description: "Graph's own body".into(), ..moved.clone() };
        assert!(already_written(&master, &moved, &as_written, Brussels), "a retry finds it written and leaves it");
        let room = |role| crate::calendar::Attendee {
            person: crate::calendar::Person { address: "room@example.com".into(), name: String::new() },
            role,
            response: crate::calendar::Response::NeedsAction,
            comment: String::new(),
            is_me: false,
        };
        let booked = EventData { attendees: vec![room(crate::calendar::Role::Room)], ..moved.clone() };
        let read_back = EventData { attendees: vec![room(crate::calendar::Role::Resource)], ..moved };
        assert!(already_written(&master, &booked, &read_back, Brussels), "a room comes back as a resource");
    }

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
