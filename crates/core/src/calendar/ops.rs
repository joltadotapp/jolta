//! The calendar op queue: an edit changes the store at once and reaches the
//! server later, with undo and offline.
//!
//! `queue` writes the edit into the event tables (the optimistic change),
//! marks the event "not synced yet" and stores an op. The calendar
//! scheduler's worker (`run_due`) sends due ops, one at a time per event and
//! in order, and `settle` records how each went. A local calendar has no
//! server: the store is its truth, so its ops only carry undo.

use std::fmt;
use std::str::FromStr;

use rusqlite::{Connection, OptionalExtension, params};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};

use super::change::Field;
use super::series::Horizon;
use super::{Calendar, CalendarId, CalendarSource, EventData, EventId, Moment, caps};
use crate::model::text_enum;
use crate::{Error, Jolta, Result, store};

/// Ops that email people wait the undo window; ops fail for good after
/// as many attempts as mail ops.
pub use crate::engine::{MAX_ATTEMPTS, UNDO_SEND_SECONDS};

/// Ops sent at once, across events.
pub const AT_ONCE: usize = 4;

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

/// What an op does to one event, with what undo needs to take it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// A new event, or a series with its changed occurrences.
    Create(Vec<EventData>),
    /// One event, series or changed occurrence: `base` as it was, `event`
    /// as the user saved it.
    Update { base: EventData, event: EventData },
    /// A whole event or series with its changed occurrences, as it was.
    Delete(Vec<EventData>),
}

impl Write {
    pub fn kind(&self) -> &'static str {
        match self {
            Write::Create(_) => "create",
            Write::Update { .. } => "update",
            Write::Delete(_) => "delete",
        }
    }

    /// The event the user sees: the one saved, or the one deleted.
    pub fn event(&self) -> Option<&EventData> {
        match self {
            Write::Create(events) | Write::Delete(events) => events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()),
            Write::Update { event, .. } => Some(event),
        }
    }

    /// The change that takes this one back.
    fn undo(&self) -> Change {
        match self {
            Write::Create(events) => Change::Delete { uid: events.first().map(|e| e.uid.clone()).unwrap_or_default() },
            Write::Update { base, .. } => Change::Update(base.clone()),
            Write::Delete(events) => Change::Create(events.clone()),
        }
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
    /// Makes a retried create land on the same object: the UID for a local
    /// calendar.
    pub remote_key: String,
    pub send_after: i64,
    pub state: OpState,
    /// Claimed by the worker at least once, so it may have reached the server.
    pub tried: bool,
    pub attempts: u32,
    pub error: Option<String>,
    /// Fields the server kept on a 412 merge.
    pub lost: Vec<Field>,
    pub created: i64,
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
/// Nothing here waits on the network.
pub fn queue(conn: &Connection, edit: &Edit, now: i64, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    let calendar = store::calendar(conn, edit.calendar)?;
    if !caps::of(&calendar).writable {
        return Err(Error::Other(format!("{} is read-only", calendar.name)));
    }
    if calendar.source != CalendarSource::Local {
        return Err(Error::Other(format!("Jolta can't change events in {} yet", calendar.name)));
    }
    let Some((write, remote_key)) = plan(conn, &calendar, &edit.change)? else { return Ok(None) };
    let first = write.event().ok_or_else(|| Error::Other("an edit without an event".into()))?;
    let (uid, recurrence_id) = (first.uid.clone(), if matches!(write, Write::Update { .. }) { first.recurrence_id } else { None });
    let scope = if recurrence_id.is_some() { Scope::This } else { Scope::All };
    let delay = if edit.notify == Notify::Nobody { 0 } else { UNDO_SEND_SECONDS };
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
            send_after: now + delay,
            state: OpState::Pending,
            tried: false,
            attempts: 0,
            error: None,
            lost: Vec::new(),
            created: now,
        },
    )?;
    store::refresh_changed_locally(conn, calendar.id, &uid)?;
    Ok(Some(id))
}

/// The write a change makes against what the store holds, with the key a
/// retried create reuses. None when the store already shows the change.
fn plan(conn: &Connection, calendar: &Calendar, change: &Change) -> Result<Option<(Write, String)>> {
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
            Ok(Some((Write::Create(events.clone()), remote_key(calendar, &uid))))
        }
        Change::Update(event) => {
            let base = stored(conn, calendar.id, &event.uid, event.recurrence_id.as_ref())?
                .ok_or_else(|| Error::NotFound(format!("event {} in {}", event.uid, calendar.name)))?
                .event;
            if base == *event {
                return Ok(None);
            }
            Ok(Some((Write::Update { base, event: event.clone() }, remote_key(calendar, &event.uid))))
        }
        Change::Delete { uid } => {
            let stored = series(conn, calendar.id, uid)?;
            if stored.is_empty() {
                return Ok(None);
            }
            Ok(Some((Write::Delete(stored), remote_key(calendar, uid))))
        }
    }
}

