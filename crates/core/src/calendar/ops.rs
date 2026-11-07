//! The calendar op queue: an edit changes the store at once and reaches the
//! server later, with undo and offline.
//!
//! `queue` writes the edit into the event tables (the optimistic change),
//! marks the event "not synced yet" and stores an op. The calendar
//! scheduler's worker (`run_due`) sends due ops, one at a time per event and
//! in order, and `settle` records how each went. A local calendar has no
//! server: the store is its truth, so its ops only carry undo.
//!
//! An op's update or delete carries the version of the server object its
//! edit started from (`Held`), and every write is guarded by it. A sync
//! leaves the rows an op still has to send alone (`store::apply_item`), so
//! a change made elsewhere meanwhile reaches the op as a 412: the op then
//! merges the user's edit with the server's version field by field
//! (`merge::three_way`) and writes again, or ends in `conflict` when both
//! changed the same field, keeping the server's value until the user picks
//! theirs with Apply mine.

use std::fmt;
use std::str::FromStr;

use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};

use super::change::{FIELDS, Field};
use super::series::Horizon;
use super::sources::{self, OpWrite, Source, Version, Written};
use super::{Alarm, Calendar, CalendarId, CalendarSource, EventData, EventId, EventStatus, Moment, caps, merge, rsvp, series as occurrences};
use crate::model::text_enum;
use crate::{Error, Jolta, Result, store};

/// Ops that email people wait the undo window; ops fail for good after
/// as many attempts as mail ops.
pub use crate::engine::{MAX_ATTEMPTS, UNDO_SEND_SECONDS};

/// Ops sent at once, across events.
pub const AT_ONCE: usize = 4;

/// Writes one op makes after 412s, each merged with the server's version,
/// before it gives up: Google's `answer` uses the same bound.
const MERGE_ROUNDS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CalendarOpId(pub i64);

text_enum!(OpState {
    Pending => "pending",
    Sending => "sending",
    Done => "done",
    Failed => "failed",
    Conflict => "conflict",
    Gone => "gone",
});

text_enum!(Scope {
    This => "this",
    Following => "following",
    All => "all",
});

text_enum!(Notify {
    All => "all",
    Outside => "outside",
    Changed => "changed",
    Nobody => "none",
});

/// Provider fields Jolta doesn't model, passed through untouched: a JSON
/// object merged into a Google or Graph request body, or iCalendar lines
/// added to a CalDAV VEVENT. Never overrides a field `EventData` holds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Extra {
    Json(serde_json::Map<String, serde_json::Value>),
    Lines(Vec<String>),
}

/// The server object an update or delete writes over, at the version the
/// op's edit started from: its remote id and ETag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub remote_id: String,
    pub etag: Option<String>,
}

/// What an op does to one event, with what undo needs to take it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// A new event, or a series with its changed occurrences.
    Create(Vec<EventData>),
    /// One event, series or occurrence: `base` as it was, `event` as the
    /// user saved it. A series' changed occurrences that its new time or
    /// repeat rule no longer holds leave with it, as they were (`dropped`).
    /// `fresh` when `event` is an occurrence that had no row of its own:
    /// `held` is then its series' object, which Google and Microsoft 365
    /// reach as an instance and CalDAV gets a new VEVENT in.
    Update { base: EventData, event: EventData, dropped: Vec<Row>, fresh: bool },
    /// A whole event or series with its changed occurrences, as it was.
    Delete(Vec<EventData>),
    /// One occurrence of a series taken out: the series and the occurrence
    /// as they were, with the occurrence's own row when it had one.
    Cancel { series: EventData, occurrence: EventData, row: Option<Row> },
}

/// A stored row as an op takes it out, so putting it back keeps the object
/// and version it was filed under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub remote_id: String,
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    pub event: EventData,
}

impl From<Stored> for Row {
    fn from(s: Stored) -> Row {
        Row { remote_id: s.remote_id, etag: s.etag, blob: s.blob, event: s.event }
    }
}

impl Write {
    pub fn kind(&self) -> &'static str {
        match self {
            Write::Create(_) => "create",
            Write::Update { .. } => "update",
            Write::Delete(_) => "delete",
            Write::Cancel { .. } => "cancel",
        }
    }

    /// The event the user sees: the one saved, or the one deleted.
    pub fn event(&self) -> Option<&EventData> {
        match self {
            Write::Create(events) | Write::Delete(events) => events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()),
            Write::Update { event, .. } | Write::Cancel { occurrence: event, .. } => Some(event),
        }
    }

    /// Every event the write touches, on either side.
    fn events(&self) -> impl Iterator<Item = &EventData> {
        let (a, b): (&[EventData], &[EventData]) = match self {
            Write::Create(events) | Write::Delete(events) => (events, &[]),
            Write::Update { base, event, .. } | Write::Cancel { series: base, occurrence: event, .. } => (std::slice::from_ref(base), std::slice::from_ref(event)),
        };
        a.iter().chain(b)
    }

    /// Makes `base` the version this write starts from, for the row it names.
    fn rebase(&mut self, base: &EventData) {
        match self {
            Write::Update { base: b, .. } => *b = base.clone(),
            Write::Delete(events) => {
                if let Some(e) = events.iter_mut().find(|e| e.recurrence_id == base.recurrence_id) {
                    *e = base.clone();
                }
            }
            Write::Cancel { series, .. } if series.recurrence_id == base.recurrence_id => *series = base.clone(),
            Write::Create(_) | Write::Cancel { .. } => {}
        }
    }

    /// The change that takes this one back once it may have reached the
    /// server. None for one occurrence taken out: Microsoft 365 can't bring
    /// one back, so that waits the undo window instead.
    fn undo(&self) -> Option<Change> {
        match self {
            Write::Create(events) => Some(Change::Delete { uid: events.first().map(|e| e.uid.clone()).unwrap_or_default() }),
            Write::Update { base, dropped, .. } if dropped.is_empty() => Some(Change::Update(base.clone())),
            Write::Update { .. } => None,
            Write::Delete(events) => Some(Change::Create(events.clone())),
            Write::Cancel { .. } => None,
        }
    }

    /// Whether the write takes out occurrences or their changes, which
    /// can't be put back once sent: it waits the undo window.
    fn removes_occurrences(&self) -> bool {
        matches!(self, Write::Cancel { .. }) || matches!(self, Write::Update { dropped, .. } if !dropped.is_empty())
    }
}

/// One queued edit as the table stores it.
#[derive(Clone, Debug, PartialEq)]
pub struct CalendarOp {
    pub id: CalendarOpId,
    pub calendar: CalendarId,
    pub uid: String,
    pub recurrence_id: Option<Moment>,
    pub write: Write,
    pub scope: Scope,
    pub notify: Notify,
    pub message: Option<String>,
    pub extra: Option<Extra>,
    /// Makes a retried create land on the same object: Google's event id,
    /// Graph's transactionId, the CalDAV resource path, the UID for a local
    /// calendar.
    pub remote_key: String,
    /// For an update or delete on a server: the object and version it
    /// writes over.
    pub held: Option<Held>,
    /// A sync passed over a server version of the op's rows while it
    /// waited, so taking it back leaves the store behind the server.
    pub passed_over: bool,
    pub send_after: i64,
    pub state: OpState,
    /// Claimed by the worker at least once, so it may have reached the server.
    pub tried: bool,
    pub attempts: u32,
    pub error: Option<String>,
    /// Fields the server kept on a 412 merge.
    pub lost: Vec<Field>,
    pub created: i64,
    /// The op this one waits for: This and following shortens the old
    /// series only once the new one exists, so a failure loses nothing.
    pub after: Option<CalendarOpId>,
}

/// What the user changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A new event, or a series with its changed occurrences, under one UID.
    Create(Vec<EventData>),
    /// The event as the user saved it; its UID and recurrence id name the
    /// stored event, series or changed occurrence it replaces.
    Update(EventData),
    /// The event with this UID, with its changed occurrences.
    Delete { uid: String },
    /// One occurrence of the series with this UID, named by its recurrence id.
    DeleteOccurrence { uid: String, recurrence_id: Moment },
}

/// An edit to queue.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub calendar: CalendarId,
    pub change: Change,
    pub notify: Notify,
    pub message: Option<String>,
    pub extra: Option<Extra>,
}

impl Edit {
    /// A change that tells nobody.
    pub fn quiet(calendar: CalendarId, change: Change) -> Edit {
        Edit { calendar, change, notify: Notify::Nobody, message: None, extra: None }
    }
}

/// Applies the edit to the store and queues it for the server. None when
/// the edit changes nothing, so saving the same edit twice queues one op.
/// Nothing here waits on the network. An edit the calendar's provider
/// can't hold is refused before anything changes.
pub fn queue(conn: &Connection, edit: &Edit, now: i64, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    queue_op(conn, edit, Link::default(), now, horizon)
}

/// How an op of This and following belongs to its pair.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Link {
    pub(super) after: Option<CalendarOpId>,
    pub(super) following: bool,
}

pub(super) fn queue_op(conn: &Connection, edit: &Edit, link: Link, now: i64, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    let calendar = store::calendar(conn, edit.calendar)?;
    if !caps::of(&calendar).writable {
        return Err(Error::Other(format!("{} is read-only", calendar.name)));
    }
    let Some(Planned { write, remote_key, held }) = plan(conn, &calendar, &edit.change, horizon.here)? else { return Ok(None) };
    check(&calendar, &write, &remote_key, held.as_ref(), edit.extra.as_ref())?;
    let first = write.event().ok_or_else(|| Error::Other("an edit without an event".into()))?;
    let (uid, recurrence_id) = (first.uid.clone(), if matches!(write, Write::Update { .. } | Write::Cancel { .. }) { first.recurrence_id } else { None });
    let scope = match (link.following, recurrence_id) {
        (true, _) => Scope::Following,
        (false, Some(_)) => Scope::This,
        (false, None) => Scope::All,
    };
    let waits = edit.notify != Notify::Nobody || write.removes_occurrences() || link.following;
    let delay = if waits { UNDO_SEND_SECONDS } else { 0 };
    apply(conn, calendar.id, &write, &remote_key, horizon)?;
    let id = store::insert_calendar_op(
        conn,
        &CalendarOp {
            id: CalendarOpId(0),
            calendar: calendar.id,
            uid: uid.clone(),
            recurrence_id,
            write,
            scope,
            notify: edit.notify,
            message: edit.message.clone(),
            extra: edit.extra.clone(),
            remote_key,
            held,
            passed_over: false,
            send_after: now + delay,
            state: OpState::Pending,
            tried: false,
            attempts: 0,
            error: None,
            lost: Vec::new(),
            created: now,
            after: link.after,
        },
    )?;
    store::refresh_changed_locally(conn, calendar.id, &uid)?;
    Ok(Some(id))
}

/// Why `queue` would refuse the edit, found without changing anything, so
/// the editor can say it before the user saves.
pub fn refusal(conn: &Connection, edit: &Edit) -> Result<Option<String>> {
    let calendar = store::calendar(conn, edit.calendar)?;
    if !caps::of(&calendar).writable {
        return Ok(Some(format!("{} is read-only", calendar.name)));
    }
    let Some(Planned { write, remote_key, held }) = plan(conn, &calendar, &edit.change, super::tz::here())? else { return Ok(None) };
    Ok(check(&calendar, &write, &remote_key, held.as_ref(), edit.extra.as_ref()).err().map(|e| e.to_string()))
}

