//! CalDAV (RFC 4791) for iCloud, Fastmail and other servers: discovery
//! (RFC 6764), the calendar list, and sync through sync-collection
//! (RFC 6578), with the collection's ctag and the resources' ETags for
//! servers without it. The one write is a PUT that answers an invitation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use quick_xml::escape::escape;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::dav::{self, APPLE, CALDAV, CALENDARSERVER, DAV, MultiStatus, href_key};
use super::{Change, Gone, Known, SyncBatch, http_client, http_error, vanished};
use crate::calendar::remove;
use crate::calendar::rsvp::{self, Rsvp, Target};
use crate::calendar::{Access, Calendar, EventData, ical};
use crate::store::RemoteCalendar;
use crate::{Account, Error, Result, secrets};

/// Resources per calendar-multiget, so one reply stays a few hundred KB.
const MULTIGET_BATCH: usize = 50;

const PRINCIPAL: &str = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
const HOME: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/><c:schedule-inbox-URL/></d:prop></d:propfind>"#;
const DEFAULT_CALENDAR: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:schedule-default-calendar-URL/></d:prop></d:propfind>"#;
const CALENDARS: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:a="http://apple.com/ns/ical/"><d:prop><d:resourcetype/><d:displayname/><a:calendar-color/><c:supported-calendar-component-set/><d:current-user-privilege-set/><d:owner/><cs:getctag/><d:sync-token/></d:prop></d:propfind>"#;
const CTAG: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:getctag/></d:prop></d:propfind>"#;
const ADDRESSES: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-user-address-set/></d:prop></d:propfind>"#;
const ETAGS: &str = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getetag/></d:prop></d:propfind>"#;

pub struct CalDav {
    http: reqwest::Client,
    username: String,
    password: String,
    /// Where discovery starts, in order.
    starts: Vec<Url>,
    /// The account's address.
    address: String,
    fastmail: bool,
    /// The start is a guess from the address's domain, not a known server.
    guessed: bool,
}

/// Where the next sync starts: the sync token, or the collection's ctag on
/// servers without sync-collection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    token: Option<String>,
    ctag: Option<String>,
}

/// The resources of a collection and their ETags, as one listing gave them.
struct Listing {
    members: Vec<(String, Option<String>)>,
    gone: Vec<String>,
    /// Every resource is listed, so known ones missing from it are gone.
    complete: bool,
    cursor: Cursor,
}

impl Listing {
    /// Adds one sync-collection reply. True when the server truncated it.
    fn absorb(&mut self, ms: &MultiStatus, collection: &str) -> bool {
        let mut truncated = false;
        for r in &ms.responses {
            if href_key(&r.href) == collection {
                truncated |= r.status == Some(507);
            } else if r.status == Some(404) {
                self.gone.push(r.href.clone());
            } else if !r.href.ends_with('/') {
                self.members.push((r.href.clone(), r.prop_text(DAV, "getetag").map(str::to_string)));
            }
        }
        truncated
    }
}

enum Collection {
    Listed(Listing),
    /// The server no longer accepts the token: start again without one.
    TokenRefused,
    Unsupported,
}

pub(super) struct Reply {
    pub(super) status: u16,
    /// Where the answer came from after redirects; hrefs resolve against it.
    url: Url,
    pub(super) body: String,
    pub(super) etag: Option<String>,
}