fn same_events(a: &[EventData], b: &[EventData]) -> bool {
    a.len() == b.len() && a.iter().all(|e| b.contains(e))
}

/// Where a retried create lands. A local calendar's events are filed under
/// their UID, as `jolta cal import` files them.
fn remote_key(_calendar: &Calendar, uid: &str) -> String {
    uid.to_string()
}

/// A stored event with what the store keeps beside it.
pub struct Stored {
    pub id: EventId,
    pub remote_id: String,
    pub etag: Option<String>,
    pub event: EventData,
}

/// The stored event with this UID and recurrence id: the series or single
/// event without one.
pub fn stored(conn: &Connection, calendar: CalendarId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<Stored>> {
    let rid = recurrence_id.map(store::moment_text).unwrap_or_default();
    conn.query_row(
        "SELECT id, remote_id, etag, data FROM event WHERE calendar_id = ?1 AND uid = ?2 AND recurrence_id = ?3 ORDER BY id LIMIT 1",
        params![calendar, uid, rid],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?)),
    )
    .optional()?
    .map(|(id, remote_id, etag, data)| {
        let event = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))?;
        Ok(Stored { id, remote_id, etag, event })
    })
    .transpose()
}

/// Every stored event with this UID: a series with its changed occurrences.
fn series(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<Vec<EventData>> {
    let mut stmt = conn.prepare_cached("SELECT data FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY recurrence_id <> '', id")?;
    let rows = stmt.query_map(params![calendar, uid], |r| r.get::<_, String>(0))?;
    rows.map(|data| serde_json::from_str(&data?).map_err(|e| Error::Other(format!("stored event {uid}: {e}")))).collect()
}

/// Writes `write` into the event tables and rebuilds the series' occurrences.
fn apply(conn: &Connection, calendar: CalendarId, write: &Write, remote_key: &str, horizon: &Horizon) -> Result<()> {
    let uid = match write {
        Write::Create(events) => {
            for e in events {
                store::insert_event(conn, calendar, remote_key, None, e, None)?;
            }
            events.first().map(|e| e.uid.clone())
        }
        Write::Update { event, .. } => {
            let (remote_id, etag) = match stored(conn, calendar, &event.uid, event.recurrence_id.as_ref())? {
                Some(s) => (s.remote_id, s.etag),
                None => (stored(conn, calendar, &event.uid, None)?.map_or_else(|| remote_key.to_string(), |s| s.remote_id), None),
            };
            store::insert_event(conn, calendar, &remote_id, etag.as_deref(), event, None)?;
            Some(event.uid.clone())
        }
        Write::Delete(events) => {
            let uid = events.first().map(|e| e.uid.clone());
            conn.execute("DELETE FROM event WHERE calendar_id = ?1 AND uid = ?2", params![calendar, uid])?;
            uid
        }
    };
    if let Some(uid) = uid {
        store::rebuild_series(conn, calendar, &uid, horizon)?;
    }
    Ok(())
}

/// Puts the store back as it was before `op`.
fn revert(conn: &Connection, op: &CalendarOp, horizon: &Horizon) -> Result<()> {
    let back = match &op.write {
        Write::Create(events) => Write::Delete(events.clone()),
        Write::Update { base, event } => Write::Update { base: event.clone(), event: base.clone() },
        Write::Delete(events) => Write::Create(events.clone()),
    };
    apply(conn, op.calendar, &back, &op.remote_key, horizon)
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
    match op.state {
        OpState::Pending if !op.tried => {
            revert(conn, &op, horizon)?;
            store::delete_calendar_op(conn, id)?;
            store::refresh_changed_locally(conn, op.calendar, &op.uid)?;
            Ok(Undone::Dropped)
        }
        OpState::Pending | OpState::Sending | OpState::Done if op.notify == Notify::Nobody => {
            let edit = Edit::quiet(op.calendar, op.write.undo());
            match queue(conn, &edit, now, horizon)? {
                Some(inverse) => Ok(Undone::Queued(inverse)),
                None => Err(Error::Other(format!("{title} is already as it was"))),
            }
        }
        OpState::Pending | OpState::Sending | OpState::Done => Err(Error::Other(format!("The update to {title} already went out"))),
        OpState::Failed | OpState::Conflict | OpState::Gone => Err(Error::Other(format!("The change to {title} wasn't saved; there is nothing to undo"))),
    }
}

/// What came of one attempt to send an op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    pub op: CalendarOpId,
    pub calendar: CalendarId,
    /// The op failed for good and its change was taken back.
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// "Jolta couldn't save the change to Standup: <reason>".
    pub message: String,
    /// The event as it is again, if it is still there.
    pub event: Option<EventId>,
}