/// Refuses what the calendar's provider can't take from Jolta yet:
/// meetings with guests, whose changes email people (the Send an update?
/// dialog comes first), and any change the provider's request can't carry.
fn check(calendar: &Calendar, write: &Write, key: &str, held: Option<&Held>, extra: Option<&Extra>) -> Result<()> {
    if calendar.source == CalendarSource::Local {
        return Ok(());
    }
    if write.events().any(|e| e.attendees.iter().any(|a| !a.is_me)) {
        return Err(Error::Other(format!("Jolta can't change meetings with guests in {} yet", calendar.name)));
    }
    let held = rsvp::Stored {
        id: EventId(0),
        remote_id: held.map(|h| h.remote_id.clone()).unwrap_or_default(),
        etag: held.and_then(|h| h.etag.clone()),
        raw: None,
        data: write.event().cloned().ok_or_else(|| Error::Other("an edit without an event".into()))?,
    };
    let dropped: Vec<EventData> = match write {
        Write::Update { dropped, .. } => dropped.iter().map(|r| r.event.clone()).collect(),
        _ => Vec::new(),
    };
    let request = match write {
        Write::Create(events) => OpWrite::Create { events, key, extra },
        Write::Update { base, event, fresh: true, .. } => OpWrite::Occurrence { series: &held, base, event, extra },
        Write::Update { base, event, .. } => OpWrite::Update { held: &held, base, event, extra, dropped: &dropped },
        Write::Delete(_) => OpWrite::Delete { held: &held },
        Write::Cancel { occurrence, row, .. } => OpWrite::Cancel { held: &held, occurrence, changed: row.is_some() },
    };
    sources::writable(calendar.source, &request).map_err(|e| Error::Other(format!("{e}, so Jolta didn't change {}", calendar.name)))
}

/// A write against what the store holds, with where it lands.
struct Planned {
    write: Write,
    remote_key: String,
    held: Option<Held>,
}

/// The write a change makes against what the store holds, with the key a
/// retried create reuses and the version an update or delete writes over.
/// None when the store already shows the change.
fn plan(conn: &Connection, calendar: &Calendar, change: &Change, here: Tz) -> Result<Option<Planned>> {
    let on_server = calendar.source != CalendarSource::Local;
    match change {
        Change::Create(events) => {
            let Some(uid) = events.first().map(|e| e.uid.clone()) else { return Err(Error::Other("nothing to create".into())) };
            if events.iter().any(|e| e.uid != uid) {
                return Err(Error::Other("a create holds one UID".into()));
            }
            let stored = series(conn, calendar.id, &uid)?;
            if !stored.is_empty() {
                return if same_events(&stored, events) { Ok(None) } else { Err(Error::Other(format!("{uid} is already in {}", calendar.name))) };
            }
            let key = remote_key(conn, calendar, &uid)?;
            Ok(Some(Planned { write: Write::Create(events.clone()), remote_key: key, held: None }))
        }
        Change::Update(event) => {
            let not_found = || Error::NotFound(format!("event {} in {}", event.uid, calendar.name));
            let Some(base) = stored(conn, calendar.id, &event.uid, event.recurrence_id.as_ref())? else {
                // An occurrence without a row of its own: the series moved
                // there is what the user changed, written as the series'.
                let rid = event.recurrence_id.ok_or_else(not_found)?;
                let master = stored(conn, calendar.id, &event.uid, None)?.ok_or_else(not_found)?;
                if !is_occurrence(&master.event, &rid, here) {
                    return Err(Error::NotFound(format!("that occurrence of {}", master.event.title)));
                }
                let base = occurrences::occurrence(std::slice::from_ref(&master.event), &rid, here).ok_or_else(not_found)?;
                if base == *event {
                    return Ok(None);
                }
                let held = on_server.then(|| Held { remote_id: master.remote_id.clone(), etag: master.etag.clone() });
                return Ok(Some(Planned { write: Write::Update { base, event: event.clone(), dropped: Vec::new(), fresh: true }, remote_key: master.remote_id, held }));
            };
            if base.event == *event {
                return Ok(None);
            }
            let dropped = dropped(conn, calendar, &base.event, event, here)?;
            let held = on_server.then(|| Held { remote_id: base.remote_id.clone(), etag: base.etag.clone() });
            Ok(Some(Planned { write: Write::Update { base: base.event, event: event.clone(), dropped, fresh: false }, remote_key: base.remote_id, held }))
        }
        Change::DeleteOccurrence { uid, recurrence_id } => {
            let master = stored(conn, calendar.id, uid, None)?.ok_or_else(|| Error::NotFound(format!("event {uid} in {}", calendar.name)))?;
            if master.event.recurrence.is_none() {
                return Err(Error::Other(format!("{} doesn't repeat", master.event.title)));
            }
            let own = stored(conn, calendar.id, uid, Some(recurrence_id))?;
            let occurrence = match &own {
                Some(row) => row.event.clone(),
                None if is_occurrence(&master.event, recurrence_id, here) => occurrences::occurrence(std::slice::from_ref(&master.event), recurrence_id, here)
                    .ok_or_else(|| Error::NotFound(format!("that occurrence of {}", master.event.title)))?,
                None => return Ok(None),
            };
            if occurrence.status == EventStatus::Cancelled {
                return Ok(None);
            }
            let held = on_server.then(|| match &own {
                Some(row) => Held { remote_id: row.remote_id.clone(), etag: row.etag.clone() },
                None => Held { remote_id: master.remote_id.clone(), etag: master.etag.clone() },
            });
            let write = Write::Cancel { series: master.event.clone(), occurrence, row: own.map(Row::from) };
            Ok(Some(Planned { write, remote_key: master.remote_id, held }))
        }
        Change::Delete { uid } => {
            let events = series(conn, calendar.id, uid)?;
            let Some(first) = stored(conn, calendar.id, uid, None)?.or(stored_any(conn, calendar.id, uid)?) else { return Ok(None) };
            let held = on_server.then(|| Held { remote_id: first.remote_id.clone(), etag: first.etag.clone() });
            Ok(Some(Planned { write: Write::Delete(events), remote_key: first.remote_id, held }))
        }
    }
}

/// Whether the series `master` has an occurrence at `rid`, by its rule.
pub fn is_occurrence(master: &EventData, rid: &Moment, here: Tz) -> bool {
    let Some(span) = occurrences::occurrence_span(&master.span, rid, here) else { return false };
    let (start, end) = match span.bounds(here) {
        super::Bounds::Utc { start, .. } => (start - chrono::Duration::seconds(1), start + chrono::Duration::seconds(1)),
        super::Bounds::Days { start, .. } => {
            let day = super::tz::instant(chrono::NaiveDate::default().and_time(chrono::NaiveTime::MIN) + chrono::Duration::days(start.into()), here);
            (day - chrono::Duration::days(1), day + chrono::Duration::days(2))
        }
    };
    let rule_only = EventData { recurrence: master.recurrence.clone().map(|r| super::Recurrence { exdates: Vec::new(), ..r }), ..master.clone() };
    super::recur::expand(&rule_only, &[], super::recur::Window { start, end }, here).iter().any(|o| o.span.start().names(rid, here))
}

/// The changed occurrences of a series that leave with an edit of the
/// series: those the new rule no longer has an occurrence for, and on
/// Microsoft 365 every one once the time or the pattern changes, as
/// Exchange deletes them then; a new end alone keeps those before it
/// (Microsoft 365 test account, 2026-11-07). A series made single loses
/// them all.
fn dropped(conn: &Connection, calendar: &Calendar, base: &EventData, event: &EventData, here: Tz) -> Result<Vec<Row>> {
    if event.recurrence_id.is_some() || base.recurrence.is_none() {
        return Ok(Vec::new());
    }
    let pattern_changed = base.span != event.span || base.recurrence != event.recurrence;
    if !pattern_changed {
        return Ok(Vec::new());
    }
    let pattern = |r: Option<&super::Recurrence>| {
        r.map(|r| r.rules.iter().map(|rule| rule.split(';').filter(|p| !p.starts_with("UNTIL=") && !p.starts_with("COUNT=")).collect::<Vec<_>>().join(";")).collect::<Vec<_>>())
    };
    let exchange_resets = calendar.source == CalendarSource::Graph && (base.span != event.span || pattern(base.recurrence.as_ref()) != pattern(event.recurrence.as_ref()));
    Ok(changed_rows(conn, calendar.id, &event.uid)?
        .into_iter()
        .filter(|r| {
            let Some(rid) = &r.event.recurrence_id else { return false };
            event.recurrence.is_none() || exchange_resets || !is_occurrence(event, rid, here)
        })
        .map(Row::from)
        .collect())
}

/// How many changed occurrences, not counting those taken out, leave when
/// the series `base` becomes `event`: the scope prompt's warning.
pub fn dropped_count(conn: &Connection, calendar: &Calendar, base: &EventData, event: &EventData, here: Tz) -> Result<usize> {
    Ok(dropped(conn, calendar, base, event, here)?.iter().filter(|r| r.event.status != EventStatus::Cancelled).count())
}

/// The rows of a series' changed occurrences.
fn changed_rows(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<Vec<Stored>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT {STORED_COLS} FROM event WHERE calendar_id = ?1 AND uid = ?2 AND recurrence_id <> '' ORDER BY id"))?;
    let rows = stmt.query_map(params![calendar, uid], stored_row)?;
    rows.map(|r| to_stored(r?)).collect()
}

fn same_events(a: &[EventData], b: &[EventData]) -> bool {
    a.len() == b.len() && a.iter().all(|e| b.contains(e))
}

/// Where a new event lands, so a retried create finds the first try's
/// object: Google finds it by the UID, which also names the id it gives
/// the event; Graph's transactionId comes from the UID, the CalDAV
/// resource is named from it, a local calendar files it under the UID. An
/// event created again under the same UID, as Undo of a delete does, gets
/// a transactionId and resource name of its own: Graph answers a
/// transactionId it saw with the event it made then.
fn remote_key(conn: &Connection, calendar: &Calendar, uid: &str) -> Result<String> {
    let earlier = store::calendar_creates(conn, calendar.id, uid)?;
    let seed = if earlier == 0 { uid.to_string() } else { format!("{uid}\n{earlier}") };
    Ok(match calendar.source {
        CalendarSource::Google => sources::google::event_id(uid),
        CalendarSource::Graph => sources::graph::transaction_id(&calendar.remote_id, &seed),
        CalendarSource::CalDav => sources::caldav::resource_path(calendar, &seed)?,
        _ => uid.to_string(),
    })
}

/// A stored event with what the store keeps beside it.
pub struct Stored {
    pub id: EventId,
    pub remote_id: String,
    pub etag: Option<String>,
    /// The hash of the object's text in the blob store.
    pub blob: Option<String>,
    pub event: EventData,
}

const STORED_COLS: &str = "id, remote_id, etag, blob, data";

type StoredRow = (EventId, String, Option<String>, Option<String>, String);

fn stored_row(r: &rusqlite::Row) -> rusqlite::Result<StoredRow> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}

fn to_stored((id, remote_id, etag, blob, data): StoredRow) -> Result<Stored> {
    let event = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))?;
    Ok(Stored { id, remote_id, etag, blob, event })
}