impl CalDav {
    /// `override_url` is the address typed under Advanced. Without it iCloud
    /// and Fastmail use their known servers and other providers RFC 6764
    /// discovery on the address's domain.
    pub fn new(account: &Account, override_url: Option<&str>) -> Result<CalDav> {
        let host = account.imap_host.to_ascii_lowercase();
        let domain = account.address.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()).unwrap_or_default();
        let fastmail = host.ends_with("fastmail.com");
        let guessed = override_url.is_none() && !host.ends_with("mail.me.com") && !fastmail;
        let starts = match override_url {
            Some(url) => vec![url.to_string()],
            None if host.ends_with("mail.me.com") => vec!["https://caldav.icloud.com/".into()],
            None if fastmail => vec!["https://caldav.fastmail.com/".into()],
            None if !domain.is_empty() => vec![format!("https://{domain}/")],
            None => return Err(no_server(&account.address)),
        };
        let starts = starts
            .iter()
            .map(|s| Url::parse(s).map_err(|e| Error::Other(format!("CalDAV address {s}: {e}"))))
            .collect::<Result<_>>()?;
        Ok(CalDav {
            http: http_client(false)?,
            username: account.username.clone(),
            password: secrets::password(&account.secret_ref)?,
            starts,
            address: account.address.clone(),
            fastmail,
            guessed,
        })
    }

    pub(super) fn trusted(&self, url: &Url) -> bool {
        trusts(&self.starts, url)
    }

    /// A WebDAV request with an XML body.
    async fn send(&self, method: &str, url: &Url, depth: &str, body: &str) -> Result<Reply> {
        self.exchange(method, url, &[("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")], body).await
    }

    /// One request, following redirects with the same method and body.
    pub(super) async fn exchange(&self, method: &str, url: &Url, headers: &[(&str, &str)], body: &str) -> Result<Reply> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("a valid method");
        let mut url = url.clone();
        let mut attempts = 0;
        for _ in 0..10 {
            let mut req = self.http.request(method.clone(), url.clone()).body(body.to_string());
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            if self.trusted(&url) {
                req = req.basic_auth(&self.username, Some(&self.password));
            }
            let resp = req.send().await.map_err(http_error)?;
            let status = resp.status().as_u16();
            let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
            if (300..400).contains(&status) && status != 304 {
                let location = header("Location").ok_or_else(|| Error::Other(format!("CalDAV {url}: redirect without an address")))?;
                url = url.join(&location).map_err(|e| Error::Other(format!("CalDAV redirect to {location}: {e}")))?;
                continue;
            }
            if matches!(status, 429 | 503) && attempts < 3 {
                attempts += 1;
                let wait = header("Retry-After").and_then(|v| v.parse::<u64>().ok()).unwrap_or(2 << attempts);
                tokio::time::sleep(Duration::from_secs(wait.min(30))).await;
                continue;
            }
            if status == 401 {
                return Err(Error::Auth(self.refused(&url)));
            }
            let etag = header("ETag");
            let body = resp.text().await.map_err(http_error)?;
            return Ok(Reply { status, url, body, etag });
        }
        Err(Error::Other(format!("CalDAV {url}: too many redirects")))
    }

    fn refused(&self, url: &Url) -> String {
        let host = url.host_str().unwrap_or_default();
        match (self.trusted(url), self.fastmail) {
            (false, _) => format!("{host} asked for a password Jolta only sends to the account's own server. Enter its CalDAV address under Advanced."),
            (true, true) => "Fastmail refused the password for calendars. An app password made for mail only doesn't cover them; make one that includes CalDAV.".into(),
            (true, false) => format!("{host} refused the account's password for calendars."),
        }
    }

    /// PROPFIND that must answer 207.
    async fn propfind(&self, url: &Url, depth: &str, body: &str) -> Result<(Url, MultiStatus)> {
        let reply = self.send("PROPFIND", url, depth, body).await?;
        if reply.status != 207 {
            return Err(Error::Other(format!("CalDAV PROPFIND {}: {}", reply.url, reply.status)));
        }
        Ok((reply.url, dav::multistatus(&reply.body)?))
    }

    /// The principal URL: from each start, its well-known address first
    /// (RFC 6764), then the start itself. DNS SRV lookups are left out: they
    /// need a DNS library Jolta doesn't have, and every server tried so far
    /// answers the well-known address.
    async fn principal(&self) -> Result<Url> {
        let mut last = None;
        for start in &self.starts {
            let mut tries = vec![start.clone()];
            if start.path() == "/" {
                tries.insert(0, start.join("/.well-known/caldav").expect("a fixed path joins"));
            }
            for url in tries {
                match self.propfind(&url, "0", PRINCIPAL).await {
                    Ok((at, ms)) => {
                        if let Some(href) = ms.responses.iter().find_map(|r| r.prop_href(DAV, "current-user-principal")) {
                            return join(&at, href);
                        }
                    }
                    Err(e @ Error::Auth(_)) => return Err(e),
                    Err(e) => last = Some(e),
                }
            }
        }
        Err(match last {
            Some(e) if !self.guessed => e,
            _ => no_server(&self.address),
        })
    }

    pub async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        let principal = self.principal().await?;
        let (at, ms) = self.propfind(&principal, "0", HOME).await?;
        let first = ms.responses.first().ok_or_else(|| Error::Other("CalDAV: the principal has no properties".into()))?;
        let home = first.prop_href(CALDAV, "calendar-home-set").ok_or_else(|| Error::Other("CalDAV: no calendar home".into()))?;
        let home = join(&at, home)?;
        let inbox = first.prop_href(CALDAV, "schedule-inbox-URL").map(|h| join(&at, h)).transpose()?;
        let default = match inbox {
            Some(inbox) => match self.propfind(&inbox, "0", DEFAULT_CALENDAR).await {
                Ok((at, ms)) => ms
                    .responses
                    .first()
                    .and_then(|r| r.prop_href(CALDAV, "schedule-default-calendar-URL"))
                    .and_then(|h| join(&at, h).ok()),
                Err(_) => None,
            },
            None => None,
        };
        let (at, ms) = self.propfind(&home, "1", CALENDARS).await?;
        calendars(&at, &ms, default.as_ref(), &principal)
    }

    pub async fn sync(&self, calendar: &Calendar, known: &Known) -> Result<SyncBatch> {
        let cursor: Cursor = calendar.cursor.as_deref().and_then(|c| serde_json::from_str(c).ok()).unwrap_or_default();
        let url = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        let listing = match self.sync_collection(&url, cursor.token.as_deref()).await? {
            Collection::Listed(listing) => listing,
            Collection::TokenRefused => match self.sync_collection(&url, None).await? {
                Collection::Listed(listing) => listing,
                _ => self.etag_listing(&url, &cursor).await?,
            },
            Collection::Unsupported => self.etag_listing(&url, &cursor).await?,
        };

        let changed: Vec<String> = listing
            .members
            .iter()
            .filter(|(href, etag)| etag.is_none() || known.get(&href_key(href)) != Some(etag))
            .map(|(href, _)| href.clone())
            .collect();
        let mut changes = self.multiget(&url, &changed).await?;
        changes.extend(listing.gone.iter().map(|href| Change::Remove { remote_id: href_key(href) }));
        if listing.complete {
            let present: BTreeSet<String> = listing.members.iter().map(|(href, _)| href_key(href)).collect();
            changes.extend(vanished(known, &present));
        }
        Ok(SyncBatch {
            changes,
            cursor: Some(serde_json::to_string(&listing.cursor).expect("a cursor serializes")),
            refresh: None,
        })
    }

    /// RFC 6578: what changed since `token`, or everything without one. A
    /// truncated answer (507 on the collection) continues from its token.
    async fn sync_collection(&self, url: &Url, token: Option<&str>) -> Result<Collection> {
        let collection = href_key(url.path());
        let mut listing = Listing { members: Vec::new(), gone: Vec::new(), complete: token.is_none(), cursor: Cursor::default() };
        let mut next = token.map(str::to_string);
        for _ in 0..100 {
            let body = format!(
                r#"<d:sync-collection xmlns:d="DAV:"><d:sync-token>{}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>"#,
                escape(next.as_deref().unwrap_or_default())
            );
            let reply = self.send("REPORT", url, "0", &body).await?;
            match reply.status {
                207 => {}
                400 | 403 | 404 | 405 | 409 | 412 | 415 | 422 | 501 if token.is_some() => return Ok(Collection::TokenRefused),
                400 | 403 | 404 | 405 | 409 | 412 | 415 | 422 | 501 => return Ok(Collection::Unsupported),
                status => return Err(Error::Other(format!("CalDAV sync of {url}: {status}"))),
            }
            let ms = dav::multistatus(&reply.body)?;
            let truncated = listing.absorb(&ms, &collection);
            let Some(token) = ms.sync_token else {
                return Ok(Collection::Unsupported);
            };
            next = Some(token);
            if !truncated {
                listing.cursor.token = next;
                return Ok(Collection::Listed(listing));
            }
        }
        Err(Error::Other(format!("CalDAV sync of {url}: the server kept truncating")))
    }

    /// For servers without sync-collection: nothing when the ctag is
    /// unchanged, else every resource with its ETag.
    async fn etag_listing(&self, url: &Url, cursor: &Cursor) -> Result<Listing> {
        let (_, ms) = self.propfind(url, "0", CTAG).await?;
        let ctag = ms.responses.first().and_then(|r| r.prop_text(CALENDARSERVER, "getctag")).map(str::to_string);
        let next = Cursor { token: None, ctag: ctag.clone() };
        if ctag.is_some() && ctag == cursor.ctag {
            return Ok(Listing { members: Vec::new(), gone: Vec::new(), complete: false, cursor: next });
        }
        let (_, ms) = self.propfind(url, "1", ETAGS).await?;
        let collection = href_key(url.path());
        let members = ms
            .responses
            .iter()
            .filter(|r| href_key(&r.href) != collection && !is_collection(r))
            .map(|r| (r.href.clone(), r.prop_text(DAV, "getetag").map(str::to_string)))
            .collect();
        Ok(Listing { members, gone: Vec::new(), complete: true, cursor: next })
    }

    /// The user's own addresses, to find them on invitations: the
    /// account's, and the principal's calendar-user-address-set (RFC 6638),
    /// which holds its aliases. Asked for once per run; without an answer
    /// the account's address stands alone.
    pub(crate) async fn me(&self) -> Vec<String> {
        let key = format!("{} {}", self.username, self.starts[0]);
        if let Some(known) = KNOWN_ADDRESSES.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return known.clone();
        }
        let found = async {
            let principal = self.principal().await?;
            let (_, ms) = self.propfind(&principal, "0", ADDRESSES).await?;
            Ok::<_, Error>(mail_addresses(&ms))
        };
        match found.await {
            Ok(found) => {
                let me = with_aliases(&self.address, found);
                KNOWN_ADDRESSES.lock().unwrap_or_else(|e| e.into_inner()).insert(key, me.clone());
                me
            }
            Err(e) => {
                tracing::warn!("the CalDAV addresses of {}: {e}", self.address);
                vec![self.address.clone()]
            }
        }
    }

    /// The changed resources' iCalendar data, in batches.
    async fn multiget(&self, url: &Url, hrefs: &[String]) -> Result<Vec<Change>> {
        let mut out = Vec::with_capacity(hrefs.len());
        let me = if hrefs.is_empty() { Vec::new() } else { self.me().await };
        for chunk in hrefs.chunks(MULTIGET_BATCH) {
            let hrefs: String = chunk.iter().map(|h| format!("<d:href>{}</d:href>", escape(h.as_str()))).collect();
            let body = format!(
                r#"<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop>{hrefs}</c:calendar-multiget>"#
            );
            let reply = self.send("REPORT", url, "1", &body).await?;
            if reply.status != 207 {
                return Err(Error::Other(format!("CalDAV multiget on {url}: {}", reply.status)));
            }
            for r in dav::multistatus(&reply.body)?.responses {
                let remote_id = href_key(&r.href);
                if r.status == Some(404) {
                    out.push(Change::Remove { remote_id });
                    continue;
                }
                // Skipping it would let the new sync token pass it by for good.
                let Some(data) = r.prop(CALDAV, "calendar-data").map(|p| p.text.as_str()) else {
                    return Err(Error::Other(format!("CalDAV multiget on {url}: no data for {remote_id} ({:?})", r.status)));
                };
                match ical::read(data, &me) {
                    Ok(events) => out.push(Change::Upsert {
                        remote_id,
                        etag: r.prop_text(DAV, "getetag").map(str::to_string),
                        events,
                        raw: Some(data.as_bytes().to_vec()),
                    }),
                    Err(e) => tracing::warn!("skipping {remote_id}: {e}"),
                }
            }
        }
        Ok(out)
    }

    /// Writes the user's PARTSTAT into the resource and PUTs it back with
    /// `If-Match`; the server's scheduling (RFC 6638) sends the organizer the
    /// reply. The stored copy is edited first; when the server has a newer
    /// one (412), that one is fetched and edited instead.
    pub async fn answer(&self, calendar: &Calendar, target: &Target, rsvp: &Rsvp) -> Result<Change> {
        let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        let url = join(&collection, &path_escape(&target.event.remote_id))?;
        let rid = target.recurrence_id();
        let here = crate::calendar::tz::here();
        let me = self.me().await;
        let mut current = cached(&target.event);
        for attempt in 0..2 {
            let (text, etag) = match current.take() {
                Some(stored) => stored,
                None => {
                    let reply = self.exchange("GET", &url, &[], "").await?;
                    if reply.status != 200 {
                        return Err(Error::Other(format!("CalDAV GET {url}: {}", reply.status)));
                    }
                    (reply.body, reply.etag)
                }
            };
            let edited = rsvp::answer_in_ical(&text, &target.event.data.uid, rid.as_ref(), &me, rsvp, here)?;
            let mut headers = vec![("Content-Type", "text/calendar; charset=utf-8")];
            if let Some(etag) = &etag {
                headers.push(("If-Match", etag));
            }
            let reply = self.exchange("PUT", &url, &headers, &edited).await?;
            match reply.status {
                200..300 => {
                    return Ok(Change::Upsert {
                        remote_id: target.event.remote_id.clone(),
                        etag: reply.etag,
                        events: ical::read(&edited, &me)?,
                        raw: Some(edited.into_bytes()),
                    });
                }
                412 if attempt == 0 => continue,
                status => return Err(Error::Other(format!("CalDAV PUT {url}: {status} {}", reply.body.trim()))),
            }
        }
        unreachable!("the last attempt returns")
    }

    /// Takes a meeting the organizer cancelled out of the calendar with
    /// `If-Match`, the organizer not told: an occurrence leaves by an EXDATE
    /// on its series, the whole event by a DELETE. A scheduling server
    /// (RFC 6638) replies for an attendee whose copy loses an event, so the
    /// copy first gets SCHEDULE-AGENT=CLIENT on its ORGANIZER, which stops
    /// that. A server that doesn't schedule ignores it. A copy that changed
    /// since Jolta read it may have been reinstated, so a 412, or a copy
    /// fetched again that isn't the version Jolta knows, stops there. Every
    /// write carries an ETag; without one the copy is left alone.
    pub async fn remove(&self, calendar: &Calendar, target: &Target) -> Result<Gone> {
        let collection = Url::parse(&calendar.remote_id).map_err(|e| Error::Other(format!("calendar address {}: {e}", calendar.remote_id)))?;
        let url = join(&collection, &path_escape(&target.event.remote_id))?;
        let known = &target.event.data;
        let here = crate::calendar::tz::here();
        let (text, etag) = match cached(&target.event) {
            Some((text, Some(etag))) => (text, etag),
            _ => match self.versioned(&url).await? {
                None => return Ok(Gone::Already),
                Some((text, _)) if !same_version(known, &text, here) => return Ok(Gone::Changed),
                Some(found) => found,
            },
        };
        let edited = match &target.occurrence {
            Some(o) => match remove::without_occurrence(&text, &known.uid, &o.recurrence_id, here)? {
                Some(edited) => Some(edited),
                None => return Ok(Gone::Already),
            },
            None => remove::quiet(&text, &known.uid),
        };
        let Some(edited) = edited else { return self.delete(&url, &etag).await };
        let headers = [("Content-Type", "text/calendar; charset=utf-8"), ("If-Match", etag.as_str())];
        let reply = self.exchange("PUT", &url, &headers, &edited).await?;
        match reply.status {
            200..300 if target.occurrence.is_some() => {
                let events = ical::read(&edited, &self.me().await)?;
                let remote_id = target.event.remote_id.clone();
                return Ok(Gone::Deleted(Some(Change::Upsert { remote_id, etag: reply.etag, events, raw: Some(edited.into_bytes()) })));
            }
            200..300 => {}
            412 => return Ok(Gone::Changed),
            404 | 410 => return Ok(Gone::Already),
            status => return Err(Error::Other(format!("CalDAV PUT {url}: {status} {}", reply.body.trim()))),
        }
        // A server that stored something else than what was sent gives no
        // ETag (RFC 4791 section 5.3.4), and someone may write in between.
        let etag = match reply.etag {
            Some(etag) => etag,
            None => match self.versioned(&url).await? {
                None => return Ok(Gone::Already),
                Some((text, _)) if !same_version(known, &text, here) => return Ok(Gone::Changed),
                Some((_, etag)) => etag,
            },
        };
        self.delete(&url, &etag).await
    }

    /// The resource and its ETag, None when it is gone. A server that gives
    /// no ETag leaves Jolta no way to write only over this version.
    async fn versioned(&self, url: &Url) -> Result<Option<(String, String)>> {
        let reply = self.exchange("GET", url, &[], "").await?;
        match (reply.status, reply.etag) {
            (200, Some(etag)) => Ok(Some((reply.body, etag))),
            (200, None) => Err(Error::Other("Your calendar server gives this meeting no version tag (ETag), so Jolta can't tell whether it changed and leaves it alone. Remove it in another calendar app.".into())),
            (404 | 410, _) => Ok(None),
            (status, _) => Err(Error::Other(format!("CalDAV GET {url}: {status}"))),
        }
    }

    async fn delete(&self, url: &Url, etag: &str) -> Result<Gone> {
        let reply = self.exchange("DELETE", url, &[("If-Match", etag)], "").await?;
        match reply.status {
            200..300 | 404 | 410 => Ok(Gone::Deleted(None)),
            412 => Ok(Gone::Changed),
            status => Err(Error::Other(format!("CalDAV DELETE {url}: {status} {}", reply.body.trim()))),
        }
    }
}