/// Records how sending a claimed op went. A failed attempt waits and is
/// tried again, backing off as sync does; after `MAX_ATTEMPTS` the op
/// fails for good and the store goes back to the version before it.
pub fn settle(conn: &Connection, op: &CalendarOp, sent: std::result::Result<(), String>, now: i64, horizon: &Horizon) -> Result<Settled> {
    let mut failure = None;
    match sent {
        Ok(()) => store::set_calendar_op_state(conn, op.id, OpState::Done, None)?,
        Err(reason) if op.attempts + 1 < MAX_ATTEMPTS => {
            let wait = super::scheduler::backoff(op.attempts + 1, std::time::Duration::from_secs(300)).as_secs() as i64;
            store::fail_calendar_attempt(conn, op.id, &reason, Some(now + wait))?;
        }
        Err(reason) => {
            store::fail_calendar_attempt(conn, op.id, &reason, None)?;
            // A later edit of the event is in the store and carries this
            // one's change with it; putting the old version back would hide it.
            if !store::later_calendar_op_waits(conn, op)? {
                revert(conn, op, horizon)?;
            }
            let title = op.write.event().map(|e| e.title.as_str()).unwrap_or("the event");
            let event = stored(conn, op.calendar, &op.uid, op.recurrence_id.as_ref())?.map(|s| s.id);
            failure = Some(Failure { message: format!("Jolta couldn't save the change to {title}: {reason}"), event });
        }
    }
    store::refresh_changed_locally(conn, op.calendar, &op.uid)?;
    Ok(Settled { op: op.id, calendar: op.calendar, failure })
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
            let result = send(jolta, &op).await;
            (op, result)
        }))
        .await;
        for (op, result) in sent {
            let now = chrono::Utc::now().timestamp();
            out.push(jolta.store.write_async(move |tx| settle(tx, &op, result, now, &horizon)).await?);
        }
    }
}

/// Reaches the op's server. A local calendar's store is its server, which
/// `queue` already wrote.
async fn send(jolta: &Jolta, op: &CalendarOp) -> std::result::Result<(), String> {
    let id = op.calendar;
    let calendar = jolta.store.read_async(move |c| store::calendar(c, id)).await.map_err(|e| e.to_string())?;
    match calendar.source {
        CalendarSource::Local => Ok(()),
        _ => Err(format!("Jolta can't send changes to {} yet", calendar.name)),
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
        claim(s).iter().map(|op| s.write(|tx| settle(tx, op, result.clone(), NOW, &horizon())).unwrap()).collect()
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
        assert_eq!(op.write, Write::Update { base: event("u1", "Daily", "2026-11-24 10:00"), event: event("u1", "Standup", "2026-11-24 09:00") });
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

    #[test]
    fn only_writable_local_calendars_take_ops_for_now() {
        let (s, _) = setup("refused");
        let remote = |source, access| {
            s.write(|tx| {
                store::add_calendar(tx, source, &RemoteCalendar { remote_id: format!("{source}"), name: format!("{source}"), color: None, zone: None, access, primary: true, refresh: None })
            })
            .unwrap()
            .id
        };
        for cal in [remote(CalendarSource::Webcal, Access::Read), remote(CalendarSource::Google, Access::Owner)] {
            let e = Edit::quiet(cal, Change::Create(vec![event("u9", "Nope", "2026-11-24 09:00")]));
            assert!(s.write(|tx| queue(tx, &e, NOW, &horizon())).is_err());
        }
        assert!(shown(&s).is_empty(), "nothing was written");
    }

    #[test]
    fn an_op_that_emails_waits_for_the_undo_window() {
        let (s, cal) = setup("window");
        let edit = Edit { notify: Notify::All, ..Edit::quiet(cal, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")])) };
        let id = s.write(|tx| queue(tx, &edit, NOW, &horizon())).unwrap().unwrap();
        assert_eq!(s.read(|c| store::calendar_op(c, id)).unwrap().send_after, NOW + UNDO_SEND_SECONDS);
        assert!(s.write(|tx| store::claim_calendar_ops(tx, NOW, AT_ONCE)).unwrap().is_empty(), "not due inside the window");
        assert_eq!(claim(&s).len(), 1);
        s.write(|tx| settle(tx, &store::calendar_op(tx, id)?, Ok(()), NOW, &horizon())).unwrap();
        assert!(s.write(|tx| undo(tx, id, NOW, &horizon())).is_err(), "an update that went out can't be taken back");
    }
}