/// The stored event with this UID and recurrence id: the series or single
/// event without one.
pub fn stored(conn: &Connection, calendar: CalendarId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<Stored>> {
    let rid = recurrence_id.map(store::moment_text).unwrap_or_default();
    conn.query_row(
        &format!("SELECT {STORED_COLS} FROM event WHERE calendar_id = ?1 AND uid = ?2 AND recurrence_id = ?3 ORDER BY id LIMIT 1"),
        params![calendar, uid, rid],
        stored_row,
    )
    .optional()?
    .map(to_stored)
    .transpose()
}

/// Any stored event with this UID, for a changed occurrence stored without
/// its series.
fn stored_any(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<Option<Stored>> {
    conn.query_row(&format!("SELECT {STORED_COLS} FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY id LIMIT 1"), params![calendar, uid], stored_row)
        .optional()?
        .map(to_stored)
        .transpose()
}

/// Every stored event with this UID: a series with its changed occurrences.
pub(crate) fn series(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<Vec<EventData>> {
    let mut stmt = conn.prepare_cached("SELECT data FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY recurrence_id <> '', id")?;
    let rows = stmt.query_map(params![calendar, uid], |r| r.get::<_, String>(0))?;
    rows.map(|data| serde_json::from_str(&data?).map_err(|e| Error::Other(format!("stored event {uid}: {e}")))).collect()
}

/// Writes `write` into the event tables and rebuilds the series' occurrences.
/// An edited row keeps its remote id, ETag and text: the version the
/// server still has until the op reaches it.
fn apply(conn: &Connection, calendar: CalendarId, write: &Write, remote_key: &str, horizon: &Horizon) -> Result<()> {
    let uid = match write {
        Write::Create(events) => {
            for e in events {
                store::insert_event(conn, calendar, remote_key, None, e, None)?;
            }
            events.first().map(|e| e.uid.clone())
        }
        Write::Update { event, dropped, .. } => {
            // A new changed occurrence is filed with its series, at the
            // version the series is: in CalDAV they share one resource.
            let (remote_id, etag, blob) = match stored(conn, calendar, &event.uid, event.recurrence_id.as_ref())? {
                Some(s) => (s.remote_id, s.etag, s.blob),
                None => stored(conn, calendar, &event.uid, None)?.map_or_else(|| (remote_key.to_string(), None, None), |s| (s.remote_id, s.etag, s.blob)),
            };
            let before = stored(conn, calendar, &event.uid, event.recurrence_id.as_ref())?.map(|s| s.event);
            store::insert_event(conn, calendar, &remote_id, etag.as_deref(), event, blob.as_deref())?;
            for row in dropped {
                remove_row(conn, calendar, &row.event)?;
            }
            // A change to the whole series shows on its changed occurrences
            // where they kept the series' value, as the server does it.
            if let (None, Some(before)) = (event.recurrence_id, before) {
                for row in changed_rows(conn, calendar, &event.uid)? {
                    let now = super::change::carried(&before, event, &row.event);
                    if now != row.event {
                        store::insert_event(conn, calendar, &row.remote_id, row.etag.as_deref(), &now, row.blob.as_deref())?;
                    }
                }
            }
            Some(event.uid.clone())
        }
        Write::Delete(events) => {
            let uid = events.first().map(|e| e.uid.clone());
            conn.execute("DELETE FROM event WHERE calendar_id = ?1 AND uid = ?2", params![calendar, uid])?;
            uid
        }
        Write::Cancel { series, occurrence, row } => {
            let master = stored(conn, calendar, &series.uid, None)?.ok_or_else(|| Error::NotFound(format!("event {} in its calendar", series.uid)))?;
            let mut cut = series.clone();
            if let (Some(rec), Some(rid)) = (cut.recurrence.as_mut(), occurrence.recurrence_id) {
                rec.exdates.push(rid);
            }
            store::insert_event(conn, calendar, &master.remote_id, master.etag.as_deref(), &cut, master.blob.as_deref())?;
            if let Some(row) = row {
                remove_row(conn, calendar, &row.event)?;
            }
            Some(series.uid.clone())
        }
    };
    if let Some(uid) = uid {
        store::rebuild_series(conn, calendar, &uid, horizon)?;
    }
    Ok(())
}

/// Takes the row of one event of a series out of the store.
fn remove_row(conn: &Connection, calendar: CalendarId, e: &EventData) -> Result<()> {
    let rid = e.recurrence_id.as_ref().map(store::moment_text).unwrap_or_default();
    conn.execute("DELETE FROM event WHERE calendar_id = ?1 AND uid = ?2 AND recurrence_id = ?3", params![calendar, e.uid, rid])?;
    Ok(())
}

fn restore_row(conn: &Connection, calendar: CalendarId, row: &Row) -> Result<()> {
    store::insert_event(conn, calendar, &row.remote_id, row.etag.as_deref(), &row.event, row.blob.as_deref())
}

/// Puts the store back as it was before `op`. Events a delete took come
/// back under the object and version it held. When a sync passed over a
/// server version meanwhile, the calendar is listed whole on its next sync
/// to catch up.
fn revert(conn: &Connection, op: &CalendarOp, horizon: &Horizon) -> Result<()> {
    if store::calendar_op(conn, op.id)?.passed_over {
        store::forget_calendar_cursor(conn, op.calendar)?;
    }
    let back = match &op.write {
        Write::Create(events) => Some(Write::Delete(events.clone())),
        Write::Update { base, event, fresh: false, .. } => Some(Write::Update { base: event.clone(), event: base.clone(), dropped: Vec::new(), fresh: false }),
        // A changed occurrence the op made goes again: the series' alone shows it.
        Write::Update { base, fresh: true, .. } => {
            remove_row(conn, op.calendar, base)?;
            None
        }
        Write::Delete(events) => Some(Write::Create(events.clone())),
        Write::Cancel { series, .. } => {
            let master = stored(conn, op.calendar, &series.uid, None)?.ok_or_else(|| Error::NotFound(format!("event {} in its calendar", series.uid)))?;
            Some(Write::Update { base: master.event, event: series.clone(), dropped: Vec::new(), fresh: false })
        }
    };
    let key = op.held.as_ref().map_or(op.remote_key.as_str(), |h| h.remote_id.as_str());
    if let Some(back) = back {
        apply(conn, op.calendar, &back, key, horizon)?;
    }
    match &op.write {
        Write::Update { dropped, .. } => dropped.iter().try_for_each(|r| restore_row(conn, op.calendar, r))?,
        Write::Cancel { row: Some(row), .. } => restore_row(conn, op.calendar, row)?,
        _ => {}
    }
    store::rebuild_series(conn, op.calendar, &op.uid, horizon)?;
    if let (Write::Delete(_), Some(held)) = (&op.write, &op.held) {
        store::set_item_version(conn, op.calendar, &held.remote_id, held.etag.as_deref(), None)?;
    }
    Ok(())
}

/// How Undo went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Undone {
    /// The op never left: it is gone and the store is back as it was.
    Dropped,
    /// The op may have reached the server, so its opposite was queued.
    Queued(CalendarOpId),
}

/// Takes an op back. One the worker never claimed is dropped and the store
/// put back; one it claimed may have reached the server and gets the
/// opposite op, unless it emailed people, which can't be taken back. Only
/// an event's newest op can be undone, so undoing twice changes nothing more.
pub fn undo(conn: &Connection, id: CalendarOpId, now: i64, horizon: &Horizon) -> Result<Undone> {
    let op = store::calendar_op(conn, id)?;
    let title = op.write.event().map(|e| e.title.clone()).unwrap_or_default();
    if store::newest_calendar_op(conn, op.calendar, &op.uid)? != Some(id) {
        return Err(Error::Other(format!("{title} changed again since; undo that change first")));
    }
    // This and following: the old series' op waits for the new series'
    // create, and Undo takes back both.
    let first = op.after.map(|a| store::calendar_op(conn, a)).transpose()?;
    match op.state {
        OpState::Pending if !op.tried => {
            revert(conn, &op, horizon)?;
            store::delete_calendar_op(conn, id)?;
            store::refresh_changed_locally(conn, op.calendar, &op.uid)?;
            if let Some(first) = first {
                undo(conn, first.id, now, horizon)?;
            }
            Ok(Undone::Dropped)
        }
        OpState::Pending | OpState::Sending | OpState::Done if op.notify == Notify::Nobody => match op.write.undo() {
            Some(change) => match queue(conn, &Edit::quiet(op.calendar, change), now, horizon)? {
                Some(inverse) => Ok(Undone::Queued(inverse)),
                None => Err(Error::Other(format!("{title} is already as it was"))),
            },
            None => Err(Error::Other(format!("The change to {title} already reached the server and can't be taken back"))),
        },
        OpState::Pending | OpState::Sending | OpState::Done => Err(Error::Other(format!("The update to {title} already went out"))),
        OpState::Failed | OpState::Conflict | OpState::Gone => Err(Error::Other(format!("The change to {title} wasn't saved; there is nothing to undo"))),
    }
}

/// An op that ended in `conflict`, as the event's detail shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub op: CalendarOpId,
    /// "Changed elsewhere while you edited. Your new time wasn't saved."
    pub message: String,
    /// "Apply mine", or "Delete anyway" for a delete.
    pub action: String,
}

/// The conflict the event's newest op ended in, if it did.
pub fn conflict(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<Option<Conflict>> {
    let Some(id) = store::newest_calendar_op(conn, calendar, uid)? else { return Ok(None) };
    let op = store::calendar_op(conn, id)?;
    if op.state != OpState::Conflict {
        return Ok(None);
    }
    Ok(Some(match op.write {
        Write::Delete(_) | Write::Cancel { .. } => Conflict { op: id, message: "Changed elsewhere after you deleted it, so it wasn't deleted.".into(), action: "Delete anyway".into() },
        _ => Conflict { op: id, message: format!("Changed elsewhere while you edited. {}", unsaved(&op.lost)), action: "Apply mine".into() },
    }))
}

/// "Your new time wasn't saved." for the fields the server kept.
fn unsaved(lost: &[Field]) -> String {
    let nouns: Vec<&str> = lost.iter().map(|f| f.noun()).collect();
    match nouns.as_slice() {
        [] => "Your change wasn't saved.".into(),
        [one] => format!("Your new {one} wasn't saved."),
        [rest @ .., last] => format!("Your new {} and {last} weren't saved.", rest.join(", ")),
    }
}

/// Apply mine: queues the user's side of a conflict over the server's
/// version the store now holds, the fields the server kept taken from the
/// op's edit, or the delete again.
pub fn apply_mine(conn: &Connection, id: CalendarOpId, now: i64, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    let op = store::calendar_op(conn, id)?;
    if op.state != OpState::Conflict || store::newest_calendar_op(conn, op.calendar, &op.uid)? != Some(id) {
        return Err(Error::Other("That change was settled since".into()));
    }
    let change = match &op.write {
        Write::Delete(_) => Change::Delete { uid: op.uid.clone() },
        Write::Cancel { occurrence, .. } => match occurrence.recurrence_id {
            Some(recurrence_id) => Change::DeleteOccurrence { uid: op.uid.clone(), recurrence_id },
            None => return Err(Error::Other("That occurrence is no longer named".into())),
        },
        Write::Update { event: mine, .. } => {
            let mut event = stored(conn, op.calendar, &op.uid, op.recurrence_id.as_ref())?
                .ok_or_else(|| Error::NotFound(format!("event {} in its calendar", op.uid)))?
                .event;
            for &field in &op.lost {
                field.copy(&mut event, mine);
            }
            Change::Update(event)
        }
        Write::Create(_) => return Err(Error::Other("A new event has nothing to apply".into())),
    };
    queue(conn, &Edit { calendar: op.calendar, change, notify: op.notify, message: op.message.clone(), extra: op.extra.clone() }, now, horizon)
}

/// What came of one attempt to send an op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    pub op: CalendarOpId,
    pub calendar: CalendarId,
    /// The op failed, met a conflict or found its event deleted, and its
    /// change wasn't saved: what to tell the user.
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// "Jolta couldn't save the change to Standup: <reason>".
    pub message: String,
    /// The event as it is again, if it is still there.
    pub event: Option<EventId>,
}

/// An object as the server left it, with the hash of its text in the blob
/// store.
pub type Landed = (Version, Option<String>);

/// What reaching the server came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Sent {
    /// The server holds the change: the object as it is now; none after a
    /// delete, or on a local calendar, whose store is its server. `merged`
    /// when a change made elsewhere was merged in first.
    Done { version: Option<Landed>, merged: bool },
    /// Changed elsewhere meanwhile, and the server's value stayed for the
    /// fields in `lost`: the object as it is now, the user's other changes
    /// merged in.
    Conflict { version: Option<Landed>, lost: Vec<Field> },
    /// The event was deleted on the server.
    Gone,
}