/// Whether `text` holds the version of the event `known` that Jolta read:
/// the same SEQUENCE, which an organizer's change raises (RFC 5546 section
/// 2.1.4), and the same status.
fn same_version(known: &EventData, text: &str, here: chrono_tz::Tz) -> bool {
    let same_id = |e: &EventData| match (&e.recurrence_id, &known.recurrence_id) {
        (None, None) => true,
        (Some(a), Some(b)) => a.names(b, here),
        _ => false,
    };
    ical::read(text, &[]).is_ok_and(|events| events.iter().any(|e| e.uid == known.uid && same_id(e) && e.sequence == known.sequence && e.status == known.status))
}

/// A stored resource path, decoded by `href_key`, escaped again for a URL.
fn path_escape(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b'@' | b':' | b'+' | b'=' | b',' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b';' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Each CalDAV account's addresses, by login and server, once `CalDav::me`
/// has asked for them.
static KNOWN_ADDRESSES: Mutex<BTreeMap<String, Vec<String>>> = Mutex::new(BTreeMap::new());

/// The mail addresses in a calendar-user-address-set. The set also holds
/// the principal's URLs and a urn:uuid, which aren't addresses.
fn mail_addresses(ms: &MultiStatus) -> Vec<String> {
    ms.responses
        .iter()
        .filter_map(|r| r.prop(CALDAV, "calendar-user-address-set"))
        .flat_map(|set| set.all(DAV, "href"))
        .filter_map(|href| {
            let href = href.text.trim();
            href.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("mailto:")).map(|_| href[7..].to_string())
        })
        .filter(|address| address.contains('@'))
        .collect()
}

