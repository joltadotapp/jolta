//! Subscribed feeds: an `.ics` file at a `webcal://` or `https://` address,
//! downloaded again when it changed. Read-only.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Change, Known, SyncBatch, http_client, http_error, vanished};
use crate::calendar::{Access, Calendar, EventData, ical, series};
use crate::store::RemoteCalendar;
use crate::{Error, Result};

/// RFC 7986 leaves the interval to the client when a feed doesn't say.
const DEFAULT_REFRESH: i64 = 6 * 3600;

pub struct Webcal {
    http: reqwest::Client,
    url: String,
}

/// The validators of the last download, sent back so an unchanged feed
/// answers 304 without a body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Cursor {
    etag: Option<String>,
    modified: Option<String>,
}

impl Webcal {
    pub fn new(url: &str) -> Result<Webcal> {
        let url = url.trim();
        let url = match url.split_once("://") {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case("webcal") || scheme.eq_ignore_ascii_case("webcals") => {
                format!("https://{rest}")
            }
            Some((scheme, _)) if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http") => url.to_string(),
            _ => return Err(Error::Other(format!("not a calendar feed address: {url}"))),
        };
        Ok(Webcal { http: http_client(true)?, url })
    }

    /// The address downloads use: `webcal://` becomes `https://`.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        let resp = self.http.get(&self.url).send().await.map_err(http_error)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Other(format!("the feed answered {status}")));
        }
        let text = resp.text().await.map_err(http_error)?;
        let header = FeedHeader::read(&text);
        let host = self.url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(&self.url);
        Ok(vec![RemoteCalendar {
            remote_id: self.url.clone(),
            name: header.name.unwrap_or_else(|| host.to_string()),
            color: header.color,
            zone: header.zone,
            access: Access::Read,
            primary: true,
            refresh: Some(header.refresh.unwrap_or(DEFAULT_REFRESH)),
        }])
    }

    pub async fn sync(&self, calendar: &Calendar, known: &Known) -> Result<SyncBatch> {
        let cursor: Cursor = calendar.cursor.as_deref().and_then(|c| serde_json::from_str(c).ok()).unwrap_or_default();
        let mut req = self.http.get(&self.url);
        if let Some(etag) = &cursor.etag {
            req = req.header("If-None-Match", etag);
        }
        if let Some(modified) = &cursor.modified {
            req = req.header("If-Modified-Since", modified);
        }
        let resp = req.send().await.map_err(http_error)?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_MODIFIED {
            tracing::debug!("{}: not modified", self.url);
            return Ok(SyncBatch { changes: Vec::new(), cursor: calendar.cursor.clone(), refresh: None });
        }
        if !status.is_success() {
            return Err(Error::Other(format!("the feed answered {status}")));
        }
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let next = Cursor { etag: header("ETag"), modified: header("Last-Modified") };
        let text = resp.text().await.map_err(http_error)?;
        let refresh = FeedHeader::read(&text).refresh.unwrap_or(DEFAULT_REFRESH);
        let events = ical::read(&text, &[])?;
        let mut batch = split(events, known);
        batch.cursor = Some(serde_json::to_string(&next).expect("a cursor serializes"));
        batch.refresh = Some(refresh);
        Ok(batch)
    }
}

/// One change per UID, with a hash of its events as the ETag so unchanged
/// UIDs of a changed feed are skipped, and removals for UIDs that left it.
fn split(events: Vec<EventData>, known: &Known) -> SyncBatch {
    let mut present = BTreeSet::new();
    let mut changes: Vec<Change> = series::group(events)
        .into_iter()
        .map(|(uid, events)| {
            let json = serde_json::to_vec(&events).expect("events serialize");
            let etag = hex::encode(&Sha256::digest(&json)[..16]);
            present.insert(uid.clone());
            Change::Upsert { remote_id: uid, etag: Some(etag), events, raw: None }
        })
        .collect();
    changes.extend(vanished(known, &present));
    SyncBatch { changes, cursor: None, refresh: None }
}

/// The feed's own properties, from the VCALENDAR lines before its components.
#[derive(Debug, Default, PartialEq, Eq)]
struct FeedHeader {
    name: Option<String>,
    color: Option<String>,
    zone: Option<String>,
    refresh: Option<i64>,
}

impl FeedHeader {
    fn read(text: &str) -> FeedHeader {
        let mut out = FeedHeader::default();
        let mut depth = 0;
        for line in unfold(text) {
            let Some((head, value)) = line.split_once(':') else { continue };
            let name = head.split(';').next().unwrap_or(head).to_ascii_uppercase();
            match name.as_str() {
                "BEGIN" => depth += 1,
                "END" => depth -= 1,
                _ if depth != 1 => {}
                "X-WR-CALNAME" | "NAME" if out.name.is_none() => out.name = Some(unescape(value)),
                "X-WR-TIMEZONE" => out.zone = Some(value.trim().to_string()),
                "X-APPLE-CALENDAR-COLOR" => out.color = hex_color(value),
                "REFRESH-INTERVAL" => out.refresh = duration_secs(value),
                _ => {}
            }
        }
        out
    }
}

