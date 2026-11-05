//! Microsoft 365 calendars through Microsoft Graph. The one write is
//! answering an invitation.
//!
//! Graph hands out a calendar's events as instances (`calendarView`), with
//! a delta query that tracks the window from 2 years back to 3 years
//! ahead. Jolta expands series itself, so it keeps single events and
//! exceptions from that view, and fetches each series master it points to
//! by id: only the master holds the rule (`patternedRecurrence`, turned into
//! an RRULE here) and the cancelled occurrences. Plain occurrences are what
//! expansion produces, so they are dropped, unless their master can't be
//! fetched. Where Graph refuses delta for a calendar, each sync lists the
//! window and compares change keys.

mod add;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Change, Gone, Known, SyncBatch, encode, http_client, http_error, provider_url, vanished};
use crate::calendar::meeting::{self, Entry, EntryKind};
use crate::calendar::rsvp::{Answer, Occurrence, Rsvp, Stored, Target};
use crate::calendar::{
    Access, Alarm, AlarmTrigger, Attendee, Calendar, EventData, EventStatus, Local, Moment, Person,
    Recurrence, Response, Role, ShowAs, Span, Visibility, Zone, tz,
};
use crate::store::RemoteCalendar;
use crate::{Account, Error, Result, oauth};

const BASE: &str = "https://graph.microsoft.com/v1.0";
const PREFER: &str = r#"outlook.timezone="UTC", IdType="ImmutableId", odata.maxpagesize=200"#;
const FIELDS: &str = "id,changeKey,type,seriesMasterId,iCalUId,subject,body,location,start,end,isAllDay,originalStart,\
originalStartTimeZone,originalEndTimeZone,recurrence,showAs,sensitivity,responseStatus,attendees,organizer,isOrganizer,\
isReminderOn,reminderMinutesBeforeStart,onlineMeeting,categories,isCancelled,allowNewTimeProposals";
/// Requests per JSON batch, Graph's limit.
const BATCH: usize = 20;

pub struct GraphCalendar {
    http: reqwest::Client,
    account: Account,
    /// The zone for events whose own zone Graph names in a way Jolta can't
    /// map (`tzone://Microsoft/Custom`).
    here: Tz,
}