/// The account's address first, then the others it doesn't already hold.
fn with_aliases(address: &str, found: Vec<String>) -> Vec<String> {
    let mut me = vec![address.to_string()];
    for a in found {
        if !me.iter().any(|m| m.eq_ignore_ascii_case(&a)) {
            me.push(a);
        }
    }
    me
}

/// The password goes to the hosts discovery started from, on the scheme it
/// started with, and across iCloud's own HTTPS servers, which redirect each
/// user to their own host.
fn trusts(starts: &[Url], url: &Url) -> bool {
    let icloud = |u: &Url| u.scheme() == "https" && u.host_str().is_some_and(|h| h == "icloud.com" || h.ends_with(".icloud.com"));
    starts.iter().any(|s| (s.scheme() == url.scheme() && s.host_str() == url.host_str()) || (icloud(s) && icloud(url)))
}

fn no_server(address: &str) -> Error {
    let domain = address.rsplit_once('@').map_or(address, |(_, d)| d);
    Error::Other(format!("No CalDAV server answered for {domain}. Enter its CalDAV address under Advanced."))
}

fn join(base: &Url, href: &str) -> Result<Url> {
    base.join(href).map_err(|e| Error::Other(format!("CalDAV href {href}: {e}")))
}

fn is_collection(r: &dav::Response) -> bool {
    r.prop(DAV, "resourcetype").is_some_and(|t| t.child(DAV, "collection").is_some())
}