/// Records how sending a claimed op went. A failed attempt waits and is
/// tried again, backing off as sync does; after `MAX_ATTEMPTS` the op
/// fails for good and the store goes back to the version before it.
pub fn settle(conn: &Connection, op: &CalendarOp, sent: std::result::Result<Sent, String>, now: i64, horizon: &Horizon) -> Result<Settled> {
    let calendar = store::calendar(conn, op.calendar)?;
    let title = op.write.event().map(|e| e.title.as_str()).unwrap_or("the event");
    let mut failure = None;
    match sent {
        Ok(Sent::Done { version, merged }) => {
            store::settle_calendar_op(conn, op.id, OpState::Done)?;
            land(conn, op, version.as_ref(), !merged, horizon)?;
        }
        Ok(Sent::Conflict { version, lost }) => {
            store::settle_calendar_op(conn, op.id, OpState::Conflict)?;
            store::set_calendar_op_lost(conn, op.id, &lost)?;
            land(conn, op, version.as_ref(), false, horizon)?;
            let event = stored(conn, op.calendar, &op.uid, op.recurrence_id.as_ref())?.map(|s| s.id);
            let message = match op.write {
                Write::Delete(_) | Write::Cancel { .. } => format!("{title} changed in {} after you deleted it, so it wasn't deleted.", place(&calendar)),
                _ => format!("{title} changed in {} while you edited it. {}", place(&calendar), unsaved(&lost)),
            };
            failure = Some(Failure { message, event });
        }
        // One occurrence gone says nothing of its series: the op's change
        // goes and the calendar is listed again.
        Ok(Sent::Gone) if op.recurrence_id.is_some() => {
            store::settle_calendar_op(conn, op.id, OpState::Gone)?;
            revert(conn, op, horizon)?;
            store::forget_calendar_cursor(conn, op.calendar)?;
            failure = Some(Failure { message: format!("{title} was deleted in {} while you edited it. Your change wasn't saved.", place(&calendar)), event: None });
        }
        Ok(Sent::Gone) => {
            store::settle_calendar_op(conn, op.id, OpState::Gone)?;
            store::end_later_calendar_ops(conn, op, OpState::Gone, "the event was deleted on the server")?;
            end_waiting(conn, op, "the series it continues was deleted on the server", horizon)?;
            let remote_id = op.held.as_ref().map_or(op.remote_key.as_str(), |h| h.remote_id.as_str());
            store::remove_item(conn, op.calendar, remote_id, store::Removal::Deleted, horizon)?;
            failure = Some(Failure { message: format!("{title} was deleted in {} while you edited it. Your change wasn't saved.", place(&calendar)), event: None });
        }
        Err(reason) if op.attempts + 1 < MAX_ATTEMPTS => {
            let wait = super::scheduler::backoff(op.attempts + 1, std::time::Duration::from_secs(300)).as_secs() as i64;
            store::fail_calendar_attempt(conn, op.id, &reason, Some(now + wait))?;
        }
        Err(reason) => {
            store::fail_calendar_attempt(conn, op.id, &reason, None)?;
            // A later edit of the same row is in the store and carries this
            // one's change with it; putting the old version back would hide
            // it. That edit now starts from the version before this one,
            // which is what the server still has and what its Undo restores.
            let over = match &op.write {
                Write::Create(_) => {
                    store::end_later_calendar_ops(conn, op, OpState::Failed, &format!("the event was never saved: {reason}"))?;
                    end_waiting(conn, op, &format!("the new series was never saved: {reason}"), horizon)?;
                    None
                }
                _ => store::next_calendar_op_over(conn, op)?,
            };
            match over {
                Some(mut next) => {
                    if let Write::Update { base, .. } = &op.write {
                        next.write.rebase(base);
                        next.held.clone_from(&op.held);
                        store::rewrite_calendar_op(conn, &next)?;
                    }
                }
                None => revert(conn, op, horizon)?,
            }
            let event = stored(conn, op.calendar, &op.uid, op.recurrence_id.as_ref())?.map(|s| s.id);
            failure = Some(Failure { message: format!("Jolta couldn't save the change to {title}: {reason}"), event });
        }
    }
    store::refresh_changed_locally(conn, op.calendar, &op.uid)?;
    Ok(Settled { op: op.id, calendar: op.calendar, failure })
}

/// Ends the ops waiting for `op`, which never will be sent, and puts the
/// store back from them: This and following keeps the old series whole
/// when its new series couldn't be made.
fn end_waiting(conn: &Connection, op: &CalendarOp, error: &str, horizon: &Horizon) -> Result<()> {
    for waiting in store::calendar_ops_after(conn, op.id)? {
        store::fail_calendar_attempt(conn, waiting.id, error, None)?;
        revert(conn, &waiting, horizon)?;
        store::refresh_changed_locally(conn, waiting.calendar, &waiting.uid)?;
    }
    Ok(())
}

/// Stores the version an op left on the server as a sync would. The
/// event's later ops on the same object follow it to the id the server gave
/// it. When the write was `clean`, holding only this op's change, it is
/// also the version they write over, since their edits start from this
/// one's; after a merge it holds a change they haven't seen, so they keep
/// the version they hold and meet it as a 412 of their own.
fn land(conn: &Connection, op: &CalendarOp, version: Option<&Landed>, clean: bool, horizon: &Horizon) -> Result<()> {
    let Some((v, blob)) = version else { return Ok(()) };
    let from = op.held.as_ref().map_or(op.remote_key.as_str(), |h| h.remote_id.as_str());
    // A version that doesn't name the occurrence the op wrote would stand
    // in for its series; the calendar is listed again instead.
    if let Some(rid) = &op.recurrence_id
        && v.remote_id != from
        && !v.events.iter().any(|e| e.recurrence_id.is_some_and(|r| r.names(rid, horizon.here)))
    {
        return store::forget_calendar_cursor(conn, op.calendar);
    }
    // A new changed occurrence became an instance with an id of its own:
    // only its row moves there, and only ops on that occurrence follow it.
    if let Write::Update { event, fresh: true, .. } = &op.write
        && v.remote_id != from
    {
        store::refile_occurrence(conn, op.calendar, &op.uid, event.recurrence_id.as_ref(), from, &v.remote_id)?;
        store::apply_item(conn, op.calendar, &v.remote_id, v.etag.as_deref(), &v.events, blob.as_deref(), horizon)?;
        for mut later in store::later_calendar_ops(conn, op)? {
            if later.recurrence_id != event.recurrence_id || later.held.as_ref().is_none_or(|h| h.remote_id != from) {
                continue;
            }
            later.held = Some(Held { remote_id: v.remote_id.clone(), etag: if clean { v.etag.clone() } else { later.held.and_then(|h| h.etag) } });
            if let Write::Update { fresh, .. } = &mut later.write {
                *fresh = false;
            }
            store::rewrite_calendar_op(conn, &later)?;
        }
        return Ok(());
    }
    store::refile_event_rows(conn, op.calendar, &op.uid, from, &v.remote_id)?;
    store::apply_item(conn, op.calendar, &v.remote_id, v.etag.as_deref(), &v.events, blob.as_deref(), horizon)?;
    if clean {
        store::set_item_version(conn, op.calendar, &v.remote_id, v.etag.as_deref(), blob.as_deref())?;
    }
    for mut later in store::later_calendar_ops(conn, op)? {
        let Some(held) = later.held.as_mut().filter(|h| h.remote_id == from) else { continue };
        held.remote_id.clone_from(&v.remote_id);
        if clean {
            held.etag.clone_from(&v.etag);
            if let Write::Delete(_) = later.write {
                for e in &v.events {
                    later.write.rebase(e);
                }
            }
        }
        store::rewrite_calendar_op(conn, &later)?;
    }
    Ok(())
}

/// Where a change made elsewhere was made, for the sentences about it.
fn place(calendar: &Calendar) -> &'static str {
    match calendar.source {
        CalendarSource::Google => "Google Calendar",
        CalendarSource::Graph => "Outlook",
        _ => "another calendar app",
    }
}

/// Sends every due op, up to `AT_ONCE` at a time, and records how each went.
pub async fn run_due(jolta: &Jolta, horizon: Horizon) -> Result<Vec<Settled>> {
    let mut out = Vec::new();
    loop {
        let now = chrono::Utc::now().timestamp();
        // Claiming takes the write lock even when nothing is due, which a
        // second writer on the same file, such as the CLI, would trip over.
        if jolta.store.read_async(store::next_calendar_op_due).await?.is_none_or(|due| due > now) {
            return Ok(out);
        }
        let ops = jolta.store.write_async(move |tx| store::claim_calendar_ops(tx, now, AT_ONCE)).await?;
        if ops.is_empty() {
            return Ok(out);
        }
        let sent = futures::future::join_all(ops.into_iter().map(|op| async move {
            let result = send(jolta, &op).await.map_err(|e| e.to_string());
            (op, result)
        }))
        .await;
        for (op, result) in sent {
            let now = chrono::Utc::now().timestamp();
            out.push(jolta.store.write_async(move |tx| settle(tx, &op, result, now, &horizon)).await?);
        }
    }
}

/// Reaches the op's server and keeps the text of the object it left there.
/// A local calendar's store is its server, which `queue` already wrote.
async fn send(jolta: &Jolta, op: &CalendarOp) -> Result<Sent> {
    let id = op.calendar;
    let calendar = jolta.store.read_async(move |c| store::calendar(c, id)).await?;
    if calendar.source == CalendarSource::Local {
        return Ok(Sent::Done { version: None, merged: false });
    }
    let source = super::sync::calendar_source(jolta, &calendar).await?;
    // A CalDAV write goes onto the resource's text, which the row it
    // changes keeps; one occurrence taken out goes onto its series'.
    let rid = match &op.write {
        Write::Update { event, .. } => Some(event.recurrence_id),
        Write::Cancel { .. } => Some(None),
        _ => None,
    };
    let raw = match (&op.held, rid) {
        (Some(held), Some(rid)) => {
            let (cal, uid, remote) = (op.calendar, op.uid.clone(), held.remote_id.clone());
            let blob = jolta.store.read_async(move |c| Ok(stored(c, cal, &uid, rid.as_ref())?.filter(|s| s.remote_id == remote).and_then(|s| s.blob))).await?;
            blob.map(|hash| jolta.blobs.get(&hash)).transpose()?
        }
        _ => None,
    };
    let delivered = deliver(&Bound { source: &source, calendar: &calendar }, op, raw).await?;
    let keep = |v: Option<Version>| -> Result<Option<Landed>> {
        v.map(|v| {
            let blob = v.raw.as_ref().map(|r| jolta.blobs.put(r)).transpose()?;
            Ok((v, blob))
        })
        .transpose()
    };
    Ok(match delivered {
        Delivered::Done { version, merged } => Sent::Done { version: keep(version)?, merged },
        Delivered::Conflict { version, lost } => Sent::Conflict { version: keep(version)?, lost },
        Delivered::Gone => Sent::Gone,
    })
}

/// What the server made of an op, before its text is kept.
#[derive(Clone, Debug, PartialEq)]
enum Delivered {
    /// Written; `merged` when a change made elsewhere was merged in first.
    Done { version: Option<Version>, merged: bool },
    Conflict { version: Option<Version>, lost: Vec<Field> },
    Gone,
}

/// The server one op writes to: a calendar's source, or a test's.
pub(crate) trait Remote {
    async fn write(&self, write: &OpWrite<'_>) -> Result<Written>;
    async fn fetch(&self, remote_id: &str, uid: &str, alarms: &[Alarm]) -> Result<Option<Version>>;
}

struct Bound<'a> {
    source: &'a Source,
    calendar: &'a Calendar,
}

impl Remote for Bound<'_> {
    async fn write(&self, write: &OpWrite<'_>) -> Result<Written> {
        self.source.write(self.calendar, write).await
    }

    async fn fetch(&self, remote_id: &str, uid: &str, alarms: &[Alarm]) -> Result<Option<Version>> {
        self.source.fetch(self.calendar, remote_id, uid, alarms).await
    }
}