/// Lines with RFC 5545 folding undone.
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        match (line.strip_prefix([' ', '\t']), out.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => out.push(line.to_string()),
        }
    }
    out
}

fn unescape(value: &str) -> String {
    value.trim().replace("\\,", ",").replace("\\;", ";").replace("\\n", " ").replace("\\N", " ").replace("\\\\", "\\")
}

/// `#RRGGBB` from Apple's `#RRGGBBAA`.
pub(super) fn hex_color(value: &str) -> Option<String> {
    let v = value.trim();
    let hex = v.strip_prefix('#')?;
    (hex.len() >= 6 && hex.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("#{}", &hex[..6]))
}

/// Seconds in an RFC 5545 duration such as `P1W`, `P1D` or `PT6H`.
fn duration_secs(value: &str) -> Option<i64> {
    let rest = value.trim().strip_prefix('+').unwrap_or(value.trim()).strip_prefix('P')?;
    let (mut total, mut number, mut time) = (0i64, String::new(), false);
    for c in rest.chars() {
        match c {
            'T' => time = true,
            '0'..='9' => number.push(c),
            unit => {
                let n: i64 = std::mem::take(&mut number).parse().ok()?;
                total += n * match (unit, time) {
                    ('W', false) => 7 * 86_400,
                    ('D', false) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    (number.is_empty() && total > 0).then_some(total)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::NaiveDate;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::calendar::{CalendarId, CalendarSource, Span};

    #[test]
    fn feed_header() {
        let feed = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-CALNAME:Holidays in \r\n Belgium\r\nX-WR-TIMEZONE:Europe/Brussels\r\n\
                    REFRESH-INTERVAL;VALUE=DURATION:P1W\r\nX-APPLE-CALENDAR-COLOR:#1BADF8FF\r\nBEGIN:VEVENT\r\n\
                    NAME:not the calendar\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            FeedHeader::read(feed),
            FeedHeader {
                name: Some("Holidays in Belgium".into()),
                color: Some("#1BADF8".into()),
                zone: Some("Europe/Brussels".into()),
                refresh: Some(7 * 86_400),
            }
        );
        assert_eq!(duration_secs("PT6H"), Some(21_600));
        assert_eq!(duration_secs("P1DT12H"), Some(129_600));
        assert_eq!(duration_secs("P"), None);
        assert_eq!(duration_secs("PT6"), None);
    }

    #[test]
    fn webcal_becomes_https() {
        assert_eq!(Webcal::new("webcal://example.com/a.ics").unwrap().url(), "https://example.com/a.ics");
        assert!(Webcal::new("ftp://example.com/a.ics").is_err());
    }

    #[test]
    fn one_change_per_uid_and_removals_for_the_rest() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let span = Span::AllDay { start: day, end: day.succ_opt().unwrap() };
        let events = vec![EventData::new("a", span), EventData::new("b", span), EventData::new("a", span)];
        let known: Known = BTreeMap::from([("a".to_string(), None), ("gone".to_string(), None)]);
        let batch = split(events.clone(), &known);
        let ids: Vec<(&str, usize)> = batch
            .changes
            .iter()
            .map(|c| match c {
                Change::Upsert { remote_id, events, .. } => (remote_id.as_str(), events.len()),
                Change::Remove { remote_id, .. } => (remote_id.as_str(), 0),
            })
            .collect();
        assert_eq!(ids, [("a", 2), ("b", 1), ("gone", 0)]);
        assert_eq!(split(events.clone(), &known), split(events, &known), "the same feed gives the same ETags");
    }

    /// A feed whose validators match answers 304, and the cursor stays.
    #[tokio::test]
    async fn unchanged_feed_is_not_downloaded_again() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
            let answer = if request.contains("if-none-match: \"v1\"") && request.contains("if-modified-since: tue, 03 nov 2026") {
                "HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n\r\n"
            } else {
                "HTTP/1.1 500 Missing validators\r\nContent-Length: 0\r\n\r\n"
            };
            socket.write_all(answer.as_bytes()).await.unwrap();
        });
        let cursor = r#"{"etag":"\"v1\"","modified":"Tue, 03 Nov 2026 10:00:00 GMT"}"#;
        let calendar = Calendar {
            id: CalendarId(1),
            account_id: None,
            source: CalendarSource::Webcal,
            remote_id: format!("http://127.0.0.1:{port}/feed.ics"),
            name: "Feed".into(),
            color: None,
            zone: None,
            access: Access::Read,
            shown: true,
            counts_as_busy: false,
            notify: false,
            default_reminder: None,
            is_default: false,
            cursor: Some(cursor.into()),
            refresh: None,
            synced: None,
        };
        let feed = Webcal::new(&calendar.remote_id).unwrap();
        let batch = feed.sync(&calendar, &Known::new()).await.unwrap();
        assert_eq!(batch, SyncBatch { changes: Vec::new(), cursor: Some(cursor.into()), refresh: None });
        server.await.unwrap();
    }
}
