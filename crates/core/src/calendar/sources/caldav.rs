//! CalDAV (RFC 4791) for iCloud, Fastmail and other servers: discovery
//! (RFC 6764), the calendar list, and sync through sync-collection
//! (RFC 6578), with the collection's ctag and the resources' ETags for
//! servers without it. Read-only: PROPFIND and REPORT, nothing else.

use std::collections::BTreeSet;
use std::time::Duration;

use quick_xml::escape::escape;
use reqwest::Url;
use serde::{Deserialize, Serialize};

use super::dav::{self, APPLE, CALDAV, CALENDARSERVER, DAV, MultiStatus, href_key};
use super::{Change, Known, SyncBatch, http_client, http_error, vanished};
use crate::calendar::{Access, Calendar, ical};
use crate::store::RemoteCalendar;
use crate::{Account, Error, Result, secrets};

/// Resources per calendar-multiget, so one reply stays a few hundred KB.
const MULTIGET_BATCH: usize = 50;

const PRINCIPAL: &str = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
const HOME: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/><c:schedule-inbox-URL/></d:prop></d:propfind>"#;
const DEFAULT_CALENDAR: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:schedule-default-calendar-URL/></d:prop></d:propfind>"#;
const CALENDARS: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:a="http://apple.com/ns/ical/"><d:prop><d:resourcetype/><d:displayname/><a:calendar-color/><c:supported-calendar-component-set/><d:current-user-privilege-set/><cs:getctag/><d:sync-token/></d:prop></d:propfind>"#;
const CTAG: &str = r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:getctag/></d:prop></d:propfind>"#;
const ETAGS: &str = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getetag/></d:prop></d:propfind>"#;

pub struct CalDav {
    http: reqwest::Client,
    username: String,
    password: String,
    /// Where discovery starts, in order.
    starts: Vec<Url>,
    /// The user's own addresses, to find their answer on invitations.
    me: Vec<String>,
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

struct Reply {
    status: u16,
    /// Where the answer came from after redirects; hrefs resolve against it.
    url: Url,
    body: String,
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
            me: vec![account.address.clone()],
            fastmail,
            guessed,
        })
    }

    /// The password goes to the hosts discovery started from, and across
    /// iCloud's own servers, which redirect each user to their own host.
    fn trusted(&self, url: &Url) -> bool {
        let icloud = |u: &Url| u.host_str().is_some_and(|h| h == "icloud.com" || h.ends_with(".icloud.com"));
        self.starts.iter().any(|s| s.host_str() == url.host_str() || (icloud(s) && icloud(url)))
    }

    /// One request, following redirects with the same method and body.
    async fn send(&self, method: &str, url: &Url, depth: &str, body: &str) -> Result<Reply> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("a valid method");
        let mut url = url.clone();
        let mut attempts = 0;
        for _ in 0..10 {
            let mut req = self
                .http
                .request(method.clone(), url.clone())
                .header("Depth", depth)
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body.to_string());
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
            let body = resp.text().await.map_err(http_error)?;
            return Ok(Reply { status, url, body });
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
            _ => no_server(&self.me[0]),
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
        calendars(&at, &ms, default.as_ref())
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

    /// The changed resources' iCalendar data, in batches.
    async fn multiget(&self, url: &Url, hrefs: &[String]) -> Result<Vec<Change>> {
        let mut out = Vec::with_capacity(hrefs.len());
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
                let Some(data) = r.prop(CALDAV, "calendar-data").map(|p| p.text.as_str()) else { continue };
                match ical::read(data, &self.me) {
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
fn calendars(at: &Url, ms: &MultiStatus, default: Option<&Url>) -> Result<Vec<RemoteCalendar>> {
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
            access: access(r),
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

/// RFC 3744 privileges: `all` is the owner's, any write privilege allows
/// editing, `read` only reading, and `read-free-busy` alone only free/busy.
fn access(r: &dav::Response) -> Access {
    let privileges: Vec<&str> = r
        .prop(DAV, "current-user-privilege-set")
        .map(|set| set.all(DAV, "privilege").flat_map(|p| p.children.iter().map(|c| c.name.as_str())).collect())
        .unwrap_or_default();
    let has = |name: &str| privileges.contains(&name);
    if has("all") {
        Access::Owner
    } else if has("write") || has("write-content") || has("bind") {
        Access::Write
    } else if has("read") || privileges.is_empty() {
        Access::Read
    } else {
        Access::FreeBusy
    }
}

#[cfg(test)]
mod tests {
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
</D:multistatus>"##;

    fn names(list: &[RemoteCalendar]) -> Vec<(&str, &str, Option<&str>, Access, bool)> {
        list.iter().map(|c| (c.remote_id.as_str(), c.name.as_str(), c.color.as_deref(), c.access, c.primary)).collect()
    }

    #[test]
    fn icloud_calendar_home() {
        let at = Url::parse("https://p42-caldav.icloud.com:443/1234567890/calendars/").unwrap();
        let list = calendars(&at, &dav::multistatus(ICLOUD_HOME).unwrap(), None).unwrap();
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
        let list = calendars(&at, &dav::multistatus(FASTMAIL_HOME).unwrap(), Some(&default)).unwrap();
        assert_eq!(
            names(&list),
            [
                ("https://caldav.fastmail.com/dav/calendars/user/me%40example.com/Default/", "Calendar", Some("#3a429c"), Access::Write, true),
                ("https://caldav.fastmail.com/dav/calendars/user/me%40example.com/b7c1d2e3/", "Work", None, Access::Owner, false),
            ],
            "the default for invitations wins over the first owned calendar, matched across escapes"
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