/// Sends the op's write, guarded by the version it holds. A 412 brings the
/// server's version, which the user's edit is merged into field by field
/// before the next write; a delete goes ahead only while the event is the
/// version the user deleted. `raw` is the held object's text, for CalDAV.
async fn deliver(remote: &impl Remote, op: &CalendarOp, raw: Option<Vec<u8>>) -> Result<Delivered> {
    let extra = op.extra.as_ref();
    let held_as = |held: &Held, data: &EventData, raw: Option<Vec<u8>>| rsvp::Stored {
        id: EventId(0),
        remote_id: held.remote_id.clone(),
        etag: held.etag.clone(),
        raw,
        data: data.clone(),
    };
    match &op.write {
        Write::Create(events) => match remote.write(&OpWrite::Create { events, key: &op.remote_key, extra }).await? {
            Written::Saved(version) => Ok(Delivered::Done { version, merged: false }),
            Written::Stale | Written::Gone => Err(Error::Other("the server refused the new event".into())),
        },
        Write::Update { base, event, dropped, fresh } => {
            let Some(held) = &op.held else { return Err(Error::Other("an edit without the version it changes".into())) };
            let dropped: Vec<EventData> = dropped.iter().map(|r| r.event.clone()).collect();
            let (mut base, mut mine, mut fresh) = (base.clone(), event.clone(), *fresh);
            let mut held = held_as(held, &base, raw);
            let mut lost: Vec<Field> = Vec::new();
            for round in 0..MERGE_ROUNDS {
                let request = match fresh {
                    true => OpWrite::Occurrence { series: &held, base: &base, event: &mine, extra },
                    false => OpWrite::Update { held: &held, base: &base, event: &mine, extra, dropped: &dropped },
                };
                let written = match remote.write(&request).await? {
                    // An instance not found may only have moved: its series says.
                    Written::Gone if mine.recurrence_id.is_some() => Written::Stale,
                    written => written,
                };
                match written {
                    Written::Saved(version) if lost.is_empty() => return Ok(Delivered::Done { version, merged: round > 0 }),
                    Written::Saved(version) => return Ok(Delivered::Conflict { version, lost }),
                    Written::Gone => return Ok(Delivered::Gone),
                    Written::Stale => {
                        // Google gives no reminders for an event on the
                        // calendar's defaults; the held version's stand in.
                        let Some(now) = remote.fetch(&held.remote_id, &mine.uid, &base.alarms).await? else { return Ok(Delivered::Gone) };
                        let Some(theirs) = their_row(&now.events, &mine) else { return Ok(Delivered::Gone) };
                        let merged = merge::three_way(&base, &mine, &theirs);
                        for field in merged.lost {
                            if !lost.contains(&field) {
                                lost.push(field);
                            }
                        }
                        lost.sort();
                        if merged.event == theirs {
                            return Ok(if lost.is_empty() { Delivered::Done { version: Some(now), merged: true } } else { Delivered::Conflict { version: Some(now), lost } });
                        }
                        fresh = fresh && !now.events.iter().any(|e| same_row(e, &mine));
                        held = held_as(&Held { remote_id: now.remote_id.clone(), etag: now.etag.clone() }, &theirs, now.raw.clone());
                        (base, mine) = (theirs, merged.event);
                    }
                }
            }
            Err(Error::Other("it kept changing on the server".into()))
        }
        Write::Cancel { occurrence, row, .. } => {
            let Some(held) = &op.held else { return Err(Error::Other("a delete without the version it removes".into())) };
            let mut held = held_as(held, occurrence, raw);
            for _ in 0..MERGE_ROUNDS {
                match remote.write(&OpWrite::Cancel { held: &held, occurrence, changed: row.is_some() }).await? {
                    Written::Saved(version) => return Ok(Delivered::Done { version, merged: false }),
                    Written::Gone => return Ok(Delivered::Done { version: None, merged: false }),
                    Written::Stale => {
                        let Some(now) = remote.fetch(&held.remote_id, &occurrence.uid, &occurrence.alarms).await? else { return Ok(Delivered::Done { version: None, merged: false }) };
                        let Some(theirs) = their_row(&now.events, occurrence).filter(|t| t.status != EventStatus::Cancelled) else {
                            return Ok(Delivered::Done { version: Some(now), merged: false });
                        };
                        let changed: Vec<Field> = FIELDS.into_iter().filter(|f| f.differs(occurrence, &theirs)).collect();
                        if !changed.is_empty() {
                            return Ok(Delivered::Conflict { version: Some(now), lost: changed });
                        }
                        held = held_as(&Held { remote_id: now.remote_id.clone(), etag: now.etag.clone() }, occurrence, now.raw.clone());
                    }
                }
            }
            Err(Error::Other("it kept changing on the server".into()))
        }
        Write::Delete(events) => {
            let Some(first) = events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()) else { return Ok(Delivered::Done { version: None, merged: false }) };
            let Some(held) = &op.held else { return Err(Error::Other("a delete without the version it removes".into())) };
            let mut held = held_as(held, first, None);
            for _ in 0..MERGE_ROUNDS {
                match remote.write(&OpWrite::Delete { held: &held }).await? {
                    Written::Saved(_) | Written::Gone => return Ok(Delivered::Done { version: None, merged: false }),
                    Written::Stale => {
                        let Some(now) = remote.fetch(&held.remote_id, &first.uid, &first.alarms).await? else { return Ok(Delivered::Done { version: None, merged: false }) };
                        let Some(theirs) = now.events.iter().find(|e| same_row(e, first)) else { return Ok(Delivered::Done { version: None, merged: false }) };
                        let changed: Vec<Field> = FIELDS.into_iter().filter(|f| f.differs(first, theirs)).collect();
                        if !changed.is_empty() {
                            return Ok(Delivered::Conflict { version: Some(now), lost: changed });
                        }
                        held = held_as(&Held { remote_id: now.remote_id.clone(), etag: now.etag.clone() }, first, None);
                    }
                }
            }
            Err(Error::Other("it kept changing on the server".into()))
        }
    }
}

/// The row of `events`, one server object, that `mine` is: the same row,
/// else for an occurrence without one the series moved there, while the
/// series still has that occurrence.
fn their_row(events: &[EventData], mine: &EventData) -> Option<EventData> {
    if let Some(row) = events.iter().find(|e| same_row(e, mine)) {
        return Some(row.clone());
    }
    let rid = mine.recurrence_id.as_ref()?;
    let here = super::tz::here();
    let master = events.iter().find(|e| e.uid == mine.uid && e.recurrence_id.is_none())?;
    is_occurrence(master, rid, here).then(|| occurrences::occurrence(std::slice::from_ref(master), rid, here)).flatten()
}

/// Whether two events are the same row of a series: the same UID and the
/// same recurrence id, compared as moments.
fn same_row(a: &EventData, b: &EventData) -> bool {
    let here = super::tz::here();
    a.uid == b.uid
        && match (&a.recurrence_id, &b.recurrence_id) {
            (None, None) => true,
            (Some(x), Some(y)) => x.names(y, here),
            _ => false,
        }
}

/// A calendar on this Mac only, shown and the user's own.
pub fn new_local_calendar(conn: &Connection, name: &str, color: Option<&str>) -> Result<Calendar> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Other("A calendar needs a name".into()));
    }
    let remote = store::RemoteCalendar {
        remote_id: format!("local:{}", random_hex(16)?),
        name: name.into(),
        color: color.map(Into::into),
        zone: None,
        access: super::Access::Owner,
        primary: true,
        refresh: None,
    };
    store::add_calendar(conn, CalendarSource::Local, &remote)
}