/// The event calendars in a calendar home's depth-1 listing. Task lists and
/// other collections are left out. The primary one is the server's default
/// for invitations, else the first the user owns, else the first writable.
fn calendars(at: &Url, ms: &MultiStatus, default: Option<&Url>, principal: &Url) -> Result<Vec<RemoteCalendar>> {
    let mut out = Vec::new();
    for r in &ms.responses {
        let is_calendar = r.prop(DAV, "resourcetype").is_some_and(|t| t.child(CALDAV, "calendar").is_some());
        let events = r
            .prop(CALDAV, "supported-calendar-component-set")
            .is_none_or(|set| set.all(CALDAV, "comp").any(|c| c.attr("name").is_some_and(|n| n.eq_ignore_ascii_case("VEVENT"))));
        if !is_calendar || !events {
            continue;
        }
        let url = join(at, &r.href)?;
        let name = r.prop_text(DAV, "displayname").map(str::to_string).unwrap_or_else(|| {
            url.path_segments().and_then(|mut s| s.rfind(|p| !p.is_empty()).map(|p| p.to_string())).unwrap_or_default()
        });
        out.push(RemoteCalendar {
            remote_id: url.to_string(),
            name,
            color: r.prop_text(APPLE, "calendar-color").and_then(super::webcal::hex_color),
            zone: None,
            access: access(r, principal),
            primary: false,
            refresh: None,
        });
    }
    let default = default.map(|d| href_key(d.as_str()));
    let primary = out
        .iter()
        .position(|c| Some(href_key(&c.remote_id)) == default)
        .or_else(|| out.iter().position(|c| c.access == Access::Owner))
        .or_else(|| out.iter().position(|c| c.access == Access::Write));
    if let Some(i) = primary {
        out[i].primary = true;
    }
    Ok(out)
}

/// A calendar whose DAV:owner is the account's principal is the user's own,
/// whatever privileges it lists: Fastmail gives its default calendar no
/// `all`. One owned by another principal is shared, at most writable.
/// Without an owner, RFC 3744 privileges decide: `all` is the owner's, any
/// write privilege allows editing, `read` only reading, and
/// `read-free-busy` alone only free/busy.
fn access(r: &dav::Response, principal: &Url) -> Access {
    let privileges: Vec<&str> = r
        .prop(DAV, "current-user-privilege-set")
        .map(|set| set.all(DAV, "privilege").flat_map(|p| p.children.iter().map(|c| c.name.as_str())).collect())
        .unwrap_or_default();
    let has = |name: &str| privileges.contains(&name);
    let key = |href: &str| href_key(href).trim_end_matches('/').to_string();
    let owner = r.prop_href(DAV, "owner").map(key);
    if owner.as_ref().is_some_and(|o| *o == key(principal.as_str())) {
        Access::Owner
    } else if has("all") && owner.is_none() {
        Access::Owner
    } else if has("all") || has("write") || has("write-content") || has("bind") {
        Access::Write
    } else if has("read") || privileges.is_empty() {
        Access::Read
    } else {
        Access::FreeBusy
    }
}

/// The stored text of an event to answer on, with the ETag that guards the
/// PUT. Text without an ETag (a server may leave it out of a PUT reply) is
/// fetched again, so an answer never overwrites a newer version unguarded.
fn cached(event: &rsvp::Stored) -> Option<(String, Option<String>)> {
    let etag = event.etag.clone()?;
    event.raw.as_ref().map(|raw| (String::from_utf8_lossy(raw).into_owned(), Some(etag)))
}

#[cfg(test)]
mod tests {

