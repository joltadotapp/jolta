//! Google Calendar API v3: the calendar list, and each calendar's events
//! with `singleEvents=false`, so a series arrives as its master and its
//! overrides and Jolta expands it. After the first full listing a sync
//! token brings only what changed; Google answers 410 when the token
//! expired, and the calendar is listed in full again. The one write is
//! answering an invitation.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Change, Known, SyncBatch, encode, http_client, http_error, provider_url, vanished};
use crate::calendar::meeting::{self, Entry, EntryKind};
use crate::calendar::rsvp::{Occurrence, Rsvp, Target};
use crate::calendar::{
    Access, Alarm, AlarmTrigger, Attendee, Calendar, EventData, EventStatus, Local, Moment, Person,
    Recurrence, Response, Role, ShowAs, Span, Visibility, Zone, tz,
};
use crate::store::RemoteCalendar;
use crate::{Account, Error, Result, oauth};

const BASE: &str = "https://www.googleapis.com/calendar/v3";

pub struct Google {
    http: reqwest::Client,
    account: Account,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarListPage {
    #[serde(default)]
    items: Vec<ListEntry>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListEntry {
    id: String,
    #[serde(default)]
    summary: String,
    summary_override: Option<String>,
    background_color: Option<String>,
    time_zone: Option<String>,
    access_role: String,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    deleted: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsPage {
    #[serde(default)]
    items: Vec<Value>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
    #[serde(default)]
    default_reminders: Vec<Reminder>,
    time_zone: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GoogleEvent {
    id: String,
    etag: Option<String>,
    status: Option<String>,
    #[serde(rename = "iCalUID")]
    ical_uid: Option<String>,
    summary: String,
    description: String,
    location: String,
    start: Option<When>,
    end: Option<When>,
    recurrence: Vec<String>,
    recurring_event_id: Option<String>,
    original_start_time: Option<When>,
    transparency: Option<String>,
    visibility: Option<String>,
    event_type: Option<String>,
    organizer: Option<Who>,
    attendees: Vec<GoogleAttendee>,
    reminders: Option<Reminders>,
    conference_data: Option<ConferenceData>,
    hangout_link: Option<String>,
    color_id: Option<String>,
    sequence: i64,
    source: Option<SourceLink>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct When {
    date: Option<NaiveDate>,
    date_time: Option<DateTime<FixedOffset>>,
    time_zone: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Who {
    email: String,
    display_name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GoogleAttendee {
    email: String,
    display_name: String,
    #[serde(rename = "self")]
    is_self: bool,
    optional: bool,
    resource: bool,
    response_status: String,
    comment: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Reminders {
    use_default: bool,
    overrides: Vec<Reminder>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct Reminder {
    method: String,
    minutes: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ConferenceData {
    entry_points: Vec<EntryPoint>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct EntryPoint {
    entry_point_type: String,
    uri: String,
    meeting_code: Option<String>,
    passcode: Option<String>,
    password: Option<String>,
    pin: Option<String>,
    access_code: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct SourceLink {
    url: Option<String>,
}

/// One page of `events.list`, or Google saying the sync token expired.
enum Page {
    Events(EventsPage),
    Gone,
}

/// Every page of one sync, and where the next one starts.
struct Listing {
    items: Vec<Value>,
    default_reminders: Vec<Reminder>,
    zone: Option<Tz>,
    sync_token: Option<String>,
    /// The whole calendar was listed, so known events missing from it are gone.
    full: bool,
}

impl Google {
    pub fn new(account: &Account) -> Result<Google> {
        Ok(Google { http: http_client(true)?, account: account.clone() })
    }

    async fn get(&self, url: &str) -> Result<(u16, Vec<u8>)> {
        self.request(reqwest::Method::GET, url, None, None).await
    }

    /// A request with the account's token. Rate limits and brief outages
    /// are retried; the caller reads every other status.
    pub(super) async fn request(&self, method: reqwest::Method, url: &str, body: Option<&Value>, if_match: Option<&str>) -> Result<(u16, Vec<u8>)> {
        let url = provider_url(url, "www.googleapis.com")?;
        for attempt in 0..5u32 {
            let token = oauth::access_token(&self.account).await?;
            let mut req = self.http.request(method.clone(), url.clone()).bearer_auth(&token);
            if let Some(body) = body {
                req = req.header("Content-Type", "application/json").body(body.to_string());
            }
            if let Some(etag) = if_match {
                req = req.header("If-Match", etag);
            }
            let resp = req.send().await.map_err(http_error)?;
            let status = resp.status().as_u16();
            let retry_after = resp.headers().get("Retry-After").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let body = resp.bytes().await.map_err(http_error)?.to_vec();
            let limited = status == 403 && reason(&body).is_some_and(|r| r.contains("RateLimitExceeded") || r.contains("rateLimitExceeded"));
            if (status == 429 || limited || matches!(status, 500 | 502 | 503)) && attempt < 4 {
                tokio::time::sleep(Duration::from_secs(retry_after.unwrap_or(2 << attempt).min(60))).await;
                continue;
            }
            return match status {
                401 => Err(Error::Auth(format!("Google refused the sign-in for {}. Sign in again.", self.account.address))),
                403 if reason(&body).as_deref() == Some("insufficientPermissions") => Err(Error::Auth(format!(
                    "Google gave Jolta no calendar access for {}. Turn calendar off and on again for it to ask.",
                    self.account.address
                ))),
                _ => Ok((status, body)),
            };
        }
        unreachable!("the last attempt returns")
    }

    async fn get_ok(&self, url: &str) -> Result<Vec<u8>> {
        let (status, body) = self.get(url).await?;
        if !(200..300).contains(&status) {
            return Err(failure(status, &body));
        }
        Ok(body)
    }

    pub async fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = format!("{BASE}/users/me/calendarList?maxResults=250");
            if let Some(t) = &page_token {
                url.push_str(&format!("&pageToken={}", encode(t)));
            }
            let page: CalendarListPage = parse(&self.get_ok(&url).await?)?;
            out.extend(page.items.into_iter().filter(|c| !c.deleted).map(remote_calendar));
            page_token = page.next_page_token;
            if page_token.is_none() {
                return Ok(out);
            }
        }
    }

    pub async fn sync(&self, calendar: &Calendar, known: &Known) -> Result<SyncBatch> {
        let id = calendar.remote_id.as_str();
        let listing = listing(calendar.cursor.clone(), |sync_token: Option<String>, page_token: Option<String>| async move {
            self.events_page(id, sync_token.as_deref(), page_token.as_deref()).await
        })
        .await?;
        let uids = self.master_uids(id, &listing.items).await?;
        Ok(batch(listing, known, &uids, std::slice::from_ref(&self.account.address)))
    }

    async fn events_page(&self, calendar: &str, sync_token: Option<&str>, page_token: Option<&str>) -> Result<Page> {
        let mut url = format!(
            "{BASE}/calendars/{}/events?singleEvents=false&showDeleted=true&maxResults=250",
            encode(calendar)
        );
        if let Some(t) = sync_token {
            url.push_str(&format!("&syncToken={}", encode(t)));
        }
        if let Some(t) = page_token {
            url.push_str(&format!("&pageToken={}", encode(t)));
        }
        let (status, body) = self.get(&url).await?;
        match status {
            410 => Ok(Page::Gone),
            200..300 => Ok(Page::Events(parse(&body)?)),
            _ => Err(failure(status, &body)),
        }
    }

    /// The iCalendar UID of each series whose override arrived without one
    /// (Google trims cancelled occurrences to their ids) and whose master
    /// isn't in this sync. A master that is gone gives none.
    async fn master_uids(&self, calendar: &str, items: &[Value]) -> Result<HashMap<String, String>> {
        let mut uids: HashMap<String, String> = HashMap::new();
        for item in items {
            if let (Some(id), Some(uid)) = (item["id"].as_str(), item["iCalUID"].as_str()) {
                uids.insert(id.to_string(), uid.to_string());
            }
        }
        let missing: BTreeSet<String> = items
            .iter()
            .filter(|i| i["iCalUID"].as_str().is_none())
            .filter_map(|i| i["recurringEventId"].as_str())
            .filter(|m| !uids.contains_key(*m))
            .map(str::to_string)
            .collect();
        for master in missing {
            let url = format!("{BASE}/calendars/{}/events/{}", encode(calendar), encode(&master));
            let (status, body) = self.get(&url).await?;
            match status {
                200..300 => {
                    if let Some(uid) = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v["iCalUID"].as_str().map(str::to_string)) {
                        uids.insert(master, uid);
                    }
                }
                404 | 410 => {}
                _ => return Err(failure(status, &body)),
            }
        }
        Ok(uids)
    }

    /// Patches the user's own attendee entry (`responseStatus`, `comment`)
    /// with `sendUpdates`, guarded by the event's ETag. The entry is read
    /// fresh first, because a patch replaces the whole attendee list; when
    /// the event changes in between, Google answers 412 and it is read again.
    pub async fn answer(&self, calendar: &Calendar, target: &Target, rsvp: &Rsvp) -> Result<String> {
        let id = match &target.occurrence {
            None => target.event.remote_id.clone(),
            Some(Occurrence { stored: Some(s), .. }) => s.remote_id.clone(),
            Some(Occurrence { recurrence_id, stored: None }) => self.instance(calendar, &target.event.remote_id, recurrence_id).await?,
        };
        let url = format!("{BASE}/calendars/{}/events/{}", encode(&calendar.remote_id), encode(&id));
        let send = if rsvp.notify { "all" } else { "none" };
        for attempt in 0..3 {
            let event: Value = parse(&self.get_ok(&url).await?)?;
            let attendees = answered(&event, &self.account.address, rsvp)?;
            let (status, body) = self
                .request(reqwest::Method::PATCH, &format!("{url}?sendUpdates={send}"), Some(&json!({ "attendees": attendees })), event["etag"].as_str())
                .await?;
            match status {
                200..300 => return Ok(id),
                412 if attempt < 2 => continue,
                _ => return Err(failure(status, &body)),
            }
        }
        unreachable!("the last attempt returns")
    }

    /// The id of the occurrence of series `master` that `rid` names. Google
    /// lists instances by their time, so the day around it is searched for
    /// the one whose original start matches.
    async fn instance(&self, calendar: &Calendar, master: &str, rid: &Moment) -> Result<String> {
        let here = tz::here();
        let fallback = Zone::Tz(calendar.zone.as_deref().and_then(tz::zone).unwrap_or(chrono_tz::UTC));
        let at = match rid {
            Moment::Time(local) => local.instant(here),
            Moment::Date(day) => tz::instant(day.and_time(chrono::NaiveTime::MIN), here),
        };
        let stamp = |t: DateTime<chrono::Utc>| encode(&t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        let url = format!(
            "{BASE}/calendars/{}/events/{}/instances?timeMin={}&timeMax={}&maxResults=50",
            encode(&calendar.remote_id),
            encode(master),
            stamp(at - chrono::Duration::hours(36)),
            stamp(at + chrono::Duration::hours(36)),
        );
        let page: EventsPage = parse(&self.get_ok(&url).await?)?;
        page.items
            .into_iter()
            .filter_map(|item| serde_json::from_value::<GoogleEvent>(item).ok())
            .find(|g| g.original_start_time.as_ref().and_then(|w| moment(w, fallback)).is_some_and(|m| m.names(rid, here)))
            .map(|g| g.id)
            .ok_or_else(|| Error::NotFound("Google has no occurrence of this series at that time".into()))
    }
}

/// Which attendee the calendar answers as: the entry Google marks `self`,
/// which on a calendar shared with the user is the calendar's owner, and
/// only without one the first with one of the user's addresses.
fn own_entry<T>(attendees: &[T], is_self: impl Fn(&T) -> bool, email: impl Fn(&T) -> &str, me: &[String]) -> Option<usize> {
    attendees.iter().position(is_self).or_else(|| attendees.iter().position(|a| me.iter().any(|m| m.eq_ignore_ascii_case(email(a)))))
}

/// The event's attendee list with the user's own entry answered.
fn answered(event: &Value, me: &str, rsvp: &Rsvp) -> Result<Vec<Value>> {
    let mut attendees = event["attendees"].as_array().cloned().unwrap_or_default();
    let own = own_entry(&attendees, |a| a["self"] == true, |a| a["email"].as_str().unwrap_or_default(), &[me.to_string()])
        .ok_or_else(|| Error::Other(format!("{me} isn't on this meeting's guest list, so Jolta can't answer for it.")))?;
    let own = &mut attendees[own];
    own["responseStatus"] = json!(match rsvp.answer.response() {
        Response::Accepted => "accepted",
        Response::Tentative => "tentative",
        _ => "declined",
    });
    own["comment"] = json!(rsvp.note.clone().unwrap_or_default());
    Ok(attendees)
}

/// Pages through one sync. A refused token starts again without one.
/// `fetch` is a plain closure returning a future over owned tokens: an
/// `AsyncFnMut` borrows itself for each call, and that higher-ranked borrow
/// makes the sync future fail rustc's Send check when the scheduler spawns it.
async fn listing<F: Future<Output = Result<Page>>>(token: Option<String>, mut fetch: impl FnMut(Option<String>, Option<String>) -> F) -> Result<Listing> {
    let mut token = token;
    'listing: loop {
        let mut listing = Listing { items: Vec::new(), default_reminders: Vec::new(), zone: None, sync_token: None, full: token.is_none() };
        let mut page_token: Option<String> = None;
        loop {
            match fetch(token.clone(), page_token.clone()).await? {
                Page::Gone if token.is_some() => {
                    tracing::info!("Google refused the sync token; listing the calendar again");
                    token = None;
                    continue 'listing;
                }
                Page::Gone => return Err(Error::Other("Google answered 410 to a full listing".into())),
                Page::Events(page) => {
                    listing.items.extend(page.items);
                    if page_token.is_none() {
                        listing.default_reminders = page.default_reminders;
                        listing.zone = page.time_zone.as_deref().and_then(tz::zone);
                    }
                    page_token = page.next_page_token;
                    if page_token.is_none() {
                        listing.sync_token = page.next_sync_token;
                        return Ok(listing);
                    }
                }
            }
        }
    }
}

/// The changes one listing brings. Each Google event, master or override,
/// is its own remote object. A deleted single event or master is removed;
/// a deleted occurrence of a series stays as a cancelled override, which
/// takes that occurrence out.
fn batch(listing: Listing, known: &Known, uids: &HashMap<String, String>, me: &[String]) -> SyncBatch {
    let mut changes = Vec::new();
    let mut removed: BTreeSet<String> = BTreeSet::new();
    let mut present: BTreeSet<String> = BTreeSet::new();
    for item in listing.items {
        let raw = serde_json::to_vec(&item).expect("JSON serializes");
        let g: GoogleEvent = match serde_json::from_value(item) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("skipping a Google event: {e}");
                continue;
            }
        };
        let cancelled = g.status.as_deref() == Some("cancelled");
        if cancelled && g.recurring_event_id.is_none() {
            // Occurrences of a deleted series carry the master's id before an underscore.
            let prefix = format!("{}_", g.id);
            removed.extend(known.keys().filter(|k| k.starts_with(&prefix)).cloned());
            removed.insert(g.id);
            continue;
        }
        let uid = g.ical_uid.clone().or_else(|| g.recurring_event_id.as_ref().and_then(|m| uids.get(m).cloned()));
        let Some(event) = uid.and_then(|uid| event(&g, uid, &listing.default_reminders, listing.zone, me)) else {
            tracing::warn!("skipping Google event {}: no UID or start", g.id);
            continue;
        };
        present.insert(g.id.clone());
        changes.push(Change::Upsert { remote_id: g.id, etag: g.etag, events: vec![event], raw: Some(raw) });
    }
    if listing.full {
        removed.extend(vanished(known, &present).into_iter().filter_map(|c| match c {
            Change::Remove { remote_id } => Some(remote_id),
            Change::Upsert { .. } => None,
        }));
    }
    removed.retain(|id| !present.contains(id));
    changes.extend(removed.into_iter().map(|remote_id| Change::Remove { remote_id }));
    SyncBatch { changes, cursor: listing.sync_token, refresh: None }
}

fn remote_calendar(c: ListEntry) -> RemoteCalendar {
    RemoteCalendar {
        name: c.summary_override.filter(|s| !s.is_empty()).unwrap_or(c.summary),
        remote_id: c.id,
        color: c.background_color.filter(|s| s.starts_with('#')),
        zone: c.time_zone,
        access: match c.access_role.as_str() {
            "owner" => Access::Owner,
            "writer" => Access::Write,
            "freeBusyReader" => Access::FreeBusy,
            _ => Access::Read,
        },
        primary: c.primary,
        refresh: None,
    }
}

/// Google's JSON for one event as `EventData`. None without a start.
fn event(g: &GoogleEvent, uid: String, defaults: &[Reminder], calendar_zone: Option<Tz>, me: &[String]) -> Option<EventData> {
    let fallback = Zone::Tz(calendar_zone.unwrap_or(chrono_tz::UTC));
    let recurrence_id = g.original_start_time.as_ref().and_then(|w| moment(w, fallback));
    let start = g.start.as_ref().and_then(|w| moment(w, fallback)).or(recurrence_id)?;
    let end = g.end.as_ref().and_then(|w| moment(w, fallback)).unwrap_or(start);
    let span = match (start, end) {
        (Moment::Date(s), Moment::Date(e)) => Span::AllDay { start: s, end: e.max(s.succ_opt()?) },
        (Moment::Time(s), Moment::Time(e)) => Span::Timed { start: s, end: e },
        (Moment::Date(s), _) => Span::AllDay { start: s, end: s.succ_opt()? },
        (Moment::Time(s), _) => Span::Timed { start: s, end: s },
    };
    let start_zone = match start {
        Moment::Time(l) => l.zone,
        Moment::Date(_) => fallback,
    };

    let event_type = g.event_type.as_deref().unwrap_or("default");
    let show_as = match event_type {
        "outOfOffice" => ShowAs::OutOfOffice,
        "workingLocation" | "birthday" => ShowAs::Free,
        _ if g.transparency.as_deref() == Some("transparent") => ShowAs::Free,
        _ => ShowAs::Busy,
    };
    let own = own_entry(&g.attendees, |a| a.is_self, |a| a.email.as_str(), me);
    let attendees = g
        .attendees
        .iter()
        .enumerate()
        .map(|(i, a)| Attendee {
            person: Person { address: a.email.clone(), name: a.display_name.clone() },
            role: if a.resource {
                Role::Resource
            } else if a.optional {
                Role::Optional
            } else {
                Role::Required
            },
            response: match a.response_status.as_str() {
                "accepted" => Response::Accepted,
                "declined" => Response::Declined,
                "tentative" => Response::Tentative,
                _ => Response::NeedsAction,
            },
            comment: a.comment.clone(),
            is_me: own == Some(i),
        })
        .collect();
    let reminders: &[Reminder] = match &g.reminders {
        Some(r) if r.use_default => defaults,
        Some(r) => &r.overrides,
        None => &[],
    };
    let alarms = reminders
        .iter()
        .map(|r| Alarm { trigger: AlarmTrigger::MinutesBefore(r.minutes), email: r.method == "email" })
        .collect();

    let url = g.source.as_ref().and_then(|s| s.url.clone());
    let meeting = meeting::detect(&entries(g), url.as_deref(), &g.location, &g.description);
    Some(EventData {
        uid,
        recurrence_id,
        this_and_future: false,
        span,
        recurrence: recurrence(&g.recurrence, start_zone),
        title: g.summary.clone(),
        location: g.location.clone(),
        description: g.description.clone(),
        description_html: looks_like_html(&g.description),
        url,
        status: match g.status.as_deref() {
            Some("cancelled") => EventStatus::Cancelled,
            Some("tentative") => EventStatus::Tentative,
            _ => EventStatus::Confirmed,
        },
        show_as,
        visibility: match g.visibility.as_deref() {
            Some("public") => Visibility::Public,
            Some("private") => Visibility::Private,
            Some("confidential") => Visibility::Confidential,
            _ => Visibility::Default,
        },
        organizer: g.organizer.as_ref().map(|o| Person { address: o.email.clone(), name: o.display_name.clone() }),
        attendees,
        sequence: g.sequence,
        alarms,
        meeting,
        categories: Vec::new(),
        color: g.color_id.as_deref().and_then(event_color).map(str::to_string),
    })
}

/// A start, end or original start: a date, or a time on the wall clock of
/// its own zone, else the calendar's.
fn moment(w: &When, fallback: Zone) -> Option<Moment> {
    if let Some(date) = w.date {
        return Some(Moment::Date(date));
    }
    let at = w.date_time?;
    let zone = w.time_zone.as_deref().and_then(tz::zone).map(Zone::Tz).unwrap_or(fallback);
    Some(Moment::Time(Local { wall: tz::wall(at.to_utc(), zone, chrono_tz::UTC), zone }))
}

fn entries(g: &GoogleEvent) -> Vec<Entry> {
    let mut out: Vec<Entry> = g
        .conference_data
        .iter()
        .flat_map(|c| &c.entry_points)
        .filter_map(|e| {
            let kind = match e.entry_point_type.as_str() {
                "video" => EntryKind::Video,
                "phone" => EntryKind::Phone,
                _ => return None,
            };
            Some(Entry {
                kind,
                uri: e.uri.clone(),
                meeting_id: e.meeting_code.clone(),
                passcode: e.passcode.clone().or_else(|| e.password.clone()).or_else(|| e.pin.clone()).or_else(|| e.access_code.clone()),
            })
        })
        .collect();
    if !out.iter().any(|e| e.kind == EntryKind::Video)
        && let Some(link) = &g.hangout_link
    {
        out.insert(0, Entry { kind: EntryKind::Video, uri: link.clone(), meeting_id: None, passcode: None });
    }
    out
}

/// Google's `recurrence` lines: RRULE, RDATE and EXDATE as in iCalendar.
/// Dates without a TZID are on the series' own wall clock.
fn recurrence(lines: &[String], zone: Zone) -> Option<Recurrence> {
    let mut rec = Recurrence::default();
    for line in lines {
        let Some((head, value)) = line.split_once(':') else { continue };
        let mut parts = head.split(';');
        let name = parts.next().unwrap_or_default().to_ascii_uppercase();
        let mut tzid = None;
        let mut date_only = false;
        for p in parts {
            match p.split_once('=') {
                Some((k, v)) if k.eq_ignore_ascii_case("TZID") => tzid = tz::zone(v),
                Some((k, v)) if k.eq_ignore_ascii_case("VALUE") => date_only = v.eq_ignore_ascii_case("DATE"),
                _ => {}
            }
        }
        let zone = tzid.map(Zone::Tz).unwrap_or(zone);
        let moments = || value.split(',').filter_map(|v| ical_moment(v.split('/').next().unwrap_or(v), date_only, zone));
        match name.as_str() {
            "RRULE" => rec.rules.push(value.trim().to_string()),
            "RDATE" => rec.rdates.extend(moments()),
            "EXDATE" => rec.exdates.extend(moments()),
            _ => {}
        }
    }
    (!rec.rules.is_empty() || !rec.rdates.is_empty()).then_some(rec)
}

fn ical_moment(value: &str, date_only: bool, zone: Zone) -> Option<Moment> {
    let value = value.trim();
    if date_only || value.len() == 8 {
        return NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(Moment::Date);
    }
    let (wall, utc) = match value.strip_suffix('Z') {
        Some(v) => (v, true),
        None => (value, false),
    };
    let wall = NaiveDateTime::parse_from_str(wall, "%Y%m%dT%H%M%S").ok()?;
    let zone = if utc { Zone::Tz(chrono_tz::UTC) } else { zone };
    Some(Moment::Time(Local { wall, zone }))
}

/// Google lets descriptions hold a little HTML (`<b>`, `<a href>`, `<br>`).
fn looks_like_html(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.windows(2).enumerate().any(|(i, w)| {
        w[0] == b'<' && (w[1].is_ascii_alphabetic() || w[1] == b'/') && text[i..].contains('>')
    })
}

/// Google's fixed palette for `colorId` on events (`colors.get`, `event`).
fn event_color(id: &str) -> Option<&'static str> {
    Some(match id {
        "1" => "#a4bdfc",
        "2" => "#7ae7bf",
        "3" => "#dbadff",
        "4" => "#ff887c",
        "5" => "#fbd75b",
        "6" => "#ffb878",
        "7" => "#46d6db",
        "8" => "#e1e1e1",
        "9" => "#5484ed",
        "10" => "#51b749",
        "11" => "#dc2127",
        _ => return None,
    })
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| Error::Other(format!("Google Calendar reply: {e}")))
}

/// The first error reason Google gives (`insufficientPermissions`, `rateLimitExceeded`).
fn reason(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v["error"]["errors"][0]["reason"].as_str().or_else(|| v["error"]["status"].as_str()).map(str::to_string)
}

fn failure(status: u16, body: &[u8]) -> Error {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let message = v["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    Error::Other(format!("Google Calendar {status}: {message}"))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use chrono_tz::{America, Europe};
    use serde_json::json;

    use super::*;
    use crate::calendar::MeetingProvider;
    use crate::calendar::recur::{self, Window};
    use crate::calendar::rsvp::{Answer, Scope};

    fn wall(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn at(s: &str, tz: Tz) -> Local {
        Local { wall: wall(s), zone: Zone::Tz(tz) }
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn page(items: Vec<Value>) -> EventsPage {
        EventsPage {
            items,
            next_page_token: None,
            next_sync_token: Some("next".into()),
            default_reminders: vec![Reminder { method: "popup".into(), minutes: 10 }],
            time_zone: Some("Europe/Brussels".into()),
        }
    }

    fn listing_of(items: Vec<Value>, full: bool) -> Listing {
        let p = page(items);
        Listing { items: p.items, default_reminders: p.default_reminders, zone: Some(Europe::Brussels), sync_token: p.next_sync_token, full }
    }

    /// The one event in a batch built from `item`.
    fn mapped(item: Value) -> EventData {
        let b = batch(listing_of(vec![item], false), &Known::new(), &HashMap::new(), &["me@example.com".into()]);
        match b.changes.as_slice() {
            [Change::Upsert { events, .. }] => events[0].clone(),
            other => panic!("expected one upsert, got {other:?}"),
        }
    }

    #[test]
    fn timed_event_in_another_zone_keeps_its_own_wall_clock() {
        let e = mapped(json!({
            "id": "ev1", "etag": "\"3181\"", "status": "confirmed", "iCalUID": "ev1@google.com",
            "summary": "Call with New York", "description": "Agenda in <b>bold</b>",
            "start": {"dateTime": "2026-11-10T15:00:00+01:00", "timeZone": "America/New_York"},
            "end": {"dateTime": "2026-11-10T16:00:00+01:00", "timeZone": "America/New_York"},
            "organizer": {"email": "boss@example.com", "displayName": "Boss"},
            "attendees": [
                {"email": "boss@example.com", "organizer": true, "responseStatus": "accepted"},
                {"email": "me@example.com", "self": true, "responseStatus": "tentative", "comment": "maybe"},
                {"email": "room-1@resource.calendar.google.com", "resource": true, "responseStatus": "accepted"},
                {"email": "fyi@example.com", "optional": true}
            ],
            "reminders": {"useDefault": true},
            "conferenceData": {
                "conferenceSolution": {"key": {"type": "hangoutsMeet"}, "name": "Google Meet"},
                "entryPoints": [
                    {"entryPointType": "video", "uri": "https://meet.google.com/abc-defg-hij", "meetingCode": "abc-defg-hij"},
                    {"entryPointType": "phone", "uri": "tel:+32-2-555-0100", "pin": "123456789"}
                ]
            },
            "colorId": "11"
        }));
        assert_eq!(e.span, Span::Timed { start: at("2026-11-10 09:00", America::New_York), end: at("2026-11-10 10:00", America::New_York) });
        assert_eq!(e.uid, "ev1@google.com");
        assert!(e.description_html);
        assert_eq!(e.my_response(), Some(Response::Tentative));
        let roles: Vec<Role> = e.attendees.iter().map(|a| a.role).collect();
        assert_eq!(roles, [Role::Required, Role::Required, Role::Resource, Role::Optional]);
        assert_eq!(e.attendees[1].comment, "maybe");
        assert_eq!(e.alarms, [Alarm { trigger: AlarmTrigger::MinutesBefore(10), email: false }], "useDefault takes the calendar's reminders");
        let m = e.meeting.expect("a Meet link");
        assert_eq!((m.provider, m.join_url.as_str()), (MeetingProvider::Meet, "https://meet.google.com/abc-defg-hij"));
        assert_eq!(m.phones.len(), 1);
        assert_eq!(e.color.as_deref(), Some("#dc2127"));
        assert_eq!(e.organizer.unwrap().name, "Boss");
    }

    #[test]
    fn a_shared_calendar_answers_as_its_owner_in_the_patch_and_the_store() {
        let event = |attendees: Value| {
            json!({
                "id": "shared", "iCalUID": "shared@google.com", "summary": "Planning",
                "start": {"date": "2026-12-01"}, "end": {"date": "2026-12-02"},
                "attendees": attendees
            })
        };
        let accept = Rsvp { event: crate::calendar::EventId(1), answer: Answer::Accept, note: None, notify: true, scope: Scope::Series };
        let statuses = |list: &[Value]| list.iter().map(|a| a["responseStatus"].as_str().unwrap_or_default().to_string()).collect::<Vec<_>>();
        let marked = |e: &EventData| e.attendees.iter().map(|a| a.is_me).collect::<Vec<_>>();

        let shared = event(json!([
            {"email": "me@example.com", "responseStatus": "needsAction"},
            {"email": "owner@example.com", "self": true, "responseStatus": "needsAction"}
        ]));
        assert_eq!(statuses(&answered(&shared, "me@example.com", &accept).unwrap()), ["needsAction", "accepted"]);
        assert_eq!(marked(&mapped(shared)), [false, true]);

        let own = event(json!([
            {"email": "boss@example.com", "responseStatus": "accepted"},
            {"email": "ME@example.com", "responseStatus": "needsAction"}
        ]));
        assert_eq!(statuses(&answered(&own, "me@example.com", &accept).unwrap()), ["accepted", "accepted"], "without a self entry the address decides");
        assert_eq!(marked(&mapped(own)), [false, true]);
    }

    #[test]
    fn all_day_out_of_office_and_working_location() {
        let e = mapped(json!({
            "id": "ooo", "iCalUID": "ooo@google.com", "summary": "Holiday", "eventType": "outOfOffice",
            "start": {"date": "2026-12-24"}, "end": {"date": "2026-12-27"},
            "reminders": {"useDefault": false, "overrides": [{"method": "email", "minutes": 1440}]}
        }));
        assert_eq!(e.span, Span::AllDay { start: day("2026-12-24"), end: day("2026-12-27") });
        assert_eq!(e.show_as, ShowAs::OutOfOffice);
        assert_eq!(e.alarms, [Alarm { trigger: AlarmTrigger::MinutesBefore(1440), email: true }]);
        let wl = mapped(json!({"id": "wl", "iCalUID": "wl", "eventType": "workingLocation", "start": {"date": "2026-11-12"}, "end": {"date": "2026-11-13"}}));
        assert_eq!(wl.show_as, ShowAs::Free);
        let free = mapped(json!({"id": "f", "iCalUID": "f", "transparency": "transparent", "start": {"date": "2026-11-12"}, "end": {"date": "2026-11-13"}}));
        assert_eq!(free.show_as, ShowAs::Free);
        assert!(!free.description_html);
    }

    #[test]
    fn recurring_master_with_an_exdate_in_its_zone_and_an_override() {
        let master = mapped(json!({
            "id": "abc123", "etag": "\"1\"", "iCalUID": "series@google.com", "summary": "Standup",
            "start": {"dateTime": "2026-11-02T09:00:00+01:00", "timeZone": "Europe/Brussels"},
            "end": {"dateTime": "2026-11-02T09:15:00+01:00", "timeZone": "Europe/Brussels"},
            "recurrence": ["EXDATE;TZID=Europe/Brussels:20261109T090000", "RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=4", "RDATE;VALUE=DATE:20261225"]
        }));
        let rec = master.recurrence.clone().unwrap();
        assert_eq!(rec.rules, ["FREQ=WEEKLY;BYDAY=MO;COUNT=4"]);
        assert_eq!(rec.exdates, [Moment::Time(at("2026-11-09 09:00", Europe::Brussels))]);
        assert_eq!(rec.rdates, [Moment::Date(day("2026-12-25"))]);

        let moved = mapped(json!({
            "id": "abc123_20261116T080000Z", "iCalUID": "series@google.com", "recurringEventId": "abc123", "summary": "Standup, later",
            "originalStartTime": {"dateTime": "2026-11-16T09:00:00+01:00", "timeZone": "Europe/Brussels"},
            "start": {"dateTime": "2026-11-16T11:00:00+01:00", "timeZone": "Europe/Brussels"},
            "end": {"dateTime": "2026-11-16T11:15:00+01:00", "timeZone": "Europe/Brussels"}
        }));
        assert_eq!(moved.uid, master.uid, "an override joins its master's series");
        assert_eq!(moved.recurrence_id, Some(Moment::Time(at("2026-11-16 09:00", Europe::Brussels))));
        assert_eq!(moved.recurrence, None);

        let window = Window { start: Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(), end: Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap() };
        let starts: Vec<String> = recur::expand(&master, &[moved], window, Europe::Brussels)
            .into_iter()
            .map(|o| match o.span {
                Span::Timed { start, .. } => start.wall.format("%m-%d %H:%M").to_string(),
                Span::AllDay { start, .. } => start.to_string(),
            })
            .collect();
        assert_eq!(starts, ["11-02 09:00", "11-16 11:00", "11-23 09:00"], "9 November is excluded, 16 November moved");
    }

    #[test]
    fn cancelled_occurrence_and_deleted_events() {
        let known: Known = [("gone", "\"1\""), ("dead", "\"2\""), ("dead_20261103T090000Z", "\"3\""), ("kept", "\"4\"")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), Some(v.to_string())))
            .collect();
        let items = vec![
            json!({"id": "abc123_20261123T080000Z", "status": "cancelled", "recurringEventId": "abc123",
                   "originalStartTime": {"dateTime": "2026-11-23T09:00:00+01:00", "timeZone": "Europe/Brussels"}}),
            json!({"id": "dead", "status": "cancelled"}),
        ];
        let uids: HashMap<String, String> = [("abc123".to_string(), "series@google.com".to_string())].into();
        let b = batch(listing_of(items, false), &known, &uids, &[]);
        let Change::Upsert { remote_id, events, .. } = &b.changes[0] else { panic!("{:?}", b.changes) };
        assert_eq!(remote_id, "abc123_20261123T080000Z");
        assert_eq!(events[0].uid, "series@google.com", "the trimmed occurrence finds its UID through the master");
        assert_eq!(events[0].status, EventStatus::Cancelled);
        assert_eq!(events[0].recurrence_id, Some(Moment::Time(at("2026-11-23 09:00", Europe::Brussels))));
        assert_eq!(
            b.changes[1..],
            [Change::Remove { remote_id: "dead".into() }, Change::Remove { remote_id: "dead_20261103T090000Z".into() }],
            "a deleted master takes its stored occurrences along; an incremental sync leaves the rest"
        );
        assert_eq!(b.cursor.as_deref(), Some("next"));
    }

    #[test]
    fn an_expired_sync_token_lists_everything_again_and_drops_what_is_gone() {
        let known: Known = [("kept", "\"1\""), ("gone", "\"2\"")].into_iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect();
        let mut calls: Vec<(Option<String>, Option<String>)> = Vec::new();
        let listing = block_on(listing(Some("old".into()), |sync: Option<String>, page_token: Option<String>| {
            calls.push((sync.clone(), page_token.clone()));
            std::future::ready(Ok(match (sync, page_token) {
                (Some(_), _) => Page::Gone,
                (None, None) => Page::Events(EventsPage {
                    next_page_token: Some("p2".into()),
                    next_sync_token: None,
                    ..page(vec![json!({"id": "kept", "etag": "\"1\"", "iCalUID": "k", "start": {"date": "2026-11-11"}, "end": {"date": "2026-11-12"}})])
                }),
                (None, Some(_)) => Page::Events(EventsPage {
                    next_sync_token: Some("fresh".into()),
                    ..page(vec![json!({"id": "new", "iCalUID": "n", "start": {"date": "2026-11-12"}, "end": {"date": "2026-11-13"}})])
                }),
            }))
        }))
        .unwrap();
        assert_eq!(
            calls,
            [(Some("old".into()), None), (None, None), (None, Some("p2".into()))],
            "410 restarts without the token and follows the pages"
        );
        assert!(listing.full);
        let b = batch(listing, &known, &HashMap::new(), &[]);
        let ids: Vec<(&str, bool)> = b
            .changes
            .iter()
            .map(|c| match c {
                Change::Upsert { remote_id, .. } => (remote_id.as_str(), true),
                Change::Remove { remote_id } => (remote_id.as_str(), false),
            })
            .collect();
        assert_eq!(ids, [("kept", true), ("new", true), ("gone", false)]);
        assert_eq!(b.cursor.as_deref(), Some("fresh"));
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    #[test]
    fn calendar_list_access_and_names() {
        let page: CalendarListPage = serde_json::from_value(json!({"items": [
            {"id": "me@example.com", "summary": "me@example.com", "backgroundColor": "#9fe1e7", "timeZone": "Europe/Brussels", "accessRole": "owner", "primary": true},
            {"id": "team@group.calendar.google.com", "summary": "Team", "summaryOverride": "My team", "accessRole": "writer"},
            {"id": "en.be#holiday@group.v.calendar.google.com", "summary": "Holidays", "accessRole": "reader"},
            {"id": "busy@example.com", "summary": "Colleague", "accessRole": "freeBusyReader"}
        ]})).unwrap();
        let list: Vec<(String, String, Access, bool)> =
            page.items.into_iter().map(remote_calendar).map(|c| (c.remote_id, c.name, c.access, c.primary)).collect();
        assert_eq!(
            list,
            [
                ("me@example.com".into(), "me@example.com".into(), Access::Owner, true),
                ("team@group.calendar.google.com".into(), "My team".into(), Access::Write, false),
                ("en.be#holiday@group.v.calendar.google.com".into(), "Holidays".into(), Access::Read, false),
                ("busy@example.com".into(), "Colleague".into(), Access::FreeBusy, false),
            ]
        );
        assert_eq!(encode("en.be#holiday@group.v.calendar.google.com"), "en.be%23holiday%40group.v.calendar.google.com");
    }
}