/// A new event's UID.
pub fn new_uid() -> Result<String> {
    Ok(format!("{}@jolta.app", random_hex(16)?))
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    Ok(hex::encode(buf))
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Access, Local, Recurrence, Span, Zone};
    use crate::store::{RemoteCalendar, Store};

    const NOW: i64 = 1_795_000_000;

    fn horizon() -> Horizon {
        Horizon::around(Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap(), Brussels)
    }

    fn setup(name: &str) -> (Store, CalendarId) {
        let dir = std::env::temp_dir().join(format!("jolta-cal-ops-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        let cal = s.write(|tx| new_local_calendar(tx, "Test", Some("#3366ff"))).unwrap().id;
        (s, cal)
    }

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn event(uid: &str, title: &str, start: &str) -> EventData {
        let start = at(start);
        EventData { title: title.into(), ..EventData::new(uid, Span::Timed { start, end: Local { wall: start.wall + chrono::Duration::minutes(30), ..start } }) }
    }

    fn q(s: &Store, cal: CalendarId, change: Change) -> Option<CalendarOpId> {
        s.write(|tx| queue(tx, &Edit::quiet(cal, change), NOW, &horizon())).unwrap()
    }

    /// (start, title) of every occurrence in November 2026.
    fn shown(s: &Store) -> Vec<(String, String)> {
        let day = |d| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        s.read(|c| store::occurrences(c, day(1), NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(), Brussels))
            .unwrap()
            .into_iter()
            .map(|o| match o.bounds {
                crate::calendar::Bounds::Utc { start, .. } => (start.with_timezone(&Brussels).format("%d %H:%M").to_string(), o.title),
                crate::calendar::Bounds::Days { start, .. } => (format!("day {start}"), o.title),
            })
            .collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn not_synced(s: &Store, cal: CalendarId, uid: &str) -> Vec<bool> {
        s.read(|c| Ok(c.prepare("SELECT changed_locally FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY id")?
            .query_map(params![cal, uid], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?))
            .unwrap()
    }

    fn claim(s: &Store) -> Vec<CalendarOp> {
        s.write(|tx| store::claim_calendar_ops(tx, NOW + 3600, AT_ONCE)).unwrap()
    }

    fn send_all(s: &Store, result: std::result::Result<(), String>) -> Vec<Settled> {
        let sent = result.map(|()| Sent::Done { version: None, merged: false });
        claim(s).iter().map(|op| s.write(|tx| settle(tx, op, sent.clone(), NOW, &horizon())).unwrap()).collect()
    }

    fn state(s: &Store, id: CalendarOpId) -> (OpState, bool) {
        let op = s.read(|c| store::calendar_op(c, id)).unwrap();
        (op.state, op.tried)
    }

    #[test]
    fn create_edit_delete_show_at_once_and_settle() {
        let (s, cal) = setup("flow");
        let create = q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]), "the create shows before anything is sent");
        assert_eq!(not_synced(&s, cal, "u1"), [true]);

        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 10:00"))).unwrap();
        assert_eq!(shown(&s), pairs(&[("24 10:00", "Daily")]));

        let settled = send_all(&s, Ok(()));
        assert_eq!(settled.iter().map(|x| x.op).collect::<Vec<_>>(), [create], "one op per event at a time, oldest first");
        assert_eq!(not_synced(&s, cal, "u1"), [true], "the edit still waits");
        send_all(&s, Ok(()));
        assert_eq!((state(&s, create), state(&s, edit)), ((OpState::Done, true), (OpState::Done, true)));
        assert_eq!(not_synced(&s, cal, "u1"), [false], "nothing waits: the mark goes");

        let delete = q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        assert!(shown(&s).is_empty());
        send_all(&s, Ok(()));
        assert_eq!(state(&s, delete).0, OpState::Done);
    }

    #[test]
    fn the_same_edit_twice_applies_once() {
        let (s, cal) = setup("twice");
        let e = event("u1", "Standup", "2026-11-24 09:00");
        assert!(q(&s, cal, Change::Create(vec![e.clone()])).is_some());
        assert_eq!(q(&s, cal, Change::Create(vec![e.clone()])), None, "the same create again");
        let moved = event("u1", "Standup", "2026-11-24 11:00");
        assert!(q(&s, cal, Change::Update(moved.clone())).is_some());
        assert_eq!(q(&s, cal, Change::Update(moved)), None, "the same update again");
        assert!(q(&s, cal, Change::Delete { uid: "u1".into() }).is_some());
        assert_eq!(q(&s, cal, Change::Delete { uid: "u1".into() }), None, "the same delete again");
        let ops = s.read(|c| store::calendar_ops(c, 10)).unwrap();
        assert_eq!(ops.iter().map(|o| o.write.kind()).collect::<Vec<_>>(), ["delete", "update", "create"]);
        assert!(s.write(|tx| queue(tx, &Edit::quiet(cal, Change::Create(vec![event("u1", "Other", "2026-11-25 09:00"), event("u1", "Other", "2026-11-25 09:00")])), NOW, &horizon())).is_ok());
    }

    #[test]
    fn undo_before_the_worker_claims_it_drops_the_op_and_restores() {
        let (s, cal) = setup("undo-early");
        let h = horizon();
        let create = q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        send_all(&s, Ok(()));

        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 10:00"))).unwrap();
        assert_eq!(s.write(|tx| undo(tx, edit, NOW, &h)).unwrap(), Undone::Dropped);
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]), "the edit is taken back");
        assert!(s.read(|c| store::calendar_op(c, edit)).is_err(), "the op is gone, so nothing is sent");
        assert_eq!(not_synced(&s, cal, "u1"), [false]);

        let delete = q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        assert_eq!(s.write(|tx| undo(tx, delete, NOW, &h)).unwrap(), Undone::Dropped);
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]), "the deleted event is back");

        let second = q(&s, cal, Change::Create(vec![event("u2", "Lunch", "2026-11-25 12:00")])).unwrap();
        assert_eq!(s.write(|tx| undo(tx, second, NOW, &h)).unwrap(), Undone::Dropped);
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]), "the new event is gone");
        let third = q(&s, cal, Change::Create(vec![event("u3", "Coffee", "2026-11-26 15:00")])).unwrap();
        assert!(third.0 > second.0, "a dropped op's id, an undo token, is never given out again");
        assert!(s.write(|tx| undo(tx, second, NOW, &h)).is_err(), "so its stale token undoes nothing");
        assert_eq!(state(&s, create).0, OpState::Done);
    }

    #[test]
    fn undo_after_the_worker_claimed_it_queues_the_opposite() {
        let (s, cal) = setup("undo-late");
        let h = horizon();
        q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        send_all(&s, Ok(()));
        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 10:00"))).unwrap();
        send_all(&s, Ok(()));

        let Undone::Queued(inverse) = s.write(|tx| undo(tx, edit, NOW, &h)).unwrap() else { panic!("a sent op gets its opposite") };
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]));
        let op = s.read(|c| store::calendar_op(c, inverse)).unwrap();
        assert_eq!(op.write, Write::Update { base: event("u1", "Daily", "2026-11-24 10:00"), event: event("u1", "Standup", "2026-11-24 09:00"), dropped: Vec::new(), fresh: false });
        assert!(s.write(|tx| undo(tx, edit, NOW, &h)).is_err(), "undoing twice queues nothing more");
        send_all(&s, Ok(()));

        let delete = q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        send_all(&s, Ok(()));
        let Undone::Queued(recreate) = s.write(|tx| undo(tx, delete, NOW, &h)).unwrap() else { panic!() };
        assert_eq!(s.read(|c| store::calendar_op(c, recreate)).unwrap().write.kind(), "create");
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]));
    }

    /// Check 4: a crash between claim and finish leaves the op tried, so
    /// Undo can't assume it never reached the server.
    #[test]
    fn a_crash_mid_send_leaves_the_op_tried_and_undo_inverts() {
        let (s, cal) = setup("crash");
        let h = horizon();
        let create = q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        assert_eq!(claim(&s).len(), 1);
        s.write(|tx| store::release_calendar_claims(tx)).unwrap();
        assert_eq!(state(&s, create), (OpState::Pending, true));
        let Undone::Queued(inverse) = s.write(|tx| undo(tx, create, NOW, &h)).unwrap() else { panic!("a tried create gets a delete") };
        assert!(shown(&s).is_empty());
        let order: Vec<CalendarOpId> = claim(&s).iter().map(|o| o.id).collect();
        assert_eq!(order, [create], "the delete waits until the create it undoes settles");
        s.write(|tx| store::release_calendar_claims(tx)).unwrap();
        send_all(&s, Ok(()));
        send_all(&s, Ok(()));
        assert_eq!((state(&s, create).0, state(&s, inverse).0), (OpState::Done, OpState::Done));
    }

    #[test]
    fn an_op_failing_for_good_puts_the_old_version_back() {
        let (s, cal) = setup("fail");
        q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        send_all(&s, Ok(()));
        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 10:00"))).unwrap();
        for attempt in 1..MAX_ATTEMPTS {
            let settled = send_all(&s, Err("timed out".into()));
            assert_eq!(settled.len(), 1, "attempt {attempt}");
            assert!(settled[0].failure.is_none());
            let op = s.read(|c| store::calendar_op(c, edit)).unwrap();
            assert_eq!((op.state, op.attempts), (OpState::Pending, attempt));
            assert!(op.send_after > NOW, "it backs off");
            assert_eq!(shown(&s), pairs(&[("24 10:00", "Daily")]), "the edit shows while it is retried");
        }
        let settled = send_all(&s, Err("timed out".into()));
        let failure = settled[0].failure.clone().expect("the fifth failure is final");
        assert_eq!(failure.message, "Jolta couldn't save the change to Daily: timed out");
        assert!(failure.event.is_some());
        assert_eq!(state(&s, edit).0, OpState::Failed);
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]), "back to the version before the edit");
        assert_eq!(not_synced(&s, cal, "u1"), [false]);
        assert!(s.write(|tx| undo(tx, edit, NOW, &horizon())).is_err(), "a failed op has nothing to undo");
    }

    #[test]
    fn a_failed_op_keeps_the_later_edit_waiting_behind_it() {
        let (s, cal) = setup("fail-later");
        q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        send_all(&s, Ok(()));
        let first = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 10:00"))).unwrap();
        let second = q(&s, cal, Change::Update(event("u1", "Daily sync", "2026-11-24 11:00"))).unwrap();
        for _ in 0..MAX_ATTEMPTS {
            send_all(&s, Err("timed out".into()));
        }
        assert_eq!(state(&s, first).0, OpState::Failed);
        assert_eq!(shown(&s), pairs(&[("24 11:00", "Daily sync")]), "the later edit stays in the store");
        send_all(&s, Ok(()));
        assert_eq!(state(&s, second).0, OpState::Done);
        assert_eq!(shown(&s), pairs(&[("24 11:00", "Daily sync")]));
    }

    #[test]
    fn undo_after_a_failed_edit_goes_back_to_the_version_before_it() {
        let (s, cal) = setup("fail-undo");
        q(&s, cal, Change::Create(vec![event("u1", "A", "2026-11-24 09:00")])).unwrap();
        send_all(&s, Ok(()));
        q(&s, cal, Change::Update(event("u1", "B", "2026-11-24 10:00"))).unwrap();
        let c = q(&s, cal, Change::Update(event("u1", "C", "2026-11-24 11:00"))).unwrap();
        for _ in 0..MAX_ATTEMPTS {
            send_all(&s, Err("timed out".into()));
        }
        assert_eq!(s.write(|tx| undo(tx, c, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(shown(&s), pairs(&[("24 09:00", "A")]), "B never saved, so undoing C goes back to A");
    }

    /// A failed change to the series rolls back though an edit of one
    /// occurrence waits: that edit rewrites another row.
    #[test]
    fn a_failed_series_edit_rolls_back_past_an_occurrence_edit() {
        let (s, cal) = setup("fail-series");
        let series = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=3".into()], ..Default::default() }), ..event("u1", "Standup", "2026-11-10 09:00") };
        let moved = EventData { recurrence_id: Some(Moment::Time(at("2026-11-17 09:00"))), ..event("u1", "Moved", "2026-11-17 14:00") };
        q(&s, cal, Change::Create(vec![series.clone(), moved.clone()])).unwrap();
        send_all(&s, Ok(()));
        q(&s, cal, Change::Update(EventData { title: "Daily".into(), ..series })).unwrap();
        let occurrence = q(&s, cal, Change::Update(EventData { title: "Moved later".into(), ..moved })).unwrap();
        for _ in 0..MAX_ATTEMPTS {
            send_all(&s, Err("timed out".into()));
        }
        send_all(&s, Ok(()));
        assert_eq!(state(&s, occurrence).0, OpState::Done);
        assert_eq!(shown(&s), pairs(&[("10 09:00", "Standup"), ("17 14:00", "Moved later"), ("24 09:00", "Standup")]), "the series is back, the occurrence edit stays");
    }

    /// A create that never reached the server takes the edits queued
    /// behind it down with it, instead of leaving them to finish on nothing.
    #[test]
    fn a_failed_create_fails_the_edits_behind_it() {
        let (s, cal) = setup("fail-create");
        let series = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=3".into()], ..Default::default() }), ..event("u1", "Standup", "2026-11-10 09:00") };
        let moved = EventData { recurrence_id: Some(Moment::Time(at("2026-11-17 09:00"))), ..event("u1", "Moved", "2026-11-17 14:00") };
        let create = q(&s, cal, Change::Create(vec![series, moved.clone()])).unwrap();
        let edit = q(&s, cal, Change::Update(EventData { title: "Moved later".into(), ..moved })).unwrap();
        for _ in 0..MAX_ATTEMPTS {
            send_all(&s, Err("timed out".into()));
        }
        assert_eq!((state(&s, create).0, state(&s, edit).0), (OpState::Failed, OpState::Failed));
        assert!(shown(&s).is_empty(), "the event that was never saved is gone");
        assert!(claim(&s).is_empty(), "nothing is left to send");
    }

    #[test]
    fn the_next_due_time_skips_ops_waiting_behind_a_held_one() {
        let (s, cal) = setup("due");
        let held = q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        s.write(|tx| store::hold_calendar_op(tx, held, NOW + 600)).unwrap();
        q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 10:00"))).unwrap();
        assert_eq!(s.read(store::next_calendar_op_due).unwrap(), Some(NOW + 600), "the edit behind it isn't due before the held op");
    }

    /// Check 2: changing one occurrence writes its own row; the series row
    /// and the other occurrences stay.
    #[test]
    fn an_op_on_one_occurrence_never_touches_the_series() {
        let (s, cal) = setup("occurrence");
        let series = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=3".into()], ..Default::default() }), ..event("u1", "Standup", "2026-11-10 09:00") };
        let moved = EventData { recurrence_id: Some(Moment::Time(at("2026-11-17 09:00"))), ..event("u1", "Standup, moved", "2026-11-17 14:00") };
        q(&s, cal, Change::Create(vec![series.clone(), moved.clone()])).unwrap();
        send_all(&s, Ok(()));
        let master_row = |s: &Store| s.read(|c| Ok(stored(c, cal, "u1", None)?.map(|x| (x.id, x.event)))).unwrap();
        let before = master_row(&s);

        let later = EventData { title: "Standup, later".into(), ..event("u1", "x", "2026-11-17 16:00") };
        let later = EventData { recurrence_id: moved.recurrence_id, ..later };
        let op = q(&s, cal, Change::Update(later)).unwrap();
        assert_eq!(s.read(|c| store::calendar_op(c, op)).unwrap().scope, Scope::This);
        assert_eq!(master_row(&s), before, "the series row is the same row with the same data");
        assert_eq!(shown(&s), pairs(&[("10 09:00", "Standup"), ("17 16:00", "Standup, later"), ("24 09:00", "Standup")]));

        assert_eq!(s.write(|tx| undo(tx, op, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(master_row(&s), before);
        assert_eq!(shown(&s), pairs(&[("10 09:00", "Standup"), ("17 14:00", "Standup, moved"), ("24 09:00", "Standup")]));

        q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        assert!(shown(&s).is_empty(), "deleting the series takes its changed occurrences");
        let undo_delete = s.read(|c| store::calendar_ops(c, 1)).unwrap()[0].id;
        s.write(|tx| undo(tx, undo_delete, NOW, &horizon())).unwrap();
        assert_eq!(shown(&s).len(), 3, "and undo brings them all back");
    }

    fn remote_calendar(s: &Store, source: CalendarSource, access: Access) -> CalendarId {
        s.write(|tx| {
            store::add_calendar(tx, source, &RemoteCalendar { remote_id: format!("{source}-{access}"), name: format!("{source}"), color: None, zone: None, access, primary: true, refresh: None })
        })
        .unwrap()
        .id
    }

    /// Check 1: a write only goes to a calendar the user can write, and
    /// meetings with guests wait for the Send an update? dialog.
    #[test]
    fn only_writable_calendars_take_ops_and_meetings_with_guests_wait() {
        let (s, _) = setup("refused");
        for cal in [remote_calendar(&s, CalendarSource::Webcal, Access::Read), remote_calendar(&s, CalendarSource::Google, Access::Read)] {
            let e = Edit::quiet(cal, Change::Create(vec![event("u9", "Nope", "2026-11-24 09:00")]));
            assert!(s.write(|tx| queue(tx, &e, NOW, &horizon())).is_err());
        }
        let google = remote_calendar(&s, CalendarSource::Google, Access::Owner);
        let guest = crate::calendar::Attendee {
            person: crate::calendar::Person { address: "ann@example.com".into(), name: "Ann".into() },
            role: crate::calendar::Role::Required,
            response: crate::calendar::Response::NeedsAction,
            comment: String::new(),
            is_me: false,
        };
        let meeting = EventData { attendees: vec![guest], ..event("u8", "Review", "2026-11-24 09:00") };
        let err = s.write(|tx| queue(tx, &Edit::quiet(google, Change::Create(vec![meeting])), NOW, &horizon())).unwrap_err();
        assert!(err.to_string().contains("meetings with guests"), "{err}");
        let oof = EventData { categories: vec!["Away".into()], ..event("u7", "Away", "2026-11-24 09:00") };
        let err = s.write(|tx| queue(tx, &Edit::quiet(google, Change::Create(vec![oof])), NOW, &horizon())).unwrap_err();
        assert!(err.to_string().contains("can't hold categories"), "refused at once, not after five tries: {err}");
        assert!(shown(&s).is_empty(), "nothing was written");
        assert!(s.write(|tx| queue(tx, &Edit::quiet(google, Change::Create(vec![event("u6", "Lunch", "2026-11-24 12:00")])), NOW, &horizon())).unwrap().is_some());
    }

    /// A calendar server for the op tests: objects by remote id, each with
    /// a version that is its ETag, guarded writes, and a log of requests.
    #[derive(Default)]
    struct Server {
        objects: std::sync::Mutex<std::collections::BTreeMap<String, (u32, EventData)>>,
        /// Creates answered with an id of the server's own, as Graph does.
        names_itself: bool,
        /// Writes that reach the server and then lose their answer.
        lose_answers: std::sync::Mutex<u32>,
        log: std::sync::Mutex<Vec<String>>,
    }

    impl Server {
        fn put(&self, id: &str, e: EventData) {
            let mut objects = self.objects.lock().unwrap();
            let version = objects.get(id).map_or(1, |(v, _)| v + 1);
            objects.insert(id.into(), (version, e));
        }

        fn edit(&self, id: &str, change: impl FnOnce(&mut EventData)) {
            let mut e = self.objects.lock().unwrap()[id].1.clone();
            change(&mut e);
            self.put(id, e);
        }

        fn get(&self, id: &str) -> Option<EventData> {
            self.objects.lock().unwrap().get(id).map(|(_, e)| e.clone())
        }

        fn version(&self, id: &str) -> Option<Version> {
            self.objects.lock().unwrap().get(id).map(|(v, e)| Version { remote_id: id.into(), etag: Some(v.to_string()), events: vec![e.clone()], raw: None })
        }

        fn log(&self) -> Vec<String> {
            std::mem::take(&mut self.log.lock().unwrap())
        }

        fn answer(&self, v: Option<Version>) -> Result<Written> {
            let mut lose = self.lose_answers.lock().unwrap();
            if *lose > 0 {
                *lose -= 1;
                return Err(Error::Other("timed out".into()));
            }
            Ok(Written::Saved(v))
        }
    }

    impl Remote for Server {
        async fn write(&self, write: &OpWrite<'_>) -> Result<Written> {
            match *write {
                OpWrite::Create { events, key, .. } => {
                    self.log.lock().unwrap().push(format!("create {key}"));
                    let id = if self.names_itself { format!("server-{key}") } else { key.to_string() };
                    match self.get(&id) {
                        Some(e) if e.uid == events[0].uid => {}
                        Some(_) => return Err(Error::Other("taken".into())),
                        None => self.put(&id, events[0].clone()),
                    }
                    self.answer(self.version(&id))
                }
                OpWrite::Update { held, base, event, .. } => {
                    self.log.lock().unwrap().push(format!("update {} at {}", held.remote_id, held.etag.as_deref().unwrap_or("-")));
                    let Some(now) = self.version(&held.remote_id) else { return Ok(Written::Gone) };
                    if now.etag != held.etag {
                        return Ok(Written::Stale);
                    }
                    self.edit(&held.remote_id, |e| {
                        for f in FIELDS.into_iter().filter(|f| f.differs(base, event)) {
                            f.copy(e, event);
                        }
                    });
                    self.answer(self.version(&held.remote_id))
                }
                OpWrite::Occurrence { series, event, .. } => {
                    self.log.lock().unwrap().push(format!("occurrence of {} at {}", series.remote_id, series.etag.as_deref().unwrap_or("-")));
                    let Some(now) = self.version(&series.remote_id) else { return Ok(Written::Gone) };
                    if now.etag != series.etag {
                        return Ok(Written::Stale);
                    }
                    let id = format!("{}_instance", series.remote_id);
                    self.put(&id, event.clone());
                    self.answer(self.version(&id))
                }
                OpWrite::Cancel { held, .. } => {
                    self.log.lock().unwrap().push(format!("cancel in {} at {}", held.remote_id, held.etag.as_deref().unwrap_or("-")));
                    let Some(now) = self.version(&held.remote_id) else { return Ok(Written::Gone) };
                    if now.etag != held.etag {
                        return Ok(Written::Stale);
                    }
                    self.answer(None)
                }
                OpWrite::Delete { held } => {
                    self.log.lock().unwrap().push(format!("delete {} at {}", held.remote_id, held.etag.as_deref().unwrap_or("-")));
                    let Some(now) = self.version(&held.remote_id) else { return Ok(Written::Gone) };
                    if now.etag != held.etag {
                        return Ok(Written::Stale);
                    }
                    self.objects.lock().unwrap().remove(&held.remote_id);
                    self.answer(None)
                }
            }
        }

        async fn fetch(&self, remote_id: &str, _: &str, _: &[Alarm]) -> Result<Option<Version>> {
            self.log.lock().unwrap().push(format!("fetch {remote_id}"));
            Ok(self.version(remote_id))
        }
    }

    /// Sends every due op to `server`, as the worker does.
    fn run(s: &Store, server: &Server) -> Vec<Settled> {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        claim(s)
            .iter()
            .map(|op| {
                let sent = rt.block_on(deliver(server, op, None)).map_err(|e| e.to_string()).map(|d| match d {
                    Delivered::Done { version, merged } => Sent::Done { version: version.map(|v| (v, None)), merged },
                    Delivered::Conflict { version, lost } => Sent::Conflict { version: version.map(|v| (v, None)), lost },
                    Delivered::Gone => Sent::Gone,
                });
                s.write(|tx| settle(tx, op, sent, NOW, &horizon())).unwrap()
            })
            .collect()
    }

    /// A Google calendar holding "Standup" as `g1` at version 1, on the
    /// server and synced into the store.
    fn synced(name: &str) -> (Store, CalendarId, Server) {
        let (s, _) = setup(name);
        let cal = remote_calendar(&s, CalendarSource::Google, Access::Owner);
        let server = Server::default();
        server.put("g1", event("u1", "Standup", "2026-11-24 09:00"));
        sync(&s, cal, &server);
        (s, cal, server)
    }

    /// What a sync stores: every object as the server has it.
    fn sync(s: &Store, cal: CalendarId, server: &Server) {
        let objects = server.objects.lock().unwrap().clone();
        s.write(|tx| {
            for (id, (v, e)) in &objects {
                store::apply_item(tx, cal, id, Some(&v.to_string()), std::slice::from_ref(e), None, &horizon())?;
            }
            Ok(())
        })
        .unwrap();
    }

    fn shown_where(s: &Store) -> Vec<(String, String, String)> {
        s.read(|c| Ok(c.prepare("SELECT title, location, coalesce(etag, '-') FROM event ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?))
            .unwrap()
    }

    fn triples(list: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        list.iter().map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string())).collect()
    }

    /// The hole U1 left: a sync rewrote the rows of an edit that hadn't
    /// reached the server and cleared its mark. Now the edit stays, and the
    /// change made elsewhere meanwhile is merged in when the edit is sent.
    #[test]
    fn an_edit_waiting_to_be_sent_survives_a_sync_and_merges_with_it() {
        let (s, cal, server) = synced("survives");
        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 09:00"))).unwrap();
        server.edit("g1", |e| e.location = "Room 9".into());
        sync(&s, cal, &server);
        assert_eq!(shown_where(&s), triples(&[("Daily", "", "1")]), "the edit and the version it started from stay");
        assert_eq!(not_synced(&s, cal, "u1"), [true]);

        let settled = run(&s, &server);
        assert!(settled[0].failure.is_none());
        assert_eq!(server.log(), ["update g1 at 1", "fetch g1", "update g1 at 2"]);
        assert_eq!((server.get("g1").unwrap().title.as_str(), server.get("g1").unwrap().location.as_str()), ("Daily", "Room 9"), "both changes on the server");
        assert_eq!(shown_where(&s), triples(&[("Daily", "Room 9", "3")]));
        assert_eq!((state(&s, edit).0, not_synced(&s, cal, "u1")), (OpState::Done, vec![false]));
        assert!(s.read(|c| store::calendar_op(c, edit)).unwrap().lost.is_empty());

        sync(&s, cal, &server);
        assert_eq!(shown_where(&s), triples(&[("Daily", "Room 9", "3")]), "the next sync agrees");
    }

    /// A sync passes over the server's version of a row an op holds; when
    /// the op is then taken back, the next sync lists the calendar whole.
    #[test]
    fn taking_back_an_op_a_sync_passed_over_lists_the_calendar_again() {
        let (s, cal, server) = synced("passed-over");
        s.write(|tx| store::set_calendar_cursor(tx, cal, Some("token-1"), None)).unwrap();
        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 09:00"))).unwrap();
        sync(&s, cal, &server);
        assert_eq!(s.read(|c| store::calendar(c, cal)).unwrap().cursor.as_deref(), Some("token-1"), "an unchanged server passes nothing over");
        server.objects.lock().unwrap().remove("g1");
        s.write(|tx| store::remove_item(tx, cal, "g1", store::Removal::Deleted, &horizon())).unwrap();
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Daily")]), "the edit stays while it waits");
        assert_eq!(s.write(|tx| undo(tx, edit, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(s.read(|c| store::calendar(c, cal)).unwrap().cursor, None, "the next sync lists everything and finds it gone");

        let (s, cal, _) = synced("passed-over-delete");
        s.write(|tx| store::set_calendar_cursor(tx, cal, Some("token-1"), None)).unwrap();
        let delete = q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        s.write(|tx| store::remove_item(tx, cal, "g1", store::Removal::Deleted, &horizon())).unwrap();
        assert_eq!(s.write(|tx| undo(tx, delete, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(s.read(|c| store::calendar(c, cal)).unwrap().cursor, None, "a queued delete has no rows left, yet the removal it passed over counts");
    }

    #[test]
    fn a_delete_waiting_to_be_sent_stays_deleted_through_a_sync() {
        let (s, cal, server) = synced("delete-sync");
        q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        sync(&s, cal, &server);
        assert!(shown(&s).is_empty());
        run(&s, &server);
        assert_eq!(server.log(), ["delete g1 at 1"]);
        assert!(server.get("g1").is_none() && shown(&s).is_empty());
    }

    #[test]
    fn a_field_changed_on_both_sides_keeps_the_servers_until_apply_mine() {
        let (s, cal, server) = synced("conflict");
        let mine = event("u1", "Daily", "2026-11-24 10:00");
        let edit = q(&s, cal, Change::Update(mine)).unwrap();
        server.edit("g1", |e| e.title = "Weekly".into());

        let settled = run(&s, &server);
        let failure = settled[0].failure.clone().expect("the user hears of it");
        assert_eq!(failure.message, "Daily changed in Google Calendar while you edited it. Your new title wasn't saved.");
        let op = s.read(|c| store::calendar_op(c, edit)).unwrap();
        assert_eq!((op.state, op.lost.as_slice()), (OpState::Conflict, [Field::Title].as_slice()));
        assert_eq!(shown(&s), pairs(&[("24 10:00", "Weekly")]), "the server's title, and the new time, which only the user changed");
        assert_eq!(server.get("g1").unwrap().title, "Weekly");
        let notice = s.read(|c| conflict(c, cal, "u1")).unwrap().expect("the detail shows it");
        assert_eq!((notice.message.as_str(), notice.action.as_str()), ("Changed elsewhere while you edited. Your new title wasn't saved.", "Apply mine"));
        assert!(s.write(|tx| undo(tx, edit, NOW, &horizon())).is_err());

        let again = s.write(|tx| apply_mine(tx, edit, NOW, &horizon())).unwrap().expect("queued");
        assert_eq!(shown(&s), pairs(&[("24 10:00", "Daily")]));
        assert!(s.read(|c| conflict(c, cal, "u1")).unwrap().is_none(), "the notice goes");
        run(&s, &server);
        assert_eq!((state(&s, again).0, server.get("g1").unwrap().title.as_str()), (OpState::Done, "Daily"));
    }

    /// An edit queued behind one that merged a change made elsewhere hasn't
    /// seen that change: it meets it as a 412 of its own and keeps the
    /// server's value where both changed the same field.
    #[test]
    fn an_edit_behind_a_merged_one_still_meets_the_change_made_elsewhere() {
        let (s, cal, server) = synced("behind-merge");
        let first = q(&s, cal, Change::Update(EventData { location: "Room 2".into(), ..event("u1", "Standup", "2026-11-24 09:00") })).unwrap();
        let second = q(&s, cal, Change::Update(EventData { location: "Room 2".into(), ..event("u1", "Daily", "2026-11-24 09:00") })).unwrap();
        server.edit("g1", |e| e.title = "Weekly".into());
        run(&s, &server);
        assert_eq!(state(&s, first).0, OpState::Done);
        run(&s, &server);
        let op = s.read(|c| store::calendar_op(c, second)).unwrap();
        assert_eq!((op.state, op.lost.as_slice()), (OpState::Conflict, [Field::Title].as_slice()));
        assert_eq!(server.get("g1").unwrap().title, "Weekly", "the title changed elsewhere isn't overwritten");
        assert_eq!(server.get("g1").unwrap().location, "Room 2");
    }

    #[test]
    fn an_event_deleted_elsewhere_is_never_recreated() {
        let (s, cal, server) = synced("gone");
        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 09:00"))).unwrap();
        let later = q(&s, cal, Change::Update(event("u1", "Daily sync", "2026-11-24 09:00"))).unwrap();
        server.objects.lock().unwrap().remove("g1");
        let settled = run(&s, &server);
        assert_eq!(settled[0].failure.clone().unwrap().message, "Daily was deleted in Google Calendar while you edited it. Your change wasn't saved.");
        assert_eq!((state(&s, edit).0, state(&s, later).0), (OpState::Gone, OpState::Gone), "the edit behind it can't land either");
        assert!(shown(&s).is_empty(), "gone from the store");
        assert!(server.objects.lock().unwrap().is_empty(), "and not made again");
        assert!(claim(&s).is_empty());
    }

    /// A Google series `g1` of three from Tue 24 Nov, synced.
    fn synced_series(name: &str) -> (Store, CalendarId, Server, EventData) {
        let (s, _) = setup(name);
        let cal = remote_calendar(&s, CalendarSource::Google, Access::Owner);
        let server = Server::default();
        let series = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=3".into()], ..Default::default() }), ..event("u1", "Standup", "2026-11-24 09:00") };
        server.put("g1", series.clone());
        sync(&s, cal, &server);
        (s, cal, server, series)
    }

    /// Check 2: an unchanged occurrence is written as an instance of its
    /// series, guarded by the series' version, and lands under the id the
    /// server gave the instance; the series row stays as it was.
    #[test]
    fn an_unchanged_occurrence_is_written_as_an_instance_of_its_series() {
        let (s, cal, server, series) = synced_series("instance");
        let rid = Moment::Time(at("2026-12-01 09:00"));
        let moved = EventData { recurrence_id: Some(rid), recurrence: None, ..event("u1", "Standup", "2026-12-01 11:00") };
        let op = q(&s, cal, Change::Update(moved.clone())).unwrap();
        assert!(matches!(s.read(|c| store::calendar_op(c, op)).unwrap().write, Write::Update { fresh: true, .. }));
        run(&s, &server);
        assert_eq!(server.log(), ["occurrence of g1 at 1"]);
        assert_eq!(state(&s, op).0, OpState::Done);
        let rows: Vec<(String, String)> = s.read(|c| Ok(c.prepare("SELECT remote_id, recurrence_id FROM event ORDER BY id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)).unwrap();
        assert_eq!(rows.len(), 2, "the series and one changed occurrence: {rows:?}");
        assert!(rows.iter().any(|(id, rid)| id == "g1_instance" && !rid.is_empty()), "under the instance's own id: {rows:?}");
        assert_eq!(s.read(|c| stored(c, cal, "u1", None)).unwrap().unwrap().event, series);
    }

    /// An instance the server can't find while its series is still there
    /// takes back only the op's change, never the series.
    #[test]
    fn a_missing_instance_never_takes_its_series_out_of_the_store() {
        let (s, cal, server, series) = synced_series("instance-gone");
        let moved = EventData { recurrence_id: Some(Moment::Time(at("2026-12-01 09:00"))), recurrence: None, ..event("u1", "Standup", "2026-12-01 11:00") };
        let op = q(&s, cal, Change::Update(moved)).unwrap();
        // The series moved elsewhere, so the occurrence Jolta wrote to is gone.
        server.put("g1", EventData { span: event("u1", "Standup", "2026-11-25 09:00").span, ..series.clone() });
        let settled = run(&s, &server);
        assert_eq!(state(&s, op).0, OpState::Gone);
        assert!(settled[0].failure.is_some());
        assert!(s.read(|c| stored(c, cal, "u1", None)).unwrap().is_some(), "the series stays");
        assert_eq!(s.read(|c| series_rows(c, cal)).unwrap(), 1, "and the change is taken back");
    }

    fn series_rows(c: &Connection, cal: CalendarId) -> Result<i64> {
        Ok(c.query_row("SELECT count(*) FROM event WHERE calendar_id = ?1 AND uid = 'u1'", [cal], |r| r.get(0))?)
    }

    /// Check 4: a create whose first try reached the server but lost its
    /// answer is sent again to the same object, which keeps one event.
    #[test]
    fn a_create_whose_answer_was_lost_makes_one_event() {
        let (s, _) = setup("lost-answer");
        let cal = remote_calendar(&s, CalendarSource::Google, Access::Owner);
        let server = Server { lose_answers: std::sync::Mutex::new(1), ..Server::default() };
        let create = q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        assert!(run(&s, &server)[0].failure.is_none(), "a lost answer is tried again");
        assert_eq!(state(&s, create), (OpState::Pending, true));
        run(&s, &server);
        let key = sources::google::event_id("u1");
        assert_eq!(server.log(), [format!("create {key}"), format!("create {key}")]);
        assert_eq!(server.objects.lock().unwrap().len(), 1);
        assert_eq!(shown_where(&s), triples(&[("Standup", "", "1")]), "stored under the server's version");
        assert_eq!(state(&s, create).0, OpState::Done);
    }

    /// Graph names a new event itself: the stored rows move to its id, and
    /// an edit queued behind the create writes over the version it made.
    #[test]
    fn an_edit_behind_a_create_writes_over_the_object_the_server_named() {
        let (s, _) = setup("named");
        let cal = remote_calendar(&s, CalendarSource::Graph, Access::Owner);
        let server = Server { names_itself: true, ..Server::default() };
        q(&s, cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])).unwrap();
        let edit = q(&s, cal, Change::Update(event("u1", "Daily", "2026-11-24 09:00"))).unwrap();
        run(&s, &server);
        assert_eq!(shown_where(&s), triples(&[("Daily", "", "1")]), "the edit stays shown over the created version");
        run(&s, &server);
        let log = server.log();
        let id = log[0].trim_start_matches("create ").to_string();
        assert_eq!(log, [format!("create {id}"), format!("update server-{id} at 1")]);
        assert_eq!((state(&s, edit).0, server.get(&format!("server-{id}")).unwrap().title.as_str()), (OpState::Done, "Daily"));
        assert_eq!(s.read(|c| Ok(c.query_row("SELECT remote_id FROM event", [], |r| r.get::<_, String>(0))?)).unwrap(), format!("server-{id}"));
    }

    #[test]
    fn a_delete_of_an_event_changed_elsewhere_keeps_it() {
        let (s, cal, server) = synced("delete-conflict");
        let delete = q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        server.edit("g1", |e| e.title = "Weekly".into());
        let settled = run(&s, &server);
        assert_eq!(settled[0].failure.clone().unwrap().message, "Standup changed in Google Calendar after you deleted it, so it wasn't deleted.");
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Weekly")]), "back, as the server has it");
        assert_eq!(s.read(|c| conflict(c, cal, "u1")).unwrap().unwrap().action, "Delete anyway");
        s.write(|tx| apply_mine(tx, delete, NOW, &horizon())).unwrap();
        run(&s, &server);
        assert!(server.get("g1").is_none() && shown(&s).is_empty());

        let (s, cal, server) = synced("delete-same");
        q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        server.edit("g1", |_| {});
        run(&s, &server);
        assert_eq!(server.log(), ["delete g1 at 1", "fetch g1", "delete g1 at 2"], "a new ETag over the same event deletes it");
        assert!(server.get("g1").is_none());
    }

    #[test]
    fn an_op_that_emails_waits_for_the_undo_window() {
        let (s, cal) = setup("window");
        let edit = Edit { notify: Notify::All, ..Edit::quiet(cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])) };
        let id = s.write(|tx| queue(tx, &edit, NOW, &horizon())).unwrap().unwrap();
        assert_eq!(s.read(|c| store::calendar_op(c, id)).unwrap().send_after, NOW + UNDO_SEND_SECONDS);
        assert!(s.write(|tx| store::claim_calendar_ops(tx, NOW, AT_ONCE)).unwrap().is_empty(), "not due inside the window");
        assert_eq!(claim(&s).len(), 1);
        s.write(|tx| settle(tx, &store::calendar_op(tx, id)?, Ok(Sent::Done { version: None, merged: false }), NOW, &horizon())).unwrap();
        assert!(s.write(|tx| undo(tx, id, NOW, &horizon())).is_err(), "an update that went out can't be taken back");
    }
}