/// Where the next sync starts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum Cursor {
    Delta {
        link: String,
        /// The month the window was placed in. A new month places it again.
        month: String,
        /// Masters of the series with instances in the window. A deleted
        /// occurrence comes back from delta as a bare id, so these are all
        /// fetched again to find which series lost it.
        masters: BTreeSet<String>,
    },
    /// Graph refused delta for this calendar: list the window each time.
    Poll,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GraphEvent {
    id: String,
    change_key: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    series_master_id: Option<String>,
    #[serde(rename = "iCalUId", deserialize_with = "original_uid")]
    ical_uid: Option<String>,
    subject: Option<String>,
    body: Option<Body>,
    location: Option<Location>,
    start: Option<When>,
    end: Option<When>,
    is_all_day: bool,
    original_start: Option<DateTime<chrono::FixedOffset>>,
    original_start_time_zone: Option<String>,
    original_end_time_zone: Option<String>,
    recurrence: Option<PatternedRecurrence>,
    show_as: Option<String>,
    sensitivity: Option<String>,
    response_status: Option<Status>,
    attendees: Vec<GraphAttendee>,
    organizer: Option<Recipient>,
    is_organizer: bool,
    is_reminder_on: bool,
    reminder_minutes_before_start: i64,
    online_meeting: Option<OnlineMeeting>,
    categories: Vec<String>,
    is_cancelled: bool,
    cancelled_occurrences: Vec<String>,
    #[serde(rename = "@removed")]
    removed: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Body {
    content_type: String,
    content: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Location {
    display_name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct When {
    date_time: String,
    time_zone: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Status {
    response: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Recipient {
    email_address: Address,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Address {
    name: String,
    address: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GraphAttendee {
    #[serde(rename = "type")]
    kind: String,
    status: Option<Status>,
    email_address: Address,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct OnlineMeeting {
    join_url: Option<String>,
    conference_id: Option<String>,
    toll_number: Option<String>,
    phones: Vec<Phone>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Phone {
    number: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PatternedRecurrence {
    pattern: Pattern,
    range: RecurrenceRange,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Pattern {
    #[serde(rename = "type")]
    kind: String,
    interval: u32,
    month: u32,
    day_of_month: u32,
    days_of_week: Vec<String>,
    first_day_of_week: Option<String>,
    index: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RecurrenceRange {
    #[serde(rename = "type")]
    kind: String,
    end_date: Option<NaiveDate>,
    recurrence_time_zone: Option<String>,
    number_of_occurrences: u32,
}

/// The instances one sync read.
struct Round {
    items: Vec<Value>,
    /// Every instance in the window is listed, so known events missing
    /// from it are gone: a first delta round, or a poll.
    complete: bool,
    delta_link: Option<String>,
}

/// A series master as fetched by id. `None` when Graph can't give it.
type Masters = BTreeMap<String, Option<(GraphEvent, Vec<u8>)>>;

impl GraphCalendar {
    pub fn new(account: &Account) -> Result<GraphCalendar> {
        Ok(GraphCalendar { http: http_client(true)?, account: account.clone(), here: tz::here() })
    }

    /// One request with the account's token and Jolta's `Prefer` header.
    /// Throttling (429) and brief outages are retried after Retry-After, as
    /// the mail engine does; every other status goes back to the caller.
    pub async fn request(&self, method: &str, url: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<(u16, Value)> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| Error::Other(format!("HTTP method {method}: {e}")))?;
        let url = if url.starts_with('/') { format!("{BASE}{url}") } else { url.to_string() };
        let url = provider_url(&url, "graph.microsoft.com")?;
        for attempt in 0..5u32 {
            let token = oauth::access_token(&self.account).await?;
            let mut req = self.http.request(method.clone(), url.clone()).bearer_auth(&token).header("Prefer", PREFER);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            if let Some(body) = body {
                req = req.header("Content-Type", "application/json").body(body.to_string());
            }
            let resp = req.send().await.map_err(http_error)?;
            let status = resp.status().as_u16();
            let retry_after = resp.headers().get("Retry-After").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            if matches!(status, 429 | 503 | 504) && attempt < 4 {
                tokio::time::sleep(Duration::from_secs(retry_after.unwrap_or(2 << attempt).min(120))).await;
                continue;
            }
            let bytes = resp.bytes().await.map_err(http_error)?;
            let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if status == 401 {
                return Err(Error::Auth(format!("Microsoft refused the sign-in for {}. Sign in again.", self.account.address)));
            }
            return Ok((status, json));
        }
        unreachable!("the last attempt returns")
    }

    fn failure(&self, status: u16, json: &Value) -> Error {
        let code = json["error"]["code"].as_str().unwrap_or_default();
        if status == 403 && code == "ErrorAccessDenied" {
            return Error::Auth(format!("Microsoft gave Jolta no calendar access for {}. Turn calendar off and on again for it to ask.", self.account.address));
        }
        let message = json["error"]["message"].as_str().unwrap_or(code);
        Error::Other(format!("Microsoft Graph {status}: {message}"))
    }

    pub async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        let mut out = Vec::new();
        let mut next = Some("/me/calendars?$select=id,name,hexColor,isDefaultCalendar,canEdit,owner&$top=100".to_string());
        while let Some(url) = next {
            let page = get_ok(self, &url).await?;
            for c in page["value"].as_array().into_iter().flatten() {
                out.push(remote_calendar(c, &self.account.address));
            }
            next = page["@odata.nextLink"].as_str().map(str::to_string);
        }
        Ok(out)
    }

    pub async fn sync(&self, calendar: &Calendar, known: &Known) -> Result<SyncBatch> {
        read(self, &calendar.remote_id, calendar.cursor.as_deref(), known, &self.account.address).await
    }

    /// `accept`, `tentativelyAccept` or `decline` with `sendResponse`, and
    /// the note as `comment` when a reply goes out. Exchange sends the
    /// organizer the reply, with a new time as `proposedNewTime`.
    pub async fn answer(&self, target: &Target, rsvp: &Rsvp) -> Result<String> {
        let id = match &target.occurrence {
            None => target.event.remote_id.clone(),
            Some(Occurrence { stored: Some(s), .. }) => s.remote_id.clone(),
            Some(Occurrence { recurrence_id, stored: None }) => self.instance(&target.event, recurrence_id).await?,
        };
        let action = match rsvp.answer {
            Answer::Accept => "accept",
            Answer::Maybe => "tentativelyAccept",
            Answer::Decline => "decline",
        };
        let body = answer_body(rsvp);
        let (status, reply) = self.request("POST", &format!("/me/events/{}/{action}", encode(&id)), Some(&body), &[]).await?;
        if !(200..300).contains(&status) {
            return Err(self.failure(status, &reply));
        }
        Ok(id)
    }

    /// Deletes a meeting the organizer cancelled from the user's calendar,
    /// unless Exchange has removed it or marked it cancelled already, as it
    /// does when it reads the organizer's cancellation. Deleting an
    /// attendee's copy sends the organizer nothing.
    pub async fn remove(&self, target: &Target) -> Result<Gone> {
        let id = match &target.occurrence {
            None => target.event.remote_id.clone(),
            Some(Occurrence { stored: Some(s), .. }) => s.remote_id.clone(),
            Some(Occurrence { recurrence_id, stored: None }) => match self.instance(&target.event, recurrence_id).await {
                Ok(id) => id,
                Err(Error::NotFound(_)) => return Ok(Gone::Already),
                Err(e) => return Err(e),
            },
        };
        let path = format!("/me/events/{}", encode(&id));
        let (status, event) = self.request("GET", &format!("{path}?$select=isCancelled"), None, &[]).await?;
        if graph_gone(status, &event) {
            return Ok(Gone::Already);
        }
        if !(200..300).contains(&status) {
            return Err(self.failure(status, &event));
        }
        let (status, reply) = self.request("DELETE", &path, None, &[]).await?;
        match status {
            200..300 | 404 => Ok(Gone::Deleted(None)),
            _ => Err(self.failure(status, &reply)),
        }
    }

    /// The id of the occurrence of `master` that `rid` names.
    async fn instance(&self, master: &Stored, rid: &Moment) -> Result<String> {
        Ok(instance(self, master, rid).await?.id)
    }
}

/// What a sync and the writes to a copy ask of Microsoft 365:
/// `GraphCalendar` sends it, a test answers it itself.
pub(super) trait Api {
    async fn request(&self, method: &str, url: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<(u16, Value)>;
    fn failure(&self, status: u16, json: &Value) -> Error;
    /// The zone for a series whose own zone Graph names in a way Jolta can't map.
    fn zone(&self) -> Tz;
}

impl Api for GraphCalendar {
    async fn request(&self, method: &str, url: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> Result<(u16, Value)> {
        GraphCalendar::request(self, method, url, body, headers).await
    }

    fn failure(&self, status: u16, json: &Value) -> Error {
        GraphCalendar::failure(self, status, json)
    }

    fn zone(&self) -> Tz {
        self.here
    }
}

/// One sync of the calendar `calendar` from `cursor`, for the account `me`.
async fn read(api: &impl Api, calendar: &str, cursor: Option<&str>, known: &Known, me: &str) -> Result<SyncBatch> {
    let now = Utc::now().date_naive();
    let month = now.format("%Y-%m").to_string();
    let cursor: Option<Cursor> = cursor.and_then(|c| serde_json::from_str(c).ok());
    let (round, earlier) = match cursor {
        Some(Cursor::Poll) => (poll(api, calendar, now).await?, BTreeSet::new()),
        Some(Cursor::Delta { link, month: m, masters }) if m == month => match pages(api, &link).await? {
            Some(round) => (round, masters),
            None => first_round(api, calendar, now).await?,
        },
        _ => first_round(api, calendar, now).await?,
    };
    let complete = round.complete;
    let delta_link = round.delta_link.clone();
    let instances = parse_items(round.items);
    let wanted = masters_to_fetch(&instances, &earlier, complete);
    let masters = fetch_masters(api, calendar, &wanted).await?;
    let (changes, masters) = batch(instances, masters, known, complete, &[me.to_string()], api.zone(), &earlier);
    let cursor = match delta_link {
        Some(link) => Cursor::Delta { link, month, masters },
        None => Cursor::Poll,
    };
    Ok(SyncBatch { changes, cursor: Some(serde_json::to_string(&cursor).expect("a cursor serializes")), refresh: None })
}

/// A full delta round over a new window: the calendar's own delta, else
/// the primary calendar's when this is it, else a poll from now on.
///
/// Delta's own fields leave out an exception's `originalStart`, its
/// recurrence id. The Graph docs say delta takes no `$select`, but Microsoft
/// 365 adds the fields it names, and the delta link keeps them for the
/// rounds that follow.
async fn first_round(api: &impl Api, calendar: &str, today: NaiveDate) -> Result<(Round, BTreeSet<String>)> {
    let (from, to) = window(today);
    let range = format!("startDateTime={from}&endDateTime={to}&$select={FIELDS}");
    if let Some(round) = pages(api, &format!("/me/calendars/{}/calendarView/delta?{range}", encode(calendar))).await? {
        return Ok((round, BTreeSet::new()));
    }
    let primary = get_ok(api, "/me/calendar?$select=id").await?;
    if primary["id"].as_str() == Some(calendar)
        && let Some(round) = pages(api, &format!("/me/calendarView/delta?{range}")).await?
    {
        return Ok((round, BTreeSet::new()));
    }
    tracing::info!("Graph has no delta for calendar {calendar}; polling its window");
    Ok((poll(api, calendar, today).await?, BTreeSet::new()))
}

async fn poll(api: &impl Api, calendar: &str, today: NaiveDate) -> Result<Round> {
    let (from, to) = window(today);
    let url = format!("/me/calendars/{}/calendarView?startDateTime={from}&endDateTime={to}&$select={FIELDS}", encode(calendar));
    let mut round = pages(api, &url).await?.ok_or_else(|| Error::Other(format!("Microsoft Graph refused the calendar view of {calendar}")))?;
    round.complete = true;
    round.delta_link = None;
    Ok(round)
}

/// Follows `@odata.nextLink` to the end. None when Graph refuses the
/// request as a delta it doesn't support or a delta link it no longer
/// knows; the caller starts over another way.
async fn pages(api: &impl Api, first: &str) -> Result<Option<Round>> {
    let mut round = Round { items: Vec::new(), complete: !first.contains("deltatoken"), delta_link: None };
    let mut next = Some(first.to_string());
    while let Some(url) = next {
        let (status, page) = api.request("GET", &url, None, &[]).await?;
        match status {
            200..300 => {}
            400 | 404 | 405 | 410 | 501 if round.items.is_empty() => {
                tracing::info!("Graph refused {url}: {status} {}", page["error"]["code"]);
                return Ok(None);
            }
            _ => return Err(api.failure(status, &page)),
        }
        round.items.extend(page["value"].as_array().cloned().unwrap_or_default());
        round.delta_link = page["@odata.deltaLink"].as_str().map(str::to_string);
        next = page["@odata.nextLink"].as_str().map(str::to_string);
    }
    Ok(Some(round))
}

async fn get_ok(api: &impl Api, url: &str) -> Result<Value> {
    let (status, json) = api.request("GET", url, None, &[]).await?;
    if !(200..300).contains(&status) {
        return Err(api.failure(status, &json));
    }
    Ok(json)
}

/// The masters by id, through JSON batches, with the fields only a GET by
/// id returns. A master Graph can't give maps to None.
async fn fetch_masters(api: &impl Api, calendar: &str, ids: &BTreeSet<String>) -> Result<Masters> {
    let mut out = Masters::new();
    let ids: Vec<&String> = ids.iter().collect();
    for chunk in ids.chunks(BATCH) {
        let mut pending: Vec<&String> = chunk.to_vec();
        for attempt in 0..4u32 {
            let requests: Vec<Value> = pending
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    json!({
                        "id": i.to_string(),
                        "method": "GET",
                        "url": format!("/me/calendars/{}/events/{}?$select={FIELDS},cancelledOccurrences", encode(calendar), encode(id)),
                        "headers": {"Prefer": PREFER},
                    })
                })
                .collect();
            let (status, reply) = api.request("POST", "/$batch", Some(&json!({ "requests": requests })), &[]).await?;
            if !(200..300).contains(&status) {
                return Err(api.failure(status, &reply));
            }
            let mut throttled = Vec::new();
            let mut wait = 0;
            for r in reply["responses"].as_array().into_iter().flatten() {
                let Some(id) = r["id"].as_str().and_then(|i| i.parse::<usize>().ok()).and_then(|i| pending.get(i)) else { continue };
                match r["status"].as_u64().unwrap_or(0) {
                    200..300 => {
                        let raw = serde_json::to_vec(&r["body"]).expect("JSON serializes");
                        let master = serde_json::from_value::<GraphEvent>(r["body"].clone()).ok().map(|m| (m, raw));
                        out.insert((*id).clone(), master);
                    }
                    429 | 503 | 504 if attempt < 3 => {
                        wait = wait.max(r["headers"]["Retry-After"].as_str().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2));
                        throttled.push(*id);
                    }
                    404 | 403 => {
                        out.insert((*id).clone(), None);
                    }
                    status => return Err(api.failure(status as u16, &r["body"])),
                }
            }
            if throttled.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(wait.min(120))).await;
            pending = throttled;
        }
    }
    Ok(out)
}

/// One occurrence of a series as Graph lists it.
pub(super) struct Instance {
    pub id: String,
    /// The series' change key as Graph lists it with the occurrence.
    pub change_key: Option<String>,
}

/// The occurrence of `master` that `rid` names, from the series' instances
/// around it. An all-day occurrence's original start is midnight in the
/// series' own zone.
pub(super) async fn instance(api: &impl Api, master: &Stored, rid: &Moment) -> Result<Instance> {
    let here = tz::here();
    let series: GraphEvent = master.raw.as_deref().and_then(|r| serde_json::from_slice(r).ok()).unwrap_or_default();
    let zone = series_zone(&series, api.zone());
    let at = match rid {
        Moment::Time(local) => local.instant(here),
        Moment::Date(day) => tz::instant(day.and_time(NaiveTime::MIN), zone),
    };
    let stamp = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let url = format!(
        "/me/events/{}/instances?startDateTime={}&endDateTime={}&$select=id,originalStart,changeKey",
        encode(&master.remote_id),
        stamp(at - chrono::Duration::hours(36)),
        stamp(at + chrono::Duration::hours(36)),
    );
    let (status, page) = api.request("GET", &url, None, &[]).await?;
    if !(200..300).contains(&status) {
        return Err(api.failure(status, &page));
    }
    page["value"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|i| {
            let Some(start) = i["originalStart"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()) else { return false };
            let start = start.to_utc();
            let original = match rid {
                Moment::Date(_) => Moment::Date(start.with_timezone(&zone).date_naive()),
                Moment::Time(_) => Moment::Time(Local { wall: start.naive_utc(), zone: Zone::Tz(chrono_tz::UTC) }),
            };
            original.names(rid, here)
        })
        .and_then(|i| Some(Instance { id: i["id"].as_str()?.to_string(), change_key: i["changeKey"].as_str().map(str::to_string) }))
        .ok_or_else(|| Error::NotFound("Microsoft 365 has no occurrence of this series at that time".into()))
}

/// Whether Graph's answer to a GET of an event says the organizer's
/// cancellation took it out of the calendar already: gone, or cancelled.
fn graph_gone(status: u16, event: &Value) -> bool {
    status == 404 || ((200..300).contains(&status) && event["isCancelled"] == true)
}

/// The delta window: from the first of this month 2 years back to 3 years
/// ahead, as Graph wants it.
fn window(today: NaiveDate) -> (String, String) {
    let first = today.with_day(1).expect("every month has a first");
    let from = first.checked_sub_months(Months::new(24)).expect("in range");
    let to = first.checked_add_months(Months::new(36)).expect("in range");
    let at = |d: NaiveDate| format!("{}T00:00:00Z", d.format("%Y-%m-%d"));
    (at(from), at(to))
}

fn remote_calendar(c: &Value, me: &str) -> RemoteCalendar {
    let owner = c["owner"]["address"].as_str().unwrap_or_default();
    RemoteCalendar {
        remote_id: c["id"].as_str().unwrap_or_default().to_string(),
        name: c["name"].as_str().unwrap_or_default().to_string(),
        color: c["hexColor"].as_str().filter(|h| h.starts_with('#')).map(str::to_string),
        zone: None,
        access: if owner.is_empty() || owner.eq_ignore_ascii_case(me) {
            Access::Owner
        } else if c["canEdit"].as_bool() == Some(true) {
            Access::Write
        } else {
            Access::Read
        },
        primary: c["isDefaultCalendar"].as_bool() == Some(true),
        refresh: None,
    }
}

/// The UID the invitation carried. Exchange keeps a UID from outside
/// Outlook, such as a Google invitation's, inside its Global Object ID as
/// "vCal-Uid", a version number and the UID ending in a zero byte
/// ([MS-OXCICAL] 2.1.3.1.1.20.26). Unpacked, the same meeting has the same
/// UID in a Google and a Microsoft calendar.
fn original_uid<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.map(|uid| unpack_uid(&uid).unwrap_or(uid)))
}

fn unpack_uid(goid: &str) -> Option<String> {
    let bytes = hex::decode(goid).ok()?;
    let marker = b"vCal-Uid";
    let at = bytes.windows(marker.len()).position(|w| w == marker)?;
    let rest = bytes.get(at + marker.len() + 4..)?;
    let uid = &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())];
    String::from_utf8(uid.to_vec()).ok().filter(|u| !u.is_empty())
}

fn parse_items(items: Vec<Value>) -> Vec<(GraphEvent, Vec<u8>)> {
    items
        .into_iter()
        .filter_map(|item| {
            let raw = serde_json::to_vec(&item).expect("JSON serializes");
            match serde_json::from_value::<GraphEvent>(item) {
                Ok(g) => Some((g, raw)),
                Err(e) => {
                    tracing::warn!("skipping a Graph event: {e}");
                    None
                }
            }
        })
        .collect()
}

/// The masters a round needs: every series with an instance in it, and
/// after a deletion delta can't place, every series the window held.
fn masters_to_fetch(instances: &[(GraphEvent, Vec<u8>)], earlier: &BTreeSet<String>, complete: bool) -> BTreeSet<String> {
    let mut wanted: BTreeSet<String> = instances.iter().filter_map(|(g, _)| g.series_master_id.clone()).collect();
    // A removed exception has to bring its master back too: the master's
    // cancelledOccurrences now holds that date, and without it the original
    // occurrence returns.
    let lost_occurrence = instances.iter().any(|(g, _)| g.removed.is_some());
    if !complete && lost_occurrence {
        wanted.extend(earlier.iter().cloned());
    }
    wanted
}

/// The changes of one round, and the series masters the window now holds.
fn batch(
    instances: Vec<(GraphEvent, Vec<u8>)>,
    masters: Masters,
    known: &Known,
    complete: bool,
    me: &[String],
    here: Tz,
    earlier: &BTreeSet<String>,
) -> (Vec<Change>, BTreeSet<String>) {
    let mut changes = Vec::new();
    let mut removed: BTreeSet<String> = BTreeSet::new();
    let mut present: BTreeSet<String> = BTreeSet::new();
    let referenced: BTreeSet<&str> = instances.iter().filter(|(g, _)| g.removed.is_none()).filter_map(|(g, _)| g.series_master_id.as_deref()).collect();
    let mut holding: BTreeSet<String> = if complete { BTreeSet::new() } else { earlier.clone() };

    for (id, master) in &masters {
        match master {
            Some((m, raw)) => {
                holding.insert(id.clone());
                if let Some(event) = m.ical_uid.clone().and_then(|uid| event(m, uid, None, me, here)) {
                    present.insert(id.clone());
                    changes.push(Change::Upsert { remote_id: id.clone(), etag: m.change_key.clone(), events: vec![event], raw: Some(raw.clone()) });
                }
            }
            None => {
                holding.remove(id);
                if !referenced.contains(id.as_str()) {
                    removed.insert(id.clone());
                }
            }
        }
    }

    for (g, raw) in instances {
        if g.removed.is_some() {
            removed.insert(g.id);
            continue;
        }
        let master = g.series_master_id.as_ref().map(|id| masters.get(id).and_then(Option::as_ref).map(|(m, _)| m));
        let mapped = match (g.kind.as_str(), master) {
            ("singleInstance", _) | ("seriesMaster", _) => g.ical_uid.clone().and_then(|uid| event(&g, uid, None, me, here)),
            ("exception", Some(Some(m))) => m.ical_uid.clone().and_then(|uid| event(&g, uid, Some(m), me, here)),
            ("occurrence", Some(Some(m))) if g.is_cancelled => m.ical_uid.clone().and_then(|uid| event(&g, uid, Some(m), me, here)),
            // An occurrence the master already produces. Stored before as an
            // exception or a stand-in, it goes.
            ("occurrence", Some(Some(_))) => {
                if known.contains_key(&g.id) {
                    removed.insert(g.id);
                }
                continue;
            }
            // The master can't be fetched: the instance stands alone.
            _ => g.ical_uid.clone().and_then(|uid| event(&GraphEvent { original_start: None, ..g.clone() }, uid, None, me, here)),
        };
        let Some(event) = mapped else {
            tracing::warn!("skipping Graph event {}: no UID or start", g.id);
            continue;
        };
        present.insert(g.id.clone());
        changes.push(Change::Upsert { remote_id: g.id, etag: g.change_key, events: vec![event], raw: Some(raw) });
    }

    if complete {
        removed.extend(vanished(known, &present).into_iter().filter_map(|c| match c {
            Change::Remove { remote_id } => Some(remote_id),
            Change::Upsert { .. } => None,
        }));
    }
    removed.retain(|id| !present.contains(id));
    changes.extend(removed.into_iter().map(|remote_id| Change::Remove { remote_id }));
    (changes, holding)
}

/// Graph's JSON for one event as `EventData`. `master` is set for an
/// exception or cancelled occurrence: its recurrence id is placed on the
/// master's clock. None without a start.
fn event(g: &GraphEvent, uid: String, master: Option<&GraphEvent>, me: &[String], here: Tz) -> Option<EventData> {
    let start_zone = zone_of(g.original_start_time_zone.as_deref(), here);
    let end_zone = g.original_end_time_zone.as_deref().and_then(tz::zone).unwrap_or(start_zone);
    let start = instant(g.start.as_ref()?)?;
    let end = g.end.as_ref().and_then(instant).unwrap_or(start);
    let wall = |at: DateTime<Utc>, z: Tz| tz::wall(at, Zone::Tz(z), here);
    let span = if g.is_all_day {
        let s = wall(start, start_zone).date();
        Span::AllDay { start: s, end: wall(end, start_zone).date().max(s.succ_opt()?) }
    } else {
        Span::Timed {
            start: Local { wall: wall(start, start_zone), zone: Zone::Tz(start_zone) },
            end: Local { wall: wall(end, end_zone), zone: Zone::Tz(end_zone) },
        }
    };
    let series = master.unwrap_or(g);
    let series_zone = series_zone(series, here);
    let recurrence_id = g.original_start.filter(|_| g.kind != "seriesMaster").map(|at| {
        let at = at.to_utc();
        if series.is_all_day {
            Moment::Date(wall(at, series_zone).date())
        } else {
            Moment::Time(Local { wall: wall(at, series_zone), zone: Zone::Tz(series_zone) })
        }
    });
    let recurrence = match (&g.recurrence, g.kind.as_str()) {
        (Some(p), "seriesMaster") => {
            let until_zone = p.range.recurrence_time_zone.as_deref().and_then(tz::zone).unwrap_or(start_zone);
            match rrule(p, g.is_all_day, until_zone) {
                Some(rule) => Some(Recurrence { rules: vec![rule], rdates: Vec::new(), exdates: cancelled(&g.cancelled_occurrences) }),
                None => {
                    tracing::warn!("Graph event {}: a repeat pattern Jolta doesn't know ({})", g.id, p.pattern.kind);
                    None
                }
            }
        }
        _ => None,
    };

    let own = g.response_status.as_ref().map(|s| response(&s.response)).filter(|r| *r != Response::NeedsAction);
    let is_me = |a: &str| me.iter().any(|m| m.eq_ignore_ascii_case(a));
    let mut attendees: Vec<Attendee> = g
        .attendees
        .iter()
        .map(|a| {
            let me = is_me(&a.email_address.address);
            let theirs = a.status.as_ref().map_or(Response::NeedsAction, |s| response(&s.response));
            Attendee {
                person: Person { address: a.email_address.address.clone(), name: a.email_address.name.clone() },
                role: match a.kind.as_str() {
                    "optional" => Role::Optional,
                    "resource" => Role::Resource,
                    _ => Role::Required,
                },
                response: if me { own.unwrap_or(theirs) } else { theirs },
                comment: String::new(),
                is_me: me,
            }
        })
        .collect();
    // Invited through a group, the user isn't on the list but has answered.
    if !g.is_organizer
        && !attendees.iter().any(|a| a.is_me)
        && let (Some(response), Some(address)) = (own, me.first())
    {
        attendees.push(Attendee {
            person: Person { address: address.clone(), name: String::new() },
            role: Role::Required,
            response,
            comment: String::new(),
            is_me: true,
        });
    }

    let (description, description_html) = match &g.body {
        Some(b) => (b.content.clone(), b.content_type.eq_ignore_ascii_case("html")),
        None => (String::new(), false),
    };
    let location = g.location.as_ref().map(|l| l.display_name.clone()).unwrap_or_default();
    let meeting = meeting::detect(&entries(g), None, &location, &description);
    Some(EventData {
        uid,
        recurrence_id,
        this_and_future: false,
        span,
        recurrence,
        title: g.subject.clone().unwrap_or_default(),
        location,
        description,
        description_html,
        url: None,
        status: if g.is_cancelled { EventStatus::Cancelled } else { EventStatus::Confirmed },
        show_as: match g.show_as.as_deref() {
            Some("free") => ShowAs::Free,
            Some("tentative") => ShowAs::Tentative,
            Some("oof") => ShowAs::OutOfOffice,
            Some("workingElsewhere") => ShowAs::Elsewhere,
            _ => ShowAs::Busy,
        },
        visibility: match g.sensitivity.as_deref() {
            Some("personal") | Some("private") => Visibility::Private,
            Some("confidential") => Visibility::Confidential,
            _ => Visibility::Default,
        },
        organizer: g
            .organizer
            .as_ref()
            .filter(|o| !o.email_address.address.is_empty())
            .map(|o| Person { address: o.email_address.address.clone(), name: o.email_address.name.clone() }),
        attendees,
        sequence: 0,
        alarms: if g.is_reminder_on {
            vec![Alarm { trigger: AlarmTrigger::MinutesBefore(g.reminder_minutes_before_start), email: false }]
        } else {
            Vec::new()
        },
        meeting,
        categories: g.categories.clone(),
        color: None,
        master_id: g.series_master_id.clone(),
    })
}

fn zone_of(name: Option<&str>, here: Tz) -> Tz {
    name.and_then(tz::zone).unwrap_or(here)
}

/// The zone of a series' original starts; an all-day series' are midnight
/// there. Graph writes an all-day series' own zone as UTC, so the rule's
/// `recurrenceTimeZone` comes first.
fn series_zone(series: &GraphEvent, here: Tz) -> Tz {
    let rule = series.recurrence.as_ref().and_then(|r| r.range.recurrence_time_zone.as_deref());
    zone_of(rule.or(series.original_start_time_zone.as_deref()), here)
}

/// A `dateTimeTimeZone` as an instant. Jolta asks for UTC; a reply in
/// another zone is read in that zone.
fn instant(w: &When) -> Option<DateTime<Utc>> {
    let text = w.date_time.get(..19)?;
    let wall = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").ok()?;
    Some(tz::instant(wall, tz::zone(&w.time_zone).unwrap_or(chrono_tz::UTC)))
}

fn response(r: &str) -> Response {
    match r {
        "accepted" | "organizer" => Response::Accepted,
        "tentativelyAccepted" => Response::Tentative,
        "declined" => Response::Declined,
        _ => Response::NeedsAction,
    }
}

/// `cancelledOccurrences` holds occurrence ids that end in the original
/// date (`OID.<master id>.2026-11-10`).
fn cancelled(ids: &[String]) -> Vec<Moment> {
    ids.iter()
        .filter_map(|id| id.rsplit('.').next())
        .filter_map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .map(Moment::Date)
        .collect()
}

fn entries(g: &GraphEvent) -> Vec<Entry> {
    let Some(m) = &g.online_meeting else { return Vec::new() };
    let video = m.join_url.iter().map(|uri| Entry { kind: EntryKind::Video, uri: uri.clone(), meeting_id: None, passcode: None });
    let numbers = m.toll_number.iter().chain(m.phones.iter().map(|p| &p.number)).filter(|n| !n.is_empty());
    let phones = numbers.map(|n| Entry {
        kind: EntryKind::Phone,
        uri: format!("tel:{}", n.replace(' ', "")),
        meeting_id: None,
        passcode: m.conference_id.clone(),
    });
    video.chain(phones).collect()
}

/// Graph's `patternedRecurrence` as an RRULE. An end date is the end of
/// that day in the series' `recurrenceTimeZone`, as UNTIL in UTC; all-day
/// series take it as a date. None for a pattern type Graph doesn't document.
pub fn rrule(p: &PatternedRecurrence, all_day: bool, until_zone: Tz) -> Option<String> {
    let pattern = &p.pattern;
    let days = || pattern.days_of_week.iter().filter_map(|d| weekday(d)).collect::<Vec<_>>().join(",");
    let setpos = || match pattern.index.as_deref().unwrap_or("first") {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        _ => -1,
    };
    let mut parts = match pattern.kind.as_str() {
        "daily" => vec!["FREQ=DAILY".to_string()],
        "weekly" => {
            let mut v = vec!["FREQ=WEEKLY".to_string(), format!("BYDAY={}", days())];
            if let Some(wkst) = pattern.first_day_of_week.as_deref().and_then(weekday) {
                v.push(format!("WKST={wkst}"));
            }
            v
        }
        // Exchange moves a day the month lacks to its last day; RFC 5545
        // skips the month, so the 29th to 31st fall back to -1.
        "absoluteMonthly" if pattern.day_of_month >= 29 => {
            vec!["FREQ=MONTHLY".into(), format!("BYMONTHDAY={},-1", pattern.day_of_month), "BYSETPOS=1".into()]
        }
        "absoluteMonthly" => vec!["FREQ=MONTHLY".into(), format!("BYMONTHDAY={}", pattern.day_of_month)],
        "relativeMonthly" => vec!["FREQ=MONTHLY".into(), format!("BYDAY={}", days()), format!("BYSETPOS={}", setpos())],
        "absoluteYearly" => {
            vec!["FREQ=YEARLY".into(), format!("BYMONTH={}", pattern.month), format!("BYMONTHDAY={}", pattern.day_of_month)]
        }
        "relativeYearly" => vec![
            "FREQ=YEARLY".into(),
            format!("BYMONTH={}", pattern.month),
            format!("BYDAY={}", days()),
            format!("BYSETPOS={}", setpos()),
        ],
        _ => return None,
    };
    if pattern.interval > 1 {
        parts.insert(1, format!("INTERVAL={}", pattern.interval));
    }
    match (p.range.kind.as_str(), p.range.end_date) {
        ("numbered", _) if p.range.number_of_occurrences > 0 => parts.push(format!("COUNT={}", p.range.number_of_occurrences)),
        ("endDate", Some(day)) if all_day => parts.push(format!("UNTIL={}", day.format("%Y%m%d"))),
        ("endDate", Some(day)) => {
            let end = tz::instant(day.and_time(NaiveTime::from_hms_opt(23, 59, 59).expect("a valid time")), until_zone);
            parts.push(format!("UNTIL={}", end.format("%Y%m%dT%H%M%SZ")));
        }
        _ => {}
    }
    Some(parts.join(";"))
}

fn weekday(day: &str) -> Option<&'static str> {
    Some(match day.to_ascii_lowercase().as_str() {
        "monday" => "MO",
        "tuesday" => "TU",
        "wednesday" => "WE",
        "thursday" => "TH",
        "friday" => "FR",
        "saturday" => "SA",
        "sunday" => "SU",
        _ => return None,
    })
}

/// Whether the organizer takes proposals for a new time, from an event's
/// JSON as Graph gave it: `allowNewTimeProposals`, true when it is missing,
/// as in copies stored before Jolta asked for it.
pub fn takes_proposals(raw: &[u8]) -> bool {
    serde_json::from_slice::<Value>(raw).ok().and_then(|v| v["allowNewTimeProposals"].as_bool()).unwrap_or(true)
}

/// The body of an answer: Graph refuses a comment, even an empty one, and
/// a proposed time without `sendResponse`.
fn answer_body(rsvp: &Rsvp) -> Value {
    let mut body = json!({ "sendResponse": rsvp.notify });
    if let Some(note) = rsvp.note.as_ref().filter(|_| rsvp.notify) {
        body["comment"] = json!(note);
    }
    if let Some(p) = rsvp.proposal.filter(|_| rsvp.notify && rsvp.answer != Answer::Accept) {
        let slot = |t: DateTime<Utc>| json!({ "dateTime": t.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" });
        body["proposedNewTime"] = json!({ "start": slot(p.start), "end": slot(p.end) });
    }
    body
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::{America, Europe};

    use super::*;
    use crate::calendar::MeetingProvider;
    use crate::calendar::recur::{self, Window};

    /// Where CLDR places Windows' `Romance Standard Time`.
    const ROMANCE: Tz = Europe::Paris;

    fn pattern(v: Value) -> PatternedRecurrence {
        serde_json::from_value(v).unwrap()
    }

    fn rule(v: Value) -> String {
        rrule(&pattern(v), false, Europe::Brussels).unwrap()
    }

    #[test]
    fn every_pattern_type_becomes_an_rrule() {
        let no_end = json!({"type": "noEnd", "startDate": "2026-11-02", "endDate": "0001-01-01", "recurrenceTimeZone": "Romance Standard Time"});
        let cases = [
            (json!({"type": "daily", "interval": 2}), "FREQ=DAILY;INTERVAL=2"),
            (json!({"type": "weekly", "interval": 1, "daysOfWeek": ["monday", "wednesday"], "firstDayOfWeek": "sunday"}), "FREQ=WEEKLY;BYDAY=MO,WE;WKST=SU"),
            (json!({"type": "absoluteMonthly", "interval": 3, "dayOfMonth": 31}), "FREQ=MONTHLY;INTERVAL=3;BYMONTHDAY=31,-1;BYSETPOS=1"),
            (json!({"type": "absoluteMonthly", "interval": 1, "dayOfMonth": 15}), "FREQ=MONTHLY;BYMONTHDAY=15"),
            (json!({"type": "relativeMonthly", "interval": 1, "daysOfWeek": ["friday"], "index": "last"}), "FREQ=MONTHLY;BYDAY=FR;BYSETPOS=-1"),
            (
                json!({"type": "relativeMonthly", "interval": 1, "daysOfWeek": ["monday", "tuesday", "wednesday", "thursday", "friday"], "index": "first"}),
                "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1",
            ),
            (json!({"type": "absoluteYearly", "interval": 1, "month": 2, "dayOfMonth": 29}), "FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=29"),
            (json!({"type": "relativeYearly", "interval": 1, "month": 11, "daysOfWeek": ["thursday"], "index": "fourth"}), "FREQ=YEARLY;BYMONTH=11;BYDAY=TH;BYSETPOS=4"),
        ];
        for (p, want) in cases {
            assert_eq!(rule(json!({"pattern": p, "range": no_end})), want, "{p}");
        }
        assert_eq!(rrule(&pattern(json!({"pattern": {"type": "hourly"}, "range": no_end})), false, Europe::Brussels), None);
    }

    #[test]
    fn a_monthly_day_the_month_lacks_falls_on_its_last_day_as_in_exchange() {
        let master = mapped(master_json(json!({"type": "absoluteMonthly", "interval": 1, "dayOfMonth": 31}), json!({"type": "noEnd"})));
        let master = EventData { span: Span::Timed { start: Local { wall: chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap().and_hms_opt(9, 0, 0).unwrap(), zone: Zone::Tz(Europe::Brussels) }, end: Local { wall: chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap().and_hms_opt(10, 0, 0).unwrap(), zone: Zone::Tz(Europe::Brussels) } }, ..master };
        assert_eq!(starts(&master, &[], (2026, 10), 5), ["2026-10-31 09:00", "2026-11-30 09:00", "2026-12-31 09:00", "2027-01-31 09:00", "2027-02-28 09:00"]);
    }

    #[test]
    fn an_outlook_id_holding_a_google_uid_gives_the_google_uid() {
        let packed = "040000008200E00074C5B7101A82E00800000000000000000000000000000000000000002B0000007643616C2D55696401000000\
                      61626331323340676F6F676C652E636F6D00";
        assert_eq!(unpack_uid(packed).as_deref(), Some("abc123@google.com"));
        let outlook = "040000008200E00074C5B7101A82E0080000000080B7E1E3F0A3DC01000000000000000010000000AB2D1E8C9F3B7A4C8E5D6F7A8B9C0D1E";
        assert_eq!(unpack_uid(outlook), None, "an Outlook meeting keeps its own id");
        let g: GraphEvent = serde_json::from_value(json!({"id": "E", "iCalUId": packed})).unwrap();
        assert_eq!(g.ical_uid.as_deref(), Some("abc123@google.com"));
    }

    #[test]
    fn a_removed_exception_fetches_its_master_again() {
        let earlier = BTreeSet::from(["M1".to_string()]);
        let instances = vec![graph(json!({"@removed": {"reason": "deleted"}, "id": "X1"}))];
        assert_eq!(masters_to_fetch(&instances, &earlier, false), earlier, "the master now cancels that occurrence");
    }

    #[test]
    fn every_range_type() {
        let daily = json!({"type": "daily", "interval": 1});
        assert_eq!(rule(json!({"pattern": daily, "range": {"type": "numbered", "numberOfOccurrences": 5}})), "FREQ=DAILY;COUNT=5");
        assert_eq!(rule(json!({"pattern": daily, "range": {"type": "noEnd"}})), "FREQ=DAILY");
        let ends = json!({"pattern": daily, "range": {"type": "endDate", "endDate": "2026-11-24", "recurrenceTimeZone": "Romance Standard Time"}});
        assert_eq!(rule(ends.clone()), "FREQ=DAILY;UNTIL=20261124T225959Z", "the end of 24 November in Brussels, in UTC");
        assert_eq!(rrule(&pattern(ends), true, Europe::Brussels).unwrap(), "FREQ=DAILY;UNTIL=20261124", "all-day series end on a date");
        let ny = json!({"pattern": daily, "range": {"type": "endDate", "endDate": "2026-11-24"}});
        assert_eq!(rrule(&pattern(ny), false, America::New_York).unwrap(), "FREQ=DAILY;UNTIL=20261125T045959Z");
    }

    /// The occurrence starts of `master` in a window, on its own clock.
    fn starts(master: &EventData, overrides: &[EventData], from: (i32, u32), months: u32) -> Vec<String> {
        let start = Utc.with_ymd_and_hms(from.0, from.1, 1, 0, 0, 0).unwrap();
        let end = start.checked_add_months(Months::new(months)).unwrap();
        recur::expand(master, overrides, Window { start, end }, Europe::Brussels)
            .into_iter()
            .map(|o| match o.span {
                Span::Timed { start, .. } => start.wall.format("%Y-%m-%d %H:%M").to_string(),
                Span::AllDay { start, .. } => start.to_string(),
            })
            .collect()
    }

    fn graph(v: Value) -> (GraphEvent, Vec<u8>) {
        (serde_json::from_value(v.clone()).unwrap(), serde_json::to_vec(&v).unwrap())
    }

    fn mapped(v: Value) -> EventData {
        let (g, _) = graph(v);
        let uid = g.ical_uid.clone().unwrap();
        event(&g, uid, None, &["me@example.com".into()], Europe::Brussels).unwrap()
    }

    fn master_json(pattern: Value, range: Value) -> Value {
        json!({
            "id": "M1", "changeKey": "ck1", "type": "seriesMaster", "iCalUId": "040000008200E00074C5B7101A82E008SERIES",
            "subject": "Review", "isAllDay": false,
            "start": {"dateTime": "2026-11-27T08:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-11-27T09:00:00.0000000", "timeZone": "UTC"},
            "originalStartTimeZone": "Romance Standard Time", "originalEndTimeZone": "Romance Standard Time",
            "recurrence": {"pattern": pattern, "range": range},
            "showAs": "busy", "isReminderOn": true, "reminderMinutesBeforeStart": 15,
            "cancelledOccurrences": ["OID.M1.2027-01-29"]
        })
    }

    #[test]
    fn last_friday_series_expands_on_brussels_time_without_its_cancelled_occurrence() {
        let master = mapped(master_json(
            json!({"type": "relativeMonthly", "interval": 1, "daysOfWeek": ["friday"], "index": "last", "firstDayOfWeek": "sunday"}),
            json!({"type": "numbered", "numberOfOccurrences": 5, "startDate": "2026-11-27", "recurrenceTimeZone": "Romance Standard Time"}),
        ));
        assert_eq!(master.span, Span::Timed {
            start: Local { wall: NaiveDateTime::parse_from_str("2026-11-27 09:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(ROMANCE) },
            end: Local { wall: NaiveDateTime::parse_from_str("2026-11-27 10:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(ROMANCE) },
        });
        assert_eq!(master.alarms, [Alarm { trigger: AlarmTrigger::MinutesBefore(15), email: false }]);
        assert_eq!(
            starts(&master, &[], (2026, 11), 6),
            ["2026-11-27 09:00", "2026-12-25 09:00", "2027-02-26 09:00", "2027-03-26 09:00"],
            "29 January is cancelled; the last Friday of March stays at 09:00 after the clock change"
        );
    }

    #[test]
    fn series_ending_on_a_date_in_romance_standard_time_stops_at_midnight_in_brussels() {
        let master = mapped(master_json(
            json!({"type": "daily", "interval": 1}),
            json!({"type": "endDate", "startDate": "2026-11-21", "endDate": "2026-11-24", "recurrenceTimeZone": "Romance Standard Time"}),
        ));
        let at = |s: &str| Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(ROMANCE) };
        let master = EventData {
            span: Span::Timed { start: at("2026-11-21 00:30"), end: at("2026-11-21 01:00") },
            recurrence: Some(Recurrence { exdates: Vec::new(), ..master.recurrence.unwrap() }),
            ..master
        };
        assert_eq!(
            starts(&master, &[], (2026, 11), 2),
            ["2026-11-21 00:30", "2026-11-22 00:30", "2026-11-23 00:30", "2026-11-24 00:30"],
            "25 November 00:30 in Brussels is still 24 November in UTC, and must not count"
        );
    }

    #[test]
    fn exchange_has_acted_on_a_cancellation_when_the_event_is_gone_or_cancelled() {
        assert!(graph_gone(404, &Value::Null));
        assert!(graph_gone(200, &json!({"isCancelled": true})));
        assert!(!graph_gone(200, &json!({"isCancelled": false})));
        assert!(!graph_gone(403, &json!({"isCancelled": true})), "an error says nothing");
    }

    #[test]
    fn exception_and_cancelled_occurrence_join_the_masters_series() {
        let (master, master_raw) = graph(master_json(
            json!({"type": "weekly", "interval": 1, "daysOfWeek": ["friday"]}),
            json!({"type": "noEnd", "startDate": "2026-11-27", "recurrenceTimeZone": "Romance Standard Time"}),
        ));
        let exception = json!({
            "id": "X1", "changeKey": "ckx", "type": "exception", "seriesMasterId": "M1", "iCalUId": "per-instance-uid-1",
            "subject": "Review, moved", "originalStart": "2026-12-04T08:00:00Z", "originalStartTimeZone": "Romance Standard Time",
            "start": {"dateTime": "2026-12-04T13:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-12-04T14:00:00.0000000", "timeZone": "UTC"},
            "attendees": [
                {"type": "required", "status": {"response": "none"}, "emailAddress": {"name": "Me", "address": "me@example.com"}},
                {"type": "resource", "status": {"response": "accepted"}, "emailAddress": {"name": "Room 1", "address": "room1@example.com"}},
                {"type": "optional", "status": {"response": "declined"}, "emailAddress": {"name": "Ann", "address": "ann@example.com"}}
            ],
            "responseStatus": {"response": "tentativelyAccepted"},
            "organizer": {"emailAddress": {"name": "Boss", "address": "boss@example.com"}},
            "onlineMeeting": {"joinUrl": "https://teams.microsoft.com/l/meetup-join/19%3ameeting_abc%40thread.v2/0", "conferenceId": "123456789", "tollNumber": "+32 2 555 01 00"},
            "onlineMeetingProvider": "teamsForBusiness",
            "body": {"contentType": "html", "content": "<p>Agenda</p>"},
            "sensitivity": "private", "showAs": "tentative", "categories": ["Blue category"]
        });
        let cancelled = json!({
            "id": "O2", "changeKey": "cko", "type": "occurrence", "seriesMasterId": "M1", "iCalUId": "per-instance-uid-2",
            "isCancelled": true, "originalStart": "2026-12-11T08:00:00Z", "originalStartTimeZone": "Romance Standard Time",
            "start": {"dateTime": "2026-12-11T08:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-12-11T09:00:00.0000000", "timeZone": "UTC"}
        });
        let plain = json!({"id": "O3", "type": "occurrence", "seriesMasterId": "M1", "iCalUId": "per-instance-uid-3",
            "originalStart": "2026-12-18T08:00:00Z", "start": {"dateTime": "2026-12-18T08:00:00.0000000", "timeZone": "UTC"}});
        let masters: Masters = [("M1".to_string(), Some((master, master_raw)))].into();
        let instances = vec![graph(exception), graph(cancelled), graph(plain)];
        let (changes, holding) = batch(instances, masters, &Known::new(), true, &["me@example.com".into()], Europe::Brussels, &BTreeSet::new());
        assert_eq!(holding, BTreeSet::from(["M1".to_string()]));
        let events: BTreeMap<&str, &EventData> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Upsert { remote_id, events, .. } => Some((remote_id.as_str(), &events[0])),
                Change::Remove { .. } => None,
            })
            .collect();
        assert_eq!(events.keys().copied().collect::<Vec<_>>(), ["M1", "O2", "X1"], "the plain occurrence is left to expansion");

        let x = events["X1"];
        assert_eq!(x.uid, "040000008200E00074C5B7101A82E008SERIES", "the master's iCalUId, not the instance's");
        let brussels = |s: &str| Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(ROMANCE) };
        assert_eq!(x.recurrence_id, Some(Moment::Time(brussels("2026-12-04 09:00"))));
        assert_eq!((events["M1"].master_id.as_deref(), x.master_id.as_deref()), (None, Some("M1")), "an exception names the series Graph files it under");
        assert_eq!(x.span, Span::Timed { start: brussels("2026-12-04 14:00"), end: brussels("2026-12-04 15:00") });
        assert_eq!(x.my_response(), Some(Response::Tentative), "the event's own answer wins over the attendee list");
        let roles: Vec<(Role, Response)> = x.attendees.iter().map(|a| (a.role, a.response)).collect();
        assert_eq!(roles, [(Role::Required, Response::Tentative), (Role::Resource, Response::Accepted), (Role::Optional, Response::Declined)]);
        assert_eq!((x.show_as, x.visibility, x.description_html), (ShowAs::Tentative, Visibility::Private, true));
        assert_eq!(x.categories, ["Blue category"]);
        let m = x.meeting.as_ref().expect("a Teams meeting");
        assert_eq!(m.provider, MeetingProvider::Teams);
        assert_eq!(m.phones.len(), 1);

        let o = events["O2"];
        assert_eq!((o.status, o.recurrence_id), (EventStatus::Cancelled, Some(Moment::Time(brussels("2026-12-11 09:00")))));

        let master = events["M1"];
        let overrides = [x.clone(), o.clone()];
        assert_eq!(
            starts(master, &overrides, (2026, 11), 2),
            ["2026-11-27 09:00", "2026-12-04 14:00", "2026-12-18 09:00", "2026-12-25 09:00"]
        );
    }

    /// Microsoft 365 as recorded on 2026-11-05: a weekly series at 10:00 in
    /// Brussels whose 17 November occurrence was moved to 15:00. Delta leaves
    /// `originalStart` out of an exception unless `$select` names it.
    struct Recorded;

    impl Api for Recorded {
        async fn request(&self, method: &str, url: &str, body: Option<&Value>, _: &[(&str, &str)]) -> Result<(u16, Value)> {
            let master = json!({
                "id": "M1", "changeKey": "ck1", "type": "seriesMaster", "iCalUId": "040000008200E00074C5B7101A82E008SERIES",
                "subject": "Weekly probe", "isAllDay": false, "seriesMasterId": null, "occurrenceId": null,
                "start": {"dateTime": "2026-11-10T09:00:00.0000000", "timeZone": "UTC"},
                "end": {"dateTime": "2026-11-10T09:30:00.0000000", "timeZone": "UTC"},
                "originalStartTimeZone": "Romance Standard Time", "originalEndTimeZone": "Romance Standard Time",
                "recurrence": {
                    "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["tuesday"], "firstDayOfWeek": "monday"},
                    "range": {"type": "numbered", "startDate": "2026-11-10", "numberOfOccurrences": 4, "recurrenceTimeZone": "Romance Standard Time"}
                },
                "isOrganizer": true, "isReminderOn": false, "showAs": "busy"
            });
            if method == "POST" && url == "/$batch" {
                let mut by_id = master.clone();
                by_id["cancelledOccurrences"] = json!([]);
                let responses: Vec<Value> =
                    body.unwrap()["requests"].as_array().unwrap().iter().map(|r| json!({"id": r["id"], "status": 200, "body": by_id})).collect();
                return Ok((200, json!({ "responses": responses })));
            }
            assert!(url.contains("/calendarView/delta?"), "only delta is asked: {url}");
            let mut exception = json!({
                "id": "X1", "changeKey": "ckx", "type": "exception", "seriesMasterId": "M1",
                "iCalUId": "040000008200E00074C5B7101A82E00807EA0B11INSTANCE", "occurrenceId": "OID.M1.2026-11-17",
                "subject": "Weekly probe", "isAllDay": false, "isCancelled": false,
                "start": {"dateTime": "2026-11-17T14:00:00.0000000", "timeZone": "UTC"},
                "end": {"dateTime": "2026-11-17T14:30:00.0000000", "timeZone": "UTC"},
                "originalStartTimeZone": "Romance Standard Time", "originalEndTimeZone": "Romance Standard Time",
                "isOrganizer": true, "isReminderOn": false, "showAs": "busy"
            });
            let selected = url.split(['?', '&']).find_map(|p| p.strip_prefix("$select=")).unwrap_or_default();
            if selected.split(',').any(|f| f == "originalStart") {
                exception["originalStart"] = json!("2026-11-17T09:00:00Z");
            }
            Ok((200, json!({ "value": [master, exception], "@odata.deltaLink": "https://graph.microsoft.com/v1.0/delta?$deltatoken=t1" })))
        }

        fn failure(&self, status: u16, _: &Value) -> Error {
            Error::Other(format!("Microsoft Graph {status}"))
        }

        fn zone(&self) -> Tz {
            Europe::Brussels
        }
    }

    #[tokio::test]
    async fn a_moved_occurrence_from_delta_replaces_its_series_occurrence() {
        let batch = read(&Recorded, "C1", None, &Known::new(), "me@example.com").await.unwrap();
        let events: BTreeMap<String, EventData> = batch
            .changes
            .into_iter()
            .filter_map(|c| match c {
                Change::Upsert { remote_id, mut events, .. } => Some((remote_id, events.remove(0))),
                Change::Remove { .. } => None,
            })
            .collect();
        let (master, moved) = (&events["M1"], &events["X1"]);
        let brussels = |s: &str| Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(ROMANCE) };
        assert_eq!((moved.uid.as_str(), moved.recurrence_id.clone()), (master.uid.as_str(), Some(Moment::Time(brussels("2026-11-17 10:00")))));
        assert_eq!(
            starts(master, std::slice::from_ref(moved), (2026, 11), 1),
            ["2026-11-10 10:00", "2026-11-17 15:00", "2026-11-24 10:00"],
            "17 November shows once, at its new time"
        );
    }

    /// Microsoft 365 after `Recorded`'s round: a calendar view or the next
    /// delta round lists `listing`, and a GET of a series by id answers
    /// `master`.
    struct Later {
        listing: Vec<Value>,
        master: u16,
    }

    impl Api for Later {
        async fn request(&self, method: &str, url: &str, body: Option<&Value>, _: &[(&str, &str)]) -> Result<(u16, Value)> {
            if method == "POST" && url == "/$batch" {
                let responses: Vec<Value> = body.unwrap()["requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| json!({"id": r["id"], "status": self.master, "body": {"error": {"code": "refused"}}}))
                    .collect();
                return Ok((200, json!({ "responses": responses })));
            }
            Ok((200, json!({ "value": self.listing, "@odata.deltaLink": "https://graph.microsoft.com/v1.0/delta?$deltatoken=t2" })))
        }

        fn failure(&self, status: u16, _: &Value) -> Error {
            Error::Other(format!("Microsoft Graph {status}"))
        }

        fn zone(&self) -> Tz {
            Europe::Brussels
        }
    }

    /// A store holding `Recorded`'s first round: a weekly series whose 17
    /// November occurrence moved to 15:00.
    struct Synced {
        store: crate::store::Store,
        calendar: crate::calendar::CalendarId,
        cursor: Option<String>,
        /// The moved occurrence as delta lists it.
        moved: Value,
    }

    impl Synced {
        async fn new(name: &str) -> Synced {
            let dir = std::env::temp_dir().join(format!("jolta-graph-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store = crate::store::Store::open(&dir.join("jolta.sqlite")).unwrap();
            let remote = RemoteCalendar { remote_id: "C1".into(), name: "Calendar".into(), color: None, zone: None, access: Access::Owner, primary: true, refresh: None };
            let calendar = store
                .write(|tx| {
                    tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (1, 'graph', 'me@example.com', 'me', 'a1', 0, 1)")?;
                    crate::store::sync_calendar_list(tx, crate::AccountId(1), crate::calendar::CalendarSource::Graph, &[remote])
                })
                .unwrap()[0]
                .id;
            let mut synced = Synced { store, calendar, cursor: None, moved: Value::Null };
            synced.cursor = synced.round(&Recorded, None).await;
            synced
        }

        /// Syncs one round and gives the cursor it ends on.
        async fn round(&mut self, api: &impl Api, cursor: Option<&str>) -> Option<String> {
            let known = self.store.read(|c| crate::store::known_items(c, self.calendar)).unwrap();
            let batch = read(api, "C1", cursor, &known, "me@example.com").await.unwrap();
            let changes: Vec<(Change, Option<String>)> = batch.changes.into_iter().map(|c| (c, None)).collect();
            if let Some(raw) = changes.iter().find_map(|(c, _)| match c {
                Change::Upsert { remote_id, raw: Some(raw), .. } if remote_id == "X1" => Some(raw),
                _ => None,
            }) {
                self.moved = serde_json::from_slice(raw).unwrap();
            }
            let horizon = crate::calendar::series::Horizon::around(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(), Europe::Brussels);
            self.store.write(|tx| crate::calendar::sync::apply(tx, self.calendar, &changes, &horizon)).unwrap();
            batch.cursor
        }

        /// The start of every occurrence in November, in Brussels.
        fn november(&self) -> Vec<String> {
            let from = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
            self.store
                .read(|c| crate::store::occurrences(c, from, from.checked_add_months(Months::new(1)).unwrap(), Europe::Brussels))
                .unwrap()
                .into_iter()
                .map(|o| match o.bounds {
                    crate::calendar::Bounds::Utc { start, .. } => start.with_timezone(&Europe::Brussels).format("%m-%d %H:%M").to_string(),
                    crate::calendar::Bounds::Days { start, .. } => start.to_string(),
                })
                .collect()
        }
    }

    #[tokio::test]
    async fn a_full_round_keeps_an_exception_it_returns_when_its_series_is_refused() {
        let mut s = Synced::new("refused-full").await;
        assert_eq!(s.november(), ["11-10 10:00", "11-17 15:00", "11-24 10:00"]);
        s.round(&Later { listing: vec![s.moved.clone()], master: 403 }, None).await;
        assert_eq!(s.november(), ["11-17 15:00"], "the series Graph won't give goes; the moved occurrence the round returned stays");
    }

    #[tokio::test]
    async fn a_series_graph_only_refuses_leaves_its_moved_occurrence_in_place() {
        let mut s = Synced::new("refused-delta").await;
        let cursor = s.cursor.clone();
        let elsewhere = json!({"@removed": {"reason": "deleted"}, "id": "an-occurrence"});
        s.round(&Later { listing: vec![elsewhere], master: 403 }, cursor.as_deref()).await;
        assert_eq!(s.november(), ["11-17 15:00"], "a refusal says nothing about the moved occurrence, which the round didn't name");
    }

    #[tokio::test]
    async fn a_deleted_series_keeps_an_occurrence_the_same_round_returns() {
        let mut s = Synced::new("deleted").await;
        let cursor = s.cursor.clone();
        let deleted = json!({"@removed": {"reason": "deleted"}, "id": "M1"});
        s.round(&Later { listing: vec![s.moved.clone(), deleted], master: 404 }, cursor.as_deref()).await;
        assert_eq!(s.november(), ["11-17 15:00"], "the round listed the moved occurrence, so it stays");
    }

    /// A weekly all-day series from Brussels as Microsoft 365 recorded it on
    /// 2026-11-05: its own zone is UTC, its original starts are midnight on
    /// the clock of `recurrenceTimeZone`.
    fn all_day_series() -> Value {
        json!({
            "id": "M2", "changeKey": "ck2", "type": "seriesMaster", "iCalUId": "040000008200E00074C5B7101A82E008ALLDAY",
            "subject": "Day off", "isAllDay": true,
            "start": {"dateTime": "2026-11-12T00:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-11-13T00:00:00.0000000", "timeZone": "UTC"},
            "originalStartTimeZone": "UTC", "originalEndTimeZone": "UTC",
            "recurrence": {
                "pattern": {"type": "weekly", "interval": 1, "daysOfWeek": ["thursday"], "firstDayOfWeek": "monday"},
                "range": {"type": "numbered", "startDate": "2026-11-12", "numberOfOccurrences": 3, "recurrenceTimeZone": "Romance Standard Time"}
            },
            "cancelledOccurrences": []
        })
    }

    #[test]
    fn an_all_day_exception_replaces_the_day_it_was_on_in_the_series_zone() {
        let renamed = json!({
            "id": "X2", "changeKey": "ckx2", "type": "exception", "seriesMasterId": "M2", "iCalUId": "per-instance-uid",
            "subject": "Day off, renamed", "isAllDay": true, "originalStart": "2026-11-18T23:00:00Z",
            "start": {"dateTime": "2026-11-19T00:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-11-20T00:00:00.0000000", "timeZone": "UTC"},
            "originalStartTimeZone": "UTC", "originalEndTimeZone": "UTC"
        });
        let masters: Masters = [("M2".to_string(), Some(graph(all_day_series())))].into();
        let (changes, _) = batch(vec![graph(renamed)], masters, &Known::new(), true, &[], Europe::Brussels, &BTreeSet::new());
        let events: Vec<&EventData> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Upsert { events, .. } => Some(&events[0]),
                Change::Remove { .. } => None,
            })
            .collect();
        let day = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(events[1].recurrence_id, Some(Moment::Date(day("2026-11-19"))), "Thursday in Brussels, not the UTC date of its midnight");
        assert_eq!(starts(events[0], &[events[1].clone()], (2026, 11), 1), ["2026-11-12", "2026-11-19", "2026-11-26"], "19 November once");
    }

    /// Microsoft 365 listing the instances of the all-day series around a day.
    struct Instances;

    impl Api for Instances {
        async fn request(&self, _: &str, url: &str, _: Option<&Value>, _: &[(&str, &str)]) -> Result<(u16, Value)> {
            assert!(url.starts_with("/me/events/M2/instances?"), "{url}");
            Ok((200, json!({ "value": [
                {"id": "O-12", "originalStart": "2026-11-11T23:00:00Z", "changeKey": "ck2"},
                {"id": "O-19", "originalStart": "2026-11-18T23:00:00Z", "changeKey": "ck2"}
            ]})))
        }

        fn failure(&self, status: u16, _: &Value) -> Error {
            Error::Other(format!("Microsoft Graph {status}"))
        }

        fn zone(&self) -> Tz {
            Europe::Brussels
        }
    }

    #[tokio::test]
    async fn an_answer_to_one_day_of_an_all_day_series_finds_that_day() {
        let raw = serde_json::to_vec(&all_day_series()).unwrap();
        let master = Stored { id: crate::calendar::EventId(1), remote_id: "M2".into(), etag: None, raw: Some(raw), data: mapped(all_day_series()) };
        let day = NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        assert_eq!(instance(&Instances, &master, &Moment::Date(day)).await.unwrap().id, "O-19");
    }

    #[test]
    fn deletions_and_a_master_graph_cannot_give() {
        let known: Known = [("S1", "a"), ("X9", "b"), ("O7", "c"), ("M9", "d")].into_iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect();
        let earlier = BTreeSet::from(["M1".to_string(), "M9".to_string()]);
        let instances = vec![
            graph(json!({"@removed": {"reason": "deleted"}, "id": "X9"})),
            graph(json!({"@removed": {"reason": "deleted"}, "id": "an-occurrence"})),
            graph(json!({"id": "O7", "type": "occurrence", "seriesMasterId": "M1", "iCalUId": "u7",
                         "start": {"dateTime": "2026-12-01T08:00:00.0000000", "timeZone": "UTC"}})),
            graph(json!({"id": "O8", "type": "occurrence", "seriesMasterId": "M2", "iCalUId": "u8", "subject": "Shared series",
                         "originalStart": "2026-12-02T08:00:00Z",
                         "start": {"dateTime": "2026-12-02T08:00:00.0000000", "timeZone": "UTC"},
                         "end": {"dateTime": "2026-12-02T08:30:00.0000000", "timeZone": "UTC"}})),
        ];
        let (m1, raw) = graph(master_json(json!({"type": "daily", "interval": 1}), json!({"type": "noEnd"})));
        let wanted = masters_to_fetch(&instances, &earlier, false);
        assert_eq!(wanted, BTreeSet::from(["M1".to_string(), "M2".to_string(), "M9".to_string()]), "an unplaceable deletion fetches every series again");
        let masters: Masters = [("M1".into(), Some((m1, raw))), ("M2".into(), None), ("M9".into(), None)].into();
        let (changes, holding) = batch(instances, masters, &known, false, &[], Europe::Brussels, &earlier);
        let summary: Vec<String> = changes
            .iter()
            .map(|c| match c {
                Change::Upsert { remote_id, events, .. } => format!("+{remote_id} {}", events[0].uid),
                Change::Remove { remote_id } => format!("-{remote_id}"),
            })
            .collect();
        assert_eq!(
            summary,
            ["+M1 040000008200E00074C5B7101A82E008SERIES", "+O8 u8", "-M9", "-O7", "-X9", "-an-occurrence"],
            "M9 is gone; O8's master can't be read, so the occurrence stands alone; O7 was stored and now expands from M1"
        );
        let Change::Upsert { events, .. } = &changes[1] else { unreachable!() };
        assert_eq!(events[0].recurrence_id, None, "a stand-in is a single event");
        assert_eq!(holding, BTreeSet::from(["M1".to_string()]));
    }

    #[test]
    fn all_day_event_comes_back_as_dates() {
        let e = mapped(json!({
            "id": "A1", "type": "singleInstance", "iCalUId": "a1", "subject": "Armistice", "isAllDay": true,
            "start": {"dateTime": "2026-11-10T23:00:00.0000000", "timeZone": "UTC"},
            "end": {"dateTime": "2026-11-11T23:00:00.0000000", "timeZone": "UTC"},
            "originalStartTimeZone": "Romance Standard Time", "showAs": "free"
        }));
        let day = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        assert_eq!(e.span, Span::AllDay { start: day("2026-11-11"), end: day("2026-11-12") });
        assert_eq!(e.show_as, ShowAs::Free);
    }

    #[test]
    fn calendars_and_the_window() {
        let me = "me@example.com";
        let own = remote_calendar(&json!({"id": "c1", "name": "Calendar", "hexColor": "#e3008c", "isDefaultCalendar": true, "canEdit": true, "owner": {"address": "Me@example.com"}}), me);
        let shared = remote_calendar(&json!({"id": "c2", "name": "Ann", "hexColor": "", "canEdit": true, "owner": {"address": "ann@example.com"}}), me);
        let read = remote_calendar(&json!({"id": "c3", "name": "Holidays", "canEdit": false, "owner": {"address": "ann@example.com"}}), me);
        assert_eq!((own.access, own.primary, own.color.as_deref()), (Access::Owner, true, Some("#e3008c")));
        assert_eq!((shared.access, shared.color), (Access::Write, None));
        assert_eq!(read.access, Access::Read);
        assert_eq!(window(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap()), ("2024-11-01T00:00:00Z".into(), "2029-11-01T00:00:00Z".into()));
    }

    #[test]
    fn an_answer_with_a_new_time_proposes_it() {
        use crate::calendar::rsvp::{Proposal, Scope};
        let at = |h: u32| Utc.with_ymd_and_hms(2026, 11, 6, h, 0, 0).unwrap();
        let rsvp = |answer, note: Option<&str>, notify, proposal| Rsvp { event: crate::calendar::EventId(1), answer, note: note.map(str::to_string), notify, scope: Scope::Series, proposal };
        let proposal = Some(Proposal { start: at(13), end: at(14) });
        assert_eq!(
            answer_body(&rsvp(Answer::Maybe, Some("Friday?"), true, proposal)),
            json!({
                "sendResponse": true,
                "comment": "Friday?",
                "proposedNewTime": {"start": {"dateTime": "2026-11-06T13:00:00", "timeZone": "UTC"}, "end": {"dateTime": "2026-11-06T14:00:00", "timeZone": "UTC"}}
            })
        );
        assert_eq!(answer_body(&rsvp(Answer::Decline, Some("x"), false, proposal)), json!({ "sendResponse": false }), "Graph refuses a comment or a time without a reply");
        assert_eq!(answer_body(&rsvp(Answer::Accept, None, true, None)), json!({ "sendResponse": true }));
        assert!(takes_proposals(br#"{"id":"a"}"#), "a copy stored before Jolta asked");
        assert!(!takes_proposals(br#"{"allowNewTimeProposals":false}"#));
    }
}