    /// Meeting `a` at a SEQUENCE, as the server stores it.
    fn meeting(sequence: i64) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:a\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSEQUENCE:{sequence}\r\n\
             ORGANIZER:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// A CalDAV server on localhost that answers each request in turn with
    /// a status, an ETag or none, and for a GET meeting `a` at a SEQUENCE.
    /// It returns each request's first line, with its If-Match when it has one.
    async fn caldav_server(answers: Vec<(&'static str, Option<&'static str>, Option<i64>)>) -> (CalDav, Calendar, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, etag, sequence) in answers {
                let Ok(Ok((mut socket, _))) = tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await else { break };
                let mut request = vec![0; 16384];
                let n = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let first = request.lines().next().unwrap_or_default().trim_end_matches(" HTTP/1.1").to_string();
                let guard = request.lines().find_map(|l| l.to_ascii_lowercase().starts_with("if-match:").then(|| l[9..].trim().to_string()));
                seen.push(match guard {
                    Some(etag) => format!("{first} If-Match {etag}"),
                    None => first,
                });
                let body = sequence.map(meeting).unwrap_or_default();
                let etag = etag.map(|e| format!("ETag: {e}\r\n")).unwrap_or_default();
                let reply = format!("HTTP/1.1 {status}\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        let start = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let dav = CalDav { http: http_client(false).unwrap(), username: "me".into(), password: "pw".into(), starts: vec![start], address: "me@example.com".into(), fastmail: false, guessed: false };
        let calendar = Calendar {
            id: crate::calendar::CalendarId(1),
            account_id: None,
            source: crate::calendar::CalendarSource::CalDav,
            remote_id: format!("http://127.0.0.1:{port}/cal/"),
            name: "Home".into(),
            color: None,
            zone: None,
            access: Access::Owner,
            shown: true,
            counts_as_busy: true,
            notify: false,
            default_reminder: None,
            is_default: true,
            cursor: None,
            refresh: None,
            synced: None,
        };
        (dav, calendar, server)
    }

    /// The stored copy of meeting `a` at SEQUENCE 0.
    fn stored(etag: Option<&str>) -> Target {
        let text = meeting(0);
        let data = ical::read(&text, &["me@example.com".into()]).unwrap().remove(0);
        let event = rsvp::Stored { id: crate::calendar::EventId(1), remote_id: "a.ics".into(), etag: etag.map(str::to_string), raw: Some(text.into_bytes()), data };
        Target { event, occurrence: None, me: "me@example.com".into() }
    }

    /// A copy that changed on the server since the last sync may have been
    /// reinstated: the 412 stops the removal, with no second try.
    #[tokio::test]
    async fn a_copy_changed_since_the_sync_is_left_alone() {
        let (dav, calendar, server) = caldav_server(vec![("412 Precondition Failed", Some("\"2\""), None), ("200 OK", Some("\"2\""), Some(1)), ("204 No Content", None, None)]).await;
        assert_eq!(dav.remove(&calendar, &stored(Some("\"1\""))).await.unwrap(), Gone::Changed);
        assert_eq!(server.await.unwrap(), ["PUT /cal/a.ics If-Match \"1\""], "no GET, PUT or DELETE after the 412");
    }

    /// A server that changes what it stores answers the PUT without an
    /// ETag; the copy fetched again is deleted only while it is the version
    /// Jolta read.
    #[tokio::test]
    async fn a_put_without_an_etag_deletes_only_the_same_version() {
        let (dav, calendar, server) = caldav_server(vec![("204 No Content", None, None), ("200 OK", Some("\"3\""), Some(1)), ("204 No Content", None, None)]).await;
        assert_eq!(dav.remove(&calendar, &stored(Some("\"1\""))).await.unwrap(), Gone::Changed);
        assert_eq!(server.await.unwrap(), ["PUT /cal/a.ics If-Match \"1\"", "GET /cal/a.ics"], "the organizer changed it in between");

        let (dav, calendar, server) = caldav_server(vec![("204 No Content", None, None), ("200 OK", Some("\"3\""), Some(0)), ("204 No Content", None, None)]).await;
        assert_eq!(dav.remove(&calendar, &stored(Some("\"1\""))).await.unwrap(), Gone::Deleted(None));
        assert_eq!(server.await.unwrap(), ["PUT /cal/a.ics If-Match \"1\"", "GET /cal/a.ics", "DELETE /cal/a.ics If-Match \"3\""]);
    }

    #[tokio::test]
    async fn a_copy_without_an_etag_is_never_written() {
        let (dav, calendar, server) = caldav_server(vec![("200 OK", None, Some(0)), ("204 No Content", None, None), ("204 No Content", None, None)]).await;
        let err = dav.remove(&calendar, &stored(None)).await.unwrap_err();
        assert!(err.to_string().contains("no version tag (ETag)"), "{err}");
        assert_eq!(server.await.unwrap(), ["GET /cal/a.ics"]);

        let (dav, calendar, server) = caldav_server(vec![("200 OK", Some("\"4\""), Some(2)), ("204 No Content", None, None)]).await;
        assert_eq!(dav.remove(&calendar, &stored(None)).await.unwrap(), Gone::Changed, "newer than the stored copy");
        assert_eq!(server.await.unwrap(), ["GET /cal/a.ics"]);
    }

    #[test]
    fn stored_text_without_an_etag_is_fetched_again() {
        let event = |etag: Option<&str>| rsvp::Stored {
            id: crate::calendar::EventId(1),
            remote_id: "/cal/a.ics".into(),
            etag: etag.map(str::to_string),
            raw: Some(b"BEGIN:VCALENDAR".to_vec()),
            data: crate::calendar::EventData::new("a", crate::calendar::Span::AllDay { start: chrono::NaiveDate::MIN, end: chrono::NaiveDate::MIN }),
        };
        assert_eq!(cached(&event(None)), None, "no ETag to guard the PUT with");
        assert_eq!(cached(&event(Some("\"7\""))), Some(("BEGIN:VCALENDAR".into(), Some("\"7\"".into()))));
    }
    use super::*;

    /// The shape iCloud answers a depth-1 PROPFIND on the calendar home
    /// with: default namespaces per element, a 404 propstat for properties a
    /// collection lacks, colors with alpha, and the inbox, outbox and a task
    /// list beside the calendars.
    const ICLOUD_HOME: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
<response xmlns="DAV:"><href>/1234567890/calendars/</href>
 <propstat><prop><resourcetype xmlns="DAV:"><collection/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat>
 <propstat><prop><displayname xmlns="DAV:"/><calendar-color xmlns="http://apple.com/ns/ical/"/></prop><status>HTTP/1.1 404 Not Found</status></propstat>
</response>
<response xmlns="DAV:"><href>/1234567890/calendars/home/</href>
 <propstat><prop>
  <resourcetype xmlns="DAV:"><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype>
  <displayname xmlns="DAV:">Home</displayname>
  <calendar-color xmlns="http://apple.com/ns/ical/" symbolic-color="blue">#1BADF8FF</calendar-color>
  <supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name="VEVENT"/></supported-calendar-component-set>
  <current-user-privilege-set xmlns="DAV:"><privilege><all/></privilege><privilege><read/></privilege><privilege><write/></privilege></current-user-privilege-set>
  <getctag xmlns="http://calendarserver.org/ns/">HwoQEgwAAPa1</getctag>
  <sync-token xmlns="DAV:">HwoQEgwAAPa1</sync-token>
 </prop><status>HTTP/1.1 200 OK</status></propstat>
</response>
<response xmlns="DAV:"><href>/1234567890/calendars/2B4E6F80-1A2B-4C3D-9E8F-0A1B2C3D4E5F/</href>
 <propstat><prop>
  <resourcetype xmlns="DAV:"><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/><shared xmlns="http://calendarserver.org/ns/"/></resourcetype>
  <displayname xmlns="DAV:">Family &amp; friends</displayname>
  <calendar-color xmlns="http://apple.com/ns/ical/">#CC73E1FF</calendar-color>
  <supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name="VEVENT"/></supported-calendar-component-set>
  <current-user-privilege-set xmlns="DAV:"><privilege><read/></privilege><privilege><read-free-busy xmlns="urn:ietf:params:xml:ns:caldav"/></privilege></current-user-privilege-set>
 </prop><status>HTTP/1.1 200 OK</status></propstat>
</response>
<response xmlns="DAV:"><href>/1234567890/calendars/tasks/</href>
 <propstat><prop>
  <resourcetype xmlns="DAV:"><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype>
  <displayname xmlns="DAV:">Reminders</displayname>
  <supported-calendar-component-set xmlns="urn:ietf:params:xml:ns:caldav"><comp name="VTODO"/></supported-calendar-component-set>
 </prop><status>HTTP/1.1 200 OK</status></propstat>
</response>
<response xmlns="DAV:"><href>/1234567890/calendars/inbox/</href>
 <propstat><prop><resourcetype xmlns="DAV:"><collection/><schedule-inbox xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat>
</response>
</multistatus>"##;

    /// The shape Fastmail (Cyrus) answers with: prefixed namespaces declared
    /// once, percent-escaped hrefs, and a calendar for both events and tasks.
    const FASTMAIL_HOME: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:A="http://apple.com/ns/ical/">
<D:response><D:href>/dav/calendars/user/me%40example.com/</D:href>
 <D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype><D:displayname>me@example.com</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
</D:response>
<D:response><D:href>/dav/calendars/user/me%40example.com/Default/</D:href>
 <D:propstat><D:prop>
  <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
  <D:displayname>Calendar</D:displayname>
  <A:calendar-color>#3a429c</A:calendar-color>
  <C:supported-calendar-component-set><C:comp name="VEVENT"/><C:comp name="VTODO"/></C:supported-calendar-component-set>
  <D:current-user-privilege-set><D:privilege><D:read/></D:privilege><D:privilege><D:write-content/></D:privilege><D:privilege><D:bind/></D:privilege></D:current-user-privilege-set>
  <D:owner><D:href>/dav/principals/user/me%40example.com</D:href></D:owner>
  <D:sync-token>https://example.com/ns/sync/4711</D:sync-token>
 </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
 <D:propstat><D:prop><CS:getctag/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
</D:response>
<D:response><D:href>/dav/calendars/user/me%40example.com/b7c1d2e3/</D:href>
 <D:propstat><D:prop>
  <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
  <D:displayname>Work</D:displayname>
  <D:current-user-privilege-set><D:privilege><D:all/></D:privilege></D:current-user-privilege-set>
 </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
</D:response>
<D:response><D:href>/dav/calendars/user/me%40example.com/ann.example.com.c4d5/</D:href>
 <D:propstat><D:prop>
  <D:resourcetype><D:collection/><C:calendar/></D:resourcetype>
  <D:displayname>Ann's calendar</D:displayname>
  <D:current-user-privilege-set><D:privilege><D:all/></D:privilege></D:current-user-privilege-set>
  <D:owner><D:href>/dav/principals/user/ann%40example.com/</D:href></D:owner>
 </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
</D:response>
</D:multistatus>"##;

    fn names(list: &[RemoteCalendar]) -> Vec<(&str, &str, Option<&str>, Access, bool)> {
        list.iter().map(|c| (c.remote_id.as_str(), c.name.as_str(), c.color.as_deref(), c.access, c.primary)).collect()
    }

    #[test]
    fn the_principals_addresses_hold_the_aliases() {
        let reply = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:"><response><href>/1234567890/principal/</href><propstat><prop>
 <calendar-user-address-set xmlns="urn:ietf:params:xml:ns:caldav">
  <href xmlns="DAV:" preferred="1">mailto:koen@icloud.example</href>
  <href xmlns="DAV:">MAILTO:koen.alias@icloud.example</href>
  <href xmlns="DAV:">mailto:Koen@Example.org</href>
  <href xmlns="DAV:">https://p42-caldav.icloud.com:443/1234567890/principal/</href>
  <href xmlns="DAV:">/1234567890/principal/</href>
  <href xmlns="DAV:">urn:uuid:1234567890</href>
 </calendar-user-address-set>
</prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        let found = mail_addresses(&dav::multistatus(reply).unwrap());
        assert_eq!(found, ["koen@icloud.example", "koen.alias@icloud.example", "Koen@Example.org"]);
        assert_eq!(with_aliases("koen@example.org", found), ["koen@example.org", "koen@icloud.example", "koen.alias@icloud.example"]);
    }

    #[test]
    fn the_password_goes_only_to_the_accounts_own_servers() {
        let url = |s: &str| Url::parse(s).unwrap();
        let icloud = [url("https://caldav.icloud.com/")];
        assert!(trusts(&icloud, &url("https://caldav.icloud.com/1234567890/calendars/")));
        assert!(trusts(&icloud, &url("https://p42-caldav.icloud.com:443/1234567890/calendars/")), "iCloud sends each user to their own host");
        assert!(!trusts(&icloud, &url("http://caldav.icloud.com/1234567890/calendars/")), "not over plain HTTP");
        assert!(!trusts(&icloud, &url("http://p42-caldav.icloud.com/1234567890/calendars/")));
        assert!(!trusts(&icloud, &url("https://attacker.example/")));
        assert!(!trusts(&icloud, &url("https://caldav.icloud.com.attacker.example/")));
        let typed = [url("http://nas.example:5232/")];
        assert!(trusts(&typed, &url("http://nas.example:5232/me/calendar/")), "an address typed under Advanced keeps working");
        assert!(!trusts(&typed, &url("http://other.example:5232/")));
    }

    #[test]
    fn icloud_calendar_home() {
        let at = Url::parse("https://p42-caldav.icloud.com:443/1234567890/calendars/").unwrap();
        let principal = Url::parse("https://p42-caldav.icloud.com/1234567890/principal/").unwrap();
        let list = calendars(&at, &dav::multistatus(ICLOUD_HOME).unwrap(), None, &principal).unwrap();
        assert_eq!(
            names(&list),
            [
                ("https://p42-caldav.icloud.com/1234567890/calendars/home/", "Home", Some("#1BADF8"), Access::Owner, true),
                (
                    "https://p42-caldav.icloud.com/1234567890/calendars/2B4E6F80-1A2B-4C3D-9E8F-0A1B2C3D4E5F/",
                    "Family & friends",
                    Some("#CC73E1"),
                    Access::Read,
                    false
                ),
            ],
            "the task list, inbox and home itself are left out"
        );
    }

    #[test]
    fn fastmail_calendar_home_with_the_servers_default() {
        let at = Url::parse("https://caldav.fastmail.com/dav/calendars/user/me%40example.com/").unwrap();
        let default = Url::parse("https://caldav.fastmail.com/dav/calendars/user/me@example.com/Default/").unwrap();
        let principal = Url::parse("https://caldav.fastmail.com/dav/principals/user/me@example.com/").unwrap();
        let list = calendars(&at, &dav::multistatus(FASTMAIL_HOME).unwrap(), Some(&default), &principal).unwrap();
        assert_eq!(
            names(&list),
            [
                ("https://caldav.fastmail.com/dav/calendars/user/me%40example.com/Default/", "Calendar", Some("#3a429c"), Access::Owner, true),
                ("https://caldav.fastmail.com/dav/calendars/user/me%40example.com/b7c1d2e3/", "Work", None, Access::Owner, false),
                ("https://caldav.fastmail.com/dav/calendars/user/me%40example.com/ann.example.com.c4d5/", "Ann's calendar", None, Access::Write, false),
            ],
            "the default for invitations wins over the first owned calendar, matched across escapes; the owner decides, not the privileges"
        );
    }

    #[test]
    fn sync_collection_reply() {
        let reply = r#"<?xml version="1.0" encoding="UTF-8"?>
<d:multistatus xmlns:d="DAV:">
 <d:response><d:href>/1234567890/calendars/home/A1.ics</d:href>
  <d:propstat><d:prop><d:getetag>&quot;mf2k1a&quot;</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/1234567890/calendars/home/B%202.ics</d:href>
  <d:propstat><d:prop><d:getetag>"mf2k1b"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/1234567890/calendars/home/gone.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>
 <d:response><d:href>/1234567890/calendars/home/</d:href><d:status>HTTP/1.1 507 Insufficient Storage</d:status></d:response>
 <d:sync-token>HwoQEgwAAPa2</d:sync-token>
</d:multistatus>"#;
        let ms = dav::multistatus(reply).unwrap();
        assert_eq!(ms.sync_token.as_deref(), Some("HwoQEgwAAPa2"));
        let mut listing = Listing { members: Vec::new(), gone: Vec::new(), complete: false, cursor: Cursor::default() };
        assert!(listing.absorb(&ms, &href_key("/1234567890/calendars/home/")), "a 507 on the collection means more follows");
        let members: Vec<(String, Option<String>)> =
            listing.members.iter().map(|(h, e)| (href_key(h), e.clone())).collect();
        assert_eq!(
            members,
            [
                ("/1234567890/calendars/home/A1.ics".to_string(), Some("\"mf2k1a\"".to_string())),
                ("/1234567890/calendars/home/B 2.ics".to_string(), Some("\"mf2k1b\"".to_string())),
            ]
        );
        assert_eq!(listing.gone, ["/1234567890/calendars/home/gone.ics"]);
    }

    #[test]
    fn multiget_calendar_data_keeps_its_lines() {
        let reply = "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response>\
            <d:href>/cal/A1.ics</d:href><d:propstat><d:prop><d:getetag>\"1\"</d:getetag>\
            <c:calendar-data>BEGIN:VCALENDAR&#13;\nSUMMARY:Tom &amp; Jerry&#13;\nEND:VCALENDAR&#13;\n</c:calendar-data>\
            </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>\
            <d:response><d:href>/cal/B.ics</d:href><d:propstat><d:prop>\
            <c:calendar-data><![CDATA[BEGIN:VCALENDAR\r\nSUMMARY:<b>&\r\nEND:VCALENDAR\r\n]]></c:calendar-data>\
            </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>";
        let ms = dav::multistatus(reply).unwrap();
        let data: Vec<&str> = ms.responses.iter().map(|r| r.prop(CALDAV, "calendar-data").unwrap().text.as_str()).collect();
        assert_eq!(
            data,
            ["BEGIN:VCALENDAR\r\nSUMMARY:Tom & Jerry\r\nEND:VCALENDAR\r\n", "BEGIN:VCALENDAR\r\nSUMMARY:<b>&\r\nEND:VCALENDAR\r\n"]
        );
    }
}
