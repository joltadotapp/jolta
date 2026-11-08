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
use super::sources::{self, OpWrite, Source, Tell, Version, Written};
use super::moving::{self, How};
use super::{Alarm, Calendar, CalendarId, CalendarSource, EventData, EventId, EventStatus, Moment, caps, description, merge, organizer, rsvp, series as occurrences};
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
    /// The event with its changed occurrences leaves `from` for the op's
    /// calendar. `rows` are its rows in `from` as they were, under the
    /// objects and versions they were filed with, which Undo puts back;
    /// `events` are what the op's calendar gets (`moving::plan`).
    Move { from: CalendarId, how: How, rows: Vec<Row>, events: Vec<EventData> },
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
            Write::Move { .. } => "move",
        }
    }

    /// The event the user sees: the one saved, or the one deleted.
    pub fn event(&self) -> Option<&EventData> {
        match self {
            Write::Create(events) | Write::Delete(events) | Write::Move { events, .. } => events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()),
            Write::Update { event, .. } | Write::Cancel { occurrence: event, .. } => Some(event),
        }
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
            Write::Create(_) | Write::Cancel { .. } | Write::Move { .. } => {}
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
            Write::Move { from, events, .. } => Some(Change::Move { uid: events.first().map(|e| e.uid.clone()).unwrap_or_default(), to: *from }),
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
    /// series only once the new one exists, so a failure loses nothing,
    /// and an edit saved with a move waits for the move.
    pub after: Option<CalendarOpId>,
}

impl CalendarOp {
    /// The object the op's rows are filed under in its calendar until it
    /// lands: the one it holds, or the key a create or a copy lands at.
    fn filed_under(&self) -> &str {
        match (&self.write, &self.held) {
            (Write::Move { how: How::Copy, .. }, _) | (_, None) => &self.remote_key,
            (_, Some(held)) => &held.remote_id,
        }
    }

    /// The calendar a move takes the event out of.
    pub fn moved_from(&self) -> Option<CalendarId> {
        match self.write {
            Write::Move { from, .. } => Some(from),
            _ => None,
        }
    }
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
    /// The event with this UID and its changed occurrences, to the calendar
    /// `to`, for the whole series.
    Move { uid: String, to: CalendarId },
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

/// Queues `edit` to be sent once the op `after` is done: an edit saved with
/// a move, in the calendar the move leaves the event in.
pub fn queue_after(conn: &Connection, edit: &Edit, after: CalendarOpId, now: i64, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    queue_op(conn, edit, Link { after: Some(after), following: false }, now, horizon)
}

/// How an op of This and following belongs to its pair.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Link {
    pub(super) after: Option<CalendarOpId>,
    pub(super) following: bool,
}

pub(super) fn queue_op(conn: &Connection, edit: &Edit, link: Link, now: i64, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    if let Change::Move { uid, to } = &edit.change {
        return queue_move(conn, edit.calendar, uid, *to, link, now, horizon).map(Some);
    }
    let calendar = store::calendar(conn, edit.calendar)?;
    if !caps::of(&calendar).writable {
        return Err(Error::Other(format!("{} is read-only", calendar.name)));
    }
    // A new meeting Jolta mails itself is the sending account's: stamped
    // before the store is compared, so the same create again is no change.
    let mut change = edit.change.clone();
    let mails_itself = organizer::sends_itself(&calendar);
    if let (true, Change::Create(events)) = (mails_itself, &mut change)
        && let Some(first) = events.first().cloned()
    {
        let sender = organizer::sender(conn, &calendar, &first)?;
        events.iter_mut().for_each(|e| organizer::stamp(e, sender.as_ref()));
    }
    let Some(Planned { mut write, remote_key, held }) = plan(conn, &calendar, &change, horizon.here)? else { return Ok(None) };
    if mails_itself && let Some(e) = write.event() {
        let sender = organizer::sender(conn, &calendar, e)?;
        organizer::prepare(&mut write, sender.as_ref(), edit.notify);
    }
    check(conn, &calendar, &write, &remote_key, held.as_ref(), edit.extra.as_ref(), Some(edit.notify))?;
    let first = write.event().ok_or_else(|| Error::Other("an edit without an event".into()))?;
    let (uid, recurrence_id) = (first.uid.clone(), if matches!(write, Write::Update { .. } | Write::Cancel { .. }) { first.recurrence_id } else { None });
    let scope = match (link.following, recurrence_id) {
        (true, _) => Scope::Following,
        (false, Some(_)) => Scope::This,
        (false, None) => Scope::All,
    };
    let waits = edit.notify != Notify::Nobody || write.removes_occurrences() || link.following;
    let delay = if waits { UNDO_SEND_SECONDS } else { 0 };
    let series_update = matches!(&write, Write::Update { event, .. } if event.recurrence_id.is_none());
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
    // Its update carries the series' changed occurrences as this op left
    // them, not as later edits still to be sent or undone have them.
    if mails_itself && series_update {
        let changed: Vec<EventData> = series(conn, calendar.id, &uid)?.into_iter().filter(|e| e.recurrence_id.is_some()).collect();
        store::set_calendar_op_changed(conn, id, &changed)?;
    }
    store::refresh_changed_locally(conn, calendar.id, &uid)?;
    Ok(Some(id))
}

/// Why `queue` would refuse the edit, found without changing anything, so
/// the editor can say it before the user saves.
pub fn refusal(conn: &Connection, edit: &Edit) -> Result<Option<String>> {
    if let Change::Move { uid, to } = &edit.change {
        return Ok(plan_move(conn, edit.calendar, uid, *to).err().map(|e| e.to_string()));
    }
    let calendar = store::calendar(conn, edit.calendar)?;
    if !caps::of(&calendar).writable {
        return Ok(Some(format!("{} is read-only", calendar.name)));
    }
    let Some(Planned { write, remote_key, held }) = plan(conn, &calendar, &edit.change, super::tz::here())? else { return Ok(None) };
    Ok(check(conn, &calendar, &write, &remote_key, held.as_ref(), edit.extra.as_ref(), None).err().map(|e| e.to_string()))
}

/// Refuses what the calendar's provider can't take from Jolta: a change to
/// a meeting someone else organizes beyond the user's own settings (check
/// 1), a write `notify` says tells nobody where the server mails the
/// guests anyway, and any change the provider's request can't carry.
/// `notify` is None when only the change is checked, before the user chose.
fn check(conn: &Connection, calendar: &Calendar, write: &Write, key: &str, held: Option<&Held>, extra: Option<&Extra>, notify: Option<Notify>) -> Result<()> {
    let account = store::calendar_address(conn, calendar)?.unwrap_or_default();
    let caps = caps::of(calendar);
    let title = write.event().map_or("the event", |e| e.title.as_str());
    // A calendar on this Mac has no server to make the call, and keeps
    // what the store holds.
    let asks = match write {
        Write::Create(events) => events.iter().find_map(|e| e.video),
        Write::Update { base, event, .. } if event.video != base.video => event.video,
        _ => None,
    };
    if asks.is_some_and(|v| !caps.video.contains(&v)) {
        return Err(Error::Other(format!("{} can't have this video call", calendar.name)));
    }
    let mails = match write {
        Write::Create(events) => events.iter().any(|e| !super::draft::guests(calendar, &account, e).is_empty()),
        Write::Update { base, event, .. } => {
            let mut me = super::draft::mine(calendar, &account, base);
            me.extend(super::draft::mine(calendar, &account, event));
            let c = super::change::classify(base, event, &me);
            if let Some(o) = super::draft::other_organizer(calendar, &account, base).or(super::draft::other_organizer(calendar, &account, event))
                && !c.is_own_only()
            {
                return Err(Error::Other(format!("{} organizes {}, so only your own settings can change", super::change::name(o), base.title)));
            }
            !c.is_own_only() && !(super::draft::guests(calendar, &account, base).is_empty() && super::draft::guests(calendar, &account, event).is_empty())
        }
        Write::Delete(events) => {
            if let Some(o) = events.iter().find_map(|e| super::draft::other_organizer(calendar, &account, e)) {
                return Err(Error::Other(format!("{} organizes {title}; decline it instead", super::change::name(o))));
            }
            events.iter().any(|e| !super::draft::guests(calendar, &account, e).is_empty())
        }
        Write::Cancel { series, occurrence, .. } => {
            if let Some(o) = super::draft::other_organizer(calendar, &account, series).or(super::draft::other_organizer(calendar, &account, occurrence)) {
                return Err(Error::Other(format!("{} organizes {title}; decline it instead", super::change::name(o))));
            }
            !super::draft::guests(calendar, &account, occurrence).is_empty()
        }
        Write::Move { .. } => false,
    };
    if organizer::sends_itself(calendar) {
        let event = write.event().ok_or_else(|| Error::Other("an edit without an event".into()))?;
        return match notify {
            Some(Notify::Outside) => Err(Error::Other("Jolta can't tell which guests use another calendar, so it mails all of them or none".into())),
            Some(Notify::All | Notify::Changed) if mails && organizer::sender(conn, calendar, event)?.is_none() => {
                Err(Error::Other(format!("Choose the account Jolta sends the invitations for {} from first", calendar.name)))
            }
            _ => Ok(()),
        };
    }
    match notify {
        Some(Notify::Nobody) if mails && caps.hold_update == caps::Hold::NeverForMeetingFields => {
            return Err(Error::Other(format!("{} emails the guests of {title} about this change, so Jolta can't save it without sending", caps::mailer(calendar))));
        }
        Some(Notify::Outside) if !caps.outside_only => return Err(Error::Other(format!("{} can't mail only some guests", caps::mailer(calendar)))),
        Some(Notify::Changed) if !caps.new_people_only => return Err(Error::Other(format!("{} can't mail only the people added or removed", caps::mailer(calendar)))),
        _ => {}
    }
    // A repeat rule the provider can't hold, set here, is refused here, as
    // the repeat editor greys it.
    let new_rule = match write {
        Write::Create(events) => events.iter().find(|e| e.recurrence_id.is_none()),
        Write::Update { base, event, .. } if base.recurrence != event.recurrence || base.span != event.span => Some(event),
        _ => None,
    };
    if let Some(e) = new_rule
        && let Some(rule) = &e.recurrence
        && let Some(text) = super::repeat::refusal(rule, &e.span, calendar, super::tz::here())
    {
        return Err(Error::Other(format!("{text}, so Jolta didn't change {}", calendar.name)));
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
        Write::Move { .. } => return Ok(()),
    };
    sources::writable(calendar.source, &request).map_err(|e| Error::Other(format!("{e}, so Jolta didn't change {}", calendar.name)))
}

/// A move against what the store holds.
struct PlannedMove {
    to: Calendar,
    write: Write,
    remote_key: String,
    held: Option<Held>,
    note: Option<String>,
}

/// How the event `uid` of `from` moves to `to` (`moving::plan`), with the
/// key the event lands at there and, on a server, the object it leaves.
/// The whole series moves. A copy is checked as the create it makes, so a
/// field the other calendar can't hold refuses the move before anything
/// changes.
fn plan_move(conn: &Connection, from: CalendarId, uid: &str, to: CalendarId) -> Result<PlannedMove> {
    let (from, to) = (store::calendar(conn, from)?, store::calendar(conn, to)?);
    let mut stmt = conn.prepare_cached(&format!("SELECT {STORED_COLS} FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY recurrence_id <> '', id"))?;
    let rows: Vec<Row> = stmt.query_map(params![from.id, uid], stored_row)?.map(|r| to_stored(r?).map(Row::from)).collect::<Result<_>>()?;
    let Some(first) = rows.first() else { return Err(Error::NotFound(format!("event {uid} in {}", from.name))) };
    let address = store::calendar_address(conn, &from)?.unwrap_or_default();
    let events: Vec<EventData> = rows.iter().map(|r| r.event.clone()).collect();
    let holds = !series(conn, to.id, uid)?.is_empty();
    let moving = moving::plan(&from, &address, &to, &events, holds).map_err(Error::Other)?;
    let held = (from.source != CalendarSource::Local).then(|| Held { remote_id: first.remote_id.clone(), etag: first.etag.clone() });
    let remote_key = match (moving.how, to.source) {
        (How::Native, CalendarSource::CalDav) => sources::caldav::moved_path(&to, &first.remote_id)?,
        (How::Native, _) => first.remote_id.clone(),
        (How::Copy, _) => remote_key(conn, &to, uid)?,
    };
    if moving.how == How::Copy {
        check(conn, &to, &Write::Create(moving.events.clone()), &remote_key, None, None, Some(Notify::Nobody))?;
    }
    let write = Write::Move { from: from.id, how: moving.how, rows, events: moving.events };
    Ok(PlannedMove { to, write, remote_key, held, note: moving.note })
}

/// Moves the event in the store at once and queues the move, filed under
/// the calendar it goes to, where the event is from now on: later edits
/// of it there wait for it, and it waits for the edits still to be sent
/// in the calendar it leaves. It tells nobody.
fn queue_move(conn: &Connection, from: CalendarId, uid: &str, to: CalendarId, link: Link, now: i64, horizon: &Horizon) -> Result<CalendarOpId> {
    let PlannedMove { to, write, remote_key, held, .. } = plan_move(conn, from, uid, to)?;
    apply(conn, to.id, &write, &remote_key, horizon)?;
    let id = store::insert_calendar_op(
        conn,
        &CalendarOp {
            id: CalendarOpId(0),
            calendar: to.id,
            uid: uid.to_string(),
            recurrence_id: None,
            write,
            scope: Scope::All,
            notify: Notify::Nobody,
            message: None,
            extra: None,
            remote_key,
            held,
            passed_over: false,
            send_after: now,
            state: OpState::Pending,
            tried: false,
            attempts: 0,
            error: None,
            lost: Vec::new(),
            created: now,
            after: link.after,
        },
    )?;
    store::refresh_changed_locally(conn, to.id, uid)?;
    Ok(id)
}

/// One calendar of the editor's picker for a stored event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveChoice {
    pub calendar: CalendarId,
    /// Why the event can't move there, shown greyed; None when it can.
    pub refusal: Option<String>,
    /// What the move costs: "Standup's Teams meeting stays behind: Google
    /// calendars can't have one."
    pub note: Option<String>,
}

/// Where the event `uid` of `from` can move: every calendar but its own,
/// each with the reason it can't take it or what the move costs.
pub fn move_choices(conn: &Connection, from: CalendarId, uid: &str) -> Result<Vec<MoveChoice>> {
    let mut out = Vec::new();
    for to in store::calendars(conn)?.into_iter().filter(|c| c.id != from) {
        out.push(match plan_move(conn, from, uid, to.id) {
            Ok(planned) => MoveChoice { calendar: to.id, refusal: None, note: planned.note },
            Err(e) => MoveChoice { calendar: to.id, refusal: Some(e.to_string()), note: None },
        });
    }
    Ok(out)
}

/// What moving the event `uid` of `from` to `to` costs, said before the
/// move; an error when it can't move there.
pub fn move_note(conn: &Connection, from: CalendarId, uid: &str, to: CalendarId) -> Result<Option<String>> {
    Ok(plan_move(conn, from, uid, to)?.note)
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
            // The SEQUENCE Jolta raised for a local meeting is no change of
            // the user's: the same edit again holds the one it started from.
            let same = EventData { sequence: base.event.sequence.max(event.sequence), ..event.clone() };
            if base.event == same {
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
        Change::Move { .. } => Err(Error::Other("a move is planned by plan_move".into())),
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
/// (Microsoft 365 test account, 2026-11-07). A series that counted its
/// occurrences and now ends on a day loses them all there too: Exchange
/// reset a moved occurrence before the new end when a COUNT became an
/// UNTIL (Microsoft 365 test account, 2026-11-07, with guests). A series
/// made single loses them all.
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
    let ends = |r: Option<&super::Recurrence>| r.map(|r| r.rules.iter().map(|rule| (rule.contains("COUNT="), rule.contains("UNTIL="))).collect::<Vec<_>>());
    let exchange_resets = calendar.source == CalendarSource::Graph
        && (base.span != event.span || pattern(base.recurrence.as_ref()) != pattern(event.recurrence.as_ref()) || ends(base.recurrence.as_ref()) != ends(event.recurrence.as_ref()));
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
            // version the series is: in CalDAV they share one resource. An
            // event a later move took out of the calendar keeps that move's
            // rows, and putting an edit of it back changes nothing here.
            let (remote_id, etag, blob) = match stored(conn, calendar, &event.uid, event.recurrence_id.as_ref())? {
                Some(s) => (s.remote_id, s.etag, s.blob),
                None => match stored(conn, calendar, &event.uid, None)? {
                    Some(s) => (s.remote_id, s.etag, s.blob),
                    None if store::moved_away(conn, calendar, &event.uid)? => return Ok(()),
                    None => (remote_key.to_string(), None, None),
                },
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
        // A native move keeps the rows' objects and versions until the
        // server's answer names where they are; a copy files them under the
        // key its create lands at.
        Write::Move { from, how, rows, events } => {
            let uid = events.first().map(|e| e.uid.clone());
            conn.execute("DELETE FROM event WHERE calendar_id = ?1 AND uid = ?2", params![from, uid])?;
            match how {
                How::Native => rows.iter().try_for_each(|r| restore_row(conn, calendar, r))?,
                How::Copy => events.iter().try_for_each(|e| store::insert_event(conn, calendar, remote_key, None, e, None))?,
            }
            if let Some(uid) = &uid {
                store::rebuild_series(conn, *from, uid, horizon)?;
            }
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
    let passed_over = store::calendar_op(conn, op.id)?.passed_over;
    if passed_over {
        store::forget_calendar_cursor(conn, op.calendar)?;
    }
    // A move puts the rows back where they were. One that was tried may
    // have reached the server, and syncs of both calendars passed it over
    // while it waited: both are listed whole on their next sync.
    if let Write::Move { from, rows, .. } = &op.write {
        conn.execute("DELETE FROM event WHERE calendar_id = ?1 AND uid = ?2", params![op.calendar, op.uid])?;
        rows.iter().try_for_each(|r| restore_row(conn, *from, r))?;
        store::rebuild_series(conn, op.calendar, &op.uid, horizon)?;
        store::rebuild_series(conn, *from, &op.uid, horizon)?;
        store::refresh_changed_locally(conn, *from, &op.uid)?;
        if op.tried || passed_over {
            store::forget_calendar_cursor(conn, op.calendar)?;
            store::forget_calendar_cursor(conn, *from)?;
        }
        return Ok(());
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
        // A later delete of the whole series took it out already: there is
        // nothing to put back.
        Write::Cancel { series, .. } => match stored(conn, op.calendar, &series.uid, None)? {
            Some(master) => Some(Write::Update { base: master.event, event: series.clone(), dropped: Vec::new(), fresh: false }),
            None => {
                store::rebuild_series(conn, op.calendar, &op.uid, horizon)?;
                return Ok(());
            }
        },
        Write::Move { .. } => unreachable!("a move is put back above"),
    };
    let key = op.filed_under();
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
    take_back(conn, op, now, horizon)
}

fn take_back(conn: &Connection, op: CalendarOp, now: i64, horizon: &Horizon) -> Result<Undone> {
    let (id, title) = (op.id, op.write.event().map(|e| e.title.clone()).unwrap_or_default());
    // This and following: the old series' op waits for the new series'
    // create, and an edit saved with a move waits for the move. Undo takes
    // back both.
    let first = op.after.map(|a| store::calendar_op(conn, a)).transpose()?;
    match op.state {
        OpState::Pending if !op.tried => {
            revert(conn, &op, horizon)?;
            store::delete_calendar_op(conn, id)?;
            store::refresh_changed_locally(conn, op.calendar, &op.uid)?;
            if let Some(first) = first {
                take_back(conn, first, now, horizon)?;
            }
            Ok(Undone::Dropped)
        }
        // The old series' shortening may have reached the server, after the
        // new series did: putting the old one back would show both.
        OpState::Pending | OpState::Sending | OpState::Done if op.scope == Scope::Following && op.after.is_some() => {
            Err(Error::Other(format!("The change to {title} already reached the server and can't be taken back")))
        }
        // The opposite of the edit goes first, where the event is, then the
        // move back, which waits for it.
        OpState::Pending | OpState::Sending | OpState::Done if op.notify == Notify::Nobody => match op.write.undo() {
            Some(change) => match queue(conn, &Edit::quiet(op.calendar, change), now, horizon)? {
                Some(inverse) => {
                    if let Some(first) = first.filter(|f| f.moved_from().is_some()) {
                        take_back(conn, first, now, horizon)?;
                    }
                    Ok(Undone::Queued(inverse))
                }
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
        Write::Move { .. } => return Err(Error::Other("A move has nothing to apply".into())),
    };
    queue(conn, &Edit { calendar: op.calendar, change, notify: op.notify, message: op.message.clone(), extra: op.extra.clone() }, now, horizon)
}

/// What came of one attempt to send an op.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    pub op: CalendarOpId,
    pub calendar: CalendarId,
    /// The calendar a move left, which changed too.
    pub moved_from: Option<CalendarId>,
    /// The account whose outbox got the op's invitations, to send them now.
    pub mailed: Option<crate::AccountId>,
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
    /// A move found the event changed in the calendar it was leaving, and
    /// left it there: the object as it is now, in that calendar.
    Stayed { version: Option<Landed> },
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
            // The server kept the occurrence a delete took out: the series
            // comes back as it was, then the occurrence as the server has it.
            if let Write::Cancel { occurrence, .. } = &op.write {
                revert(conn, op, horizon)?;
                // A delete of the whole series behind this one put aside
                // the series with this occurrence taken out; its Undo brings
                // back the series whole, with the occurrence the server kept.
                let kept: Vec<EventData> = version.iter().flat_map(|(v, _)| &v.events).filter(|e| e.uid == op.uid && e.recurrence_id.is_some()).cloned().collect();
                for mut later in store::later_calendar_ops(conn, op)? {
                    let Write::Delete(events) = &mut later.write else { continue };
                    // Only this occurrence's EXDATE goes: later edits and
                    // other occurrences taken out stay put aside.
                    if let (Some(rec), Some(rid)) = (events.iter_mut().find(|e| e.recurrence_id.is_none()).and_then(|m| m.recurrence.as_mut()), occurrence.recurrence_id) {
                        rec.exdates.retain(|x| !x.names(&rid, horizon.here));
                    }
                    for k in &kept {
                        events.retain(|e| !same_row(e, k));
                        events.push(k.clone());
                    }
                    store::rewrite_calendar_op(conn, &later)?;
                }
            }
            // A split whose old series kept its rule elsewhere: the new
            // series goes again, and nothing is left to apply.
            let compensated = op.scope == Scope::Following && op.after.is_some() && lost.contains(&Field::Repeat);
            store::settle_calendar_op(conn, op.id, if compensated { OpState::Failed } else { OpState::Conflict })?;
            store::set_calendar_op_lost(conn, op.id, &lost)?;
            land(conn, op, version.as_ref(), false, horizon)?;
            if compensated {
                take_back_new_series(conn, op, now, horizon)?;
            }
            let event = stored(conn, op.calendar, &op.uid, op.recurrence_id.as_ref())?.map(|s| s.id);
            let message = match op.write {
                Write::Delete(_) | Write::Cancel { .. } => format!("{title} changed in {} after you deleted it, so it wasn't deleted.", place(&calendar)),
                _ => format!("{title} changed in {} while you edited it. {}", place(&calendar), unsaved(&lost)),
            };
            failure = Some(Failure { message, event });
        }
        // The event stays where the server still has it, at the version the
        // server has; the edits saved with the move go with it.
        Ok(Sent::Stayed { version }) => {
            let from = store::calendar(conn, op.moved_from().unwrap_or(op.calendar))?;
            let reason = format!("{title} changed in {} while you moved it, so it stayed in {}.", place(&from), from.name);
            store::fail_calendar_attempt(conn, op.id, &reason, None)?;
            store::end_later_calendar_ops(conn, op, OpState::Failed, &reason)?;
            end_waiting(conn, op, &reason, horizon)?;
            revert(conn, op, horizon)?;
            if let Some((v, blob)) = &version {
                store::apply_item(conn, from.id, &v.remote_id, v.etag.as_deref(), &v.events, blob.as_deref(), horizon)?;
            }
            let event = stored(conn, from.id, &op.uid, None)?.map(|s| s.id);
            failure = Some(Failure { message: reason, event });
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
            store::remove_item(conn, op.calendar, op.filed_under(), store::Removal::Deleted, horizon)?;
            let message = match op.moved_from() {
                Some(from) => format!("{title} was deleted in {} while you moved it.", place(&store::calendar(conn, from)?)),
                None => format!("{title} was deleted in {} while you edited it. Your change wasn't saved.", place(&calendar)),
            };
            failure = Some(Failure { message, event: None });
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
                Write::Create(_) | Write::Move { .. } => {
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
            // The old series' shortening may have reached the server before
            // its answer was lost: the calendar is listed again, which shows
            // what the server has. Deleting the new series here could lose
            // the events after the split.
            if op.after.is_some() {
                store::forget_calendar_cursor(conn, op.calendar)?;
            }
            let message = match &op.write {
                // A copy may have reached the other calendar before its
                // delete failed: the next syncs show what each holds.
                Write::Move { how: How::Copy, .. } => format!("Jolta couldn't move {title}: {reason}. If it shows in both calendars, delete one."),
                Write::Move { .. } => format!("Jolta couldn't move {title}: {reason}"),
                _ => format!("Jolta couldn't save the change to {title}: {reason}"),
            };
            let event = stored(conn, op.moved_from().unwrap_or(op.calendar), &op.uid, op.recurrence_id.as_ref())?.map(|s| s.id);
            failure = Some(Failure { message, event });
        }
    }
    store::refresh_changed_locally(conn, op.calendar, &op.uid)?;
    if let Some(from) = op.moved_from() {
        store::refresh_changed_locally(conn, from, &op.uid)?;
    }
    Ok(Settled { op: op.id, calendar: op.calendar, moved_from: op.moved_from(), mailed: None, failure })
}

/// This and following whose old series the server kept whole: the new
/// series, made already, is deleted again, guarded by its version as any
/// delete, so the old series alone shows the events. Guests invited to it
/// hear of its cancellation as they heard of it.
fn take_back_new_series(conn: &Connection, op: &CalendarOp, now: i64, horizon: &Horizon) -> Result<()> {
    let Some(first) = op.after else { return Ok(()) };
    let first = store::calendar_op(conn, first)?;
    if first.state == OpState::Done {
        let delete = Edit { notify: first.notify, ..Edit::quiet(first.calendar, Change::Delete { uid: first.uid.clone() }) };
        queue(conn, &delete, now, horizon)?;
    }
    Ok(())
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
    let from = op.filed_under();
    // A version that doesn't name the occurrence the op wrote would stand
    // in for its series; the calendar is listed again instead.
    if let Some(rid) = &op.recurrence_id
        && v.remote_id != from
        && !v.events.iter().any(|e| e.recurrence_id.is_some_and(|r| r.names(rid, horizon.here)))
    {
        return store::forget_calendar_cursor(conn, op.calendar);
    }
    // An occurrence the server holds as an instance with an id of its own,
    // written by the op or kept against its delete: only its row moves
    // there, and only ops on that occurrence follow it.
    if op.recurrence_id.is_some() && v.remote_id != from {
        store::refile_occurrence(conn, op.calendar, &op.uid, op.recurrence_id.as_ref(), from, &v.remote_id)?;
        store::apply_item(conn, op.calendar, &v.remote_id, v.etag.as_deref(), &v.events, blob.as_deref(), horizon)?;
        for mut later in store::later_calendar_ops(conn, op)? {
            if later.recurrence_id != op.recurrence_id || later.held.as_ref().is_none_or(|h| h.remote_id != from) {
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
    // A series' changed occurrences written as instances go under their
    // own ids before the series' object replaces the rows under its id.
    for i in &v.instances {
        for e in &i.events {
            store::refile_occurrence(conn, op.calendar, &op.uid, e.recurrence_id.as_ref(), &v.remote_id, &i.remote_id)?;
        }
    }
    store::apply_item(conn, op.calendar, &v.remote_id, v.etag.as_deref(), &v.events, blob.as_deref(), horizon)?;
    for i in &v.instances {
        store::apply_item(conn, op.calendar, &i.remote_id, i.etag.as_deref(), &i.events, None, horizon)?;
    }
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
            if let Write::Update { base, event, .. } = &mut later.write
                && let Some(made) = v.events.iter().find(|e| e.recurrence_id == base.recurrence_id)
            {
                carry_call(base, event, made);
            }
        }
        store::rewrite_calendar_op(conn, &later)?;
    }
    Ok(())
}

/// An edit queued while an earlier op still asked for a video call starts
/// from the meeting the server made, so its write keeps the call and the
/// block the server wrote into the description. Its base and its event
/// each keep their own text before the block, so the edit still changes
/// what it changed.
fn carry_call(base: &mut EventData, event: &mut EventData, made: &EventData) {
    if base.video.is_none() || made.meeting.is_none() {
        return;
    }
    let block = description::split(&made.description, made.description_html);
    for e in [&mut *base, &mut *event] {
        let own = description::split(&e.description, e.description_html).editable;
        Field::MeetingLink.copy(e, made);
        (e.description, e.description_html) = block.join(&own);
    }
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
            let (result, mailed) = match result {
                Ok((sent, mailed)) => (Ok(sent), mailed),
                Err(e) => (Err(e), None),
            };
            let settled = jolta.store.write_async(move |tx| settle(tx, &op, result, now, &horizon)).await?;
            out.push(Settled { mailed, ..settled });
        }
    }
}

/// Reaches the op's server and keeps the text of the object it left there.
/// A local calendar's store is its server, which `queue` already wrote; its
/// guests hear of the op through the outbox of the account it sends from,
/// which comes back with the result.
async fn send(jolta: &Jolta, op: &CalendarOp) -> Result<(Sent, Option<crate::AccountId>)> {
    let id = op.calendar;
    let calendar = jolta.store.read_async(move |c| store::calendar(c, id)).await?;
    if let Some(from) = op.moved_from() {
        let from = jolta.store.read_async(move |c| store::calendar(c, from)).await?;
        // A calendar on this Mac is its own server: its side of a move is
        // in the store already.
        let source = async |c: &Calendar| match c.source {
            CalendarSource::Local => Ok(None),
            _ => super::sync::calendar_source(jolta, c).await.map(Some),
        };
        let (from_source, to_source) = (source(&from).await?, source(&calendar).await?);
        let from_remote = from_source.as_ref().map(|source| Bound { source, calendar: &from });
        let to_remote = to_source.as_ref().map(|source| Bound { source, calendar: &calendar });
        let delivered = deliver_move(from_remote.as_ref(), to_remote.as_ref(), &calendar, op).await?;
        return Ok((kept(jolta, delivered)?, None));
    }
    if organizer::sends_itself(&calendar) {
        let mailed = mail_guests(jolta, op, &calendar).await?;
        return Ok((Sent::Done { version: None, merged: false }, mailed));
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
    Ok((kept(jolta, delivered)?, None))
}

/// What the server made of an op, with the text of each object it left
/// kept in the blob store.
fn kept(jolta: &Jolta, delivered: Delivered) -> Result<Sent> {
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
        Delivered::Stayed { version } => Sent::Stayed { version: keep(version)? },
    })
}

/// Queues the mails that tell the guests of `op` what it changed, from the
/// account the meeting is sent from, and links them to the op. A mail
/// already in the outbox, from an earlier try of the same op, isn't queued
/// again. The op waited the undo window already, so they go at once.
async fn mail_guests(jolta: &Jolta, op: &CalendarOp, calendar: &Calendar) -> Result<Option<crate::AccountId>> {
    let Some(event) = op.write.event().cloned() else { return Ok(None) };
    if op.notify == Notify::Nobody {
        return Ok(None);
    }
    let (cal, id) = (calendar.clone(), op.id);
    let (sender, changed) = jolta.store.read_async(move |c| Ok((organizer::sender(c, &cal, &event)?, store::calendar_op_changed(c, id)?))).await?;
    let Some(sender) = sender else { return Err(Error::Other(format!("no account sends the invitations for {}", calendar.name))) };
    let mails = organizer::op_mails(op, &sender, &changed)?;
    if mails.is_empty() {
        return Ok(None);
    }
    let mut queued = Vec::new();
    for m in mails {
        queued.push((jolta.blobs.put(&m.raw)?, m));
    }
    let (id, account) = (op.id, sender.id);
    jolta
        .store
        .write_async(move |tx| {
            let mut ids = Vec::new();
            for (blob, m) in &queued {
                let row = match store::outbox_mail(tx, account, &m.message_id)? {
                    Some(store::ReplyMail::Waiting(row) | store::ReplyMail::Sent(row)) => row,
                    None => store::queue_outbox(tx, account, None, blob, &m.message_id, &m.from, &m.recipients, None, 0)?,
                };
                ids.push(row);
            }
            store::set_calendar_op_outbox(tx, id, &ids)
        })
        .await?;
    Ok(Some(account))
}

/// What the server made of an op, before its text is kept.
#[derive(Clone, Debug, PartialEq)]
enum Delivered {
    /// Written; `merged` when a change made elsewhere was merged in first.
    Done { version: Option<Version>, merged: bool },
    Conflict { version: Option<Version>, lost: Vec<Field> },
    Gone,
    Stayed { version: Option<Version> },
}

/// The server one op writes to: a calendar's source, or a test's.
pub(crate) trait Remote {
    async fn write(&self, write: &OpWrite<'_>, tell: Tell<'_>) -> Result<Written>;
    async fn fetch(&self, remote_id: &str, uid: &str, alarms: &[Alarm]) -> Result<Option<Version>>;
    /// The occurrence `rid` of the series held as `series`, changed or not.
    async fn fetch_occurrence(&self, series: &rsvp::Stored, rid: &Moment, alarms: &[Alarm]) -> Result<Option<Version>>;
}

struct Bound<'a> {
    source: &'a Source,
    calendar: &'a Calendar,
}

impl Remote for Bound<'_> {
    async fn write(&self, write: &OpWrite<'_>, tell: Tell<'_>) -> Result<Written> {
        self.source.write(self.calendar, write, tell).await
    }

    async fn fetch(&self, remote_id: &str, uid: &str, alarms: &[Alarm]) -> Result<Option<Version>> {
        self.source.fetch(self.calendar, remote_id, uid, alarms).await
    }

    async fn fetch_occurrence(&self, series: &rsvp::Stored, rid: &Moment, alarms: &[Alarm]) -> Result<Option<Version>> {
        self.source.fetch_occurrence(self.calendar, series, rid, alarms).await
    }
}

/// Sends the op's write, guarded by the version it holds. A 412 brings the
/// server's version, which the user's edit is merged into field by field
/// before the next write; a delete goes ahead only while the event is the
/// version the user deleted. `raw` is the held object's text, for CalDAV.
async fn deliver(remote: &impl Remote, op: &CalendarOp, raw: Option<Vec<u8>>) -> Result<Delivered> {
    let extra = op.extra.as_ref();
    let tell = Tell { notify: op.notify, message: op.message.as_deref() };
    let held_as = |held: &Held, data: &EventData, raw: Option<Vec<u8>>| rsvp::Stored {
        id: EventId(0),
        remote_id: held.remote_id.clone(),
        etag: held.etag.clone(),
        raw,
        data: data.clone(),
    };
    match &op.write {
        Write::Create(events) => match remote.write(&OpWrite::Create { events, key: &op.remote_key, extra }, tell).await? {
            Written::Saved(version) => Ok(Delivered::Done { version, merged: false }),
            Written::Stale | Written::Gone => Err(Error::Other("the server refused the new event".into())),
        },
        Write::Move { .. } => Err(Error::Other("a move goes through deliver_move".into())),
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
                let written = match remote.write(&request, tell).await? {
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
                        // An occurrence without an object of its own is read
                        // as its instance, which may have changed meanwhile.
                        let now = match (fresh, &mine.recurrence_id) {
                            (true, Some(rid)) => remote.fetch_occurrence(&held, rid, &base.alarms).await?,
                            _ => remote.fetch(&held.remote_id, &mine.uid, &base.alarms).await?,
                        };
                        let Some(now) = now else { return Ok(Delivered::Gone) };
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
            let mut changed = row.is_some();
            for _ in 0..MERGE_ROUNDS {
                match remote.write(&OpWrite::Cancel { held: &held, occurrence, changed }, tell).await? {
                    Written::Saved(version) => return Ok(Delivered::Done { version, merged: false }),
                    Written::Gone => return Ok(Delivered::Done { version: None, merged: false }),
                    Written::Stale => {
                        // An occurrence without an object of its own is read
                        // as its instance: a change made to it meanwhile
                        // keeps it.
                        let now = match (changed, &occurrence.recurrence_id) {
                            (false, Some(rid)) => remote.fetch_occurrence(&held, rid, &occurrence.alarms).await?,
                            _ => remote.fetch(&held.remote_id, &occurrence.uid, &occurrence.alarms).await?,
                        };
                        let Some(now) = now else { return Ok(Delivered::Done { version: None, merged: false }) };
                        changed = changed || now.remote_id != held.remote_id;
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
                match remote.write(&OpWrite::Delete { held: &held }, tell).await? {
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

/// Sends a move. `from` and `to` are the servers of the calendar the event
/// leaves and of the op's calendar `to_calendar`, None for a calendar on
/// this Mac, whose store is its server. A native move is one request to
/// `from`, held to the version the move started from. A copy creates the
/// event in `to` at the op's key, so a retry finds the event its first try
/// made, and only then deletes it from `from`, held to that version. Either
/// way, a change made to the event meanwhile leaves it where it was, and
/// the copy made is deleted again, as is one of an event deleted meanwhile.
async fn deliver_move<R: Remote>(from: Option<&R>, to: Option<&R>, to_calendar: &Calendar, op: &CalendarOp) -> Result<Delivered> {
    let Write::Move { how, rows, events, .. } = &op.write else { return Err(Error::Other("not a move".into())) };
    let Some(first) = rows.iter().map(|r| &r.event).find(|e| e.recurrence_id.is_none()).or(rows.first().map(|r| &r.event)) else {
        return Err(Error::Other("a move without its event".into()));
    };
    let held_as = |held: &Held| rsvp::Stored { id: EventId(0), remote_id: held.remote_id.clone(), etag: held.etag.clone(), raw: None, data: first.clone() };
    // The event as the server has it, when a change made meanwhile leaves
    // it where it is: None while only its version changed.
    let changed = |now: &Version| -> Option<Option<Version>> {
        match now.events.iter().find(|e| same_row(e, first)) {
            None => Some(None),
            Some(theirs) if FIELDS.into_iter().any(|f| f.differs(first, theirs)) => Some(Some(now.clone())),
            Some(_) => None,
        }
    };
    match how {
        How::Native => {
            let (Some(remote), Some(held)) = (from, &op.held) else { return Ok(Delivered::Done { version: None, merged: false }) };
            let mut held = held_as(held);
            for _ in 0..MERGE_ROUNDS {
                match remote.write(&OpWrite::Move { held: &held, to: to_calendar }, Tell::NOBODY).await? {
                    Written::Saved(version) => return Ok(Delivered::Done { version, merged: false }),
                    Written::Gone => return Ok(Delivered::Gone),
                    Written::Stale => {
                        let Some(now) = remote.fetch(&held.remote_id, &first.uid, &first.alarms).await? else { return Ok(Delivered::Gone) };
                        match changed(&now) {
                            Some(None) => return Ok(Delivered::Gone),
                            Some(theirs) => return Ok(Delivered::Stayed { version: theirs }),
                            None => held = held_as(&Held { remote_id: now.remote_id.clone(), etag: now.etag.clone() }),
                        }
                    }
                }
            }
            Err(Error::Other("it kept changing on the server".into()))
        }
        How::Copy => {
            let made = match to {
                Some(t) => match t.write(&OpWrite::Create { events, key: &op.remote_key, extra: None }, Tell::NOBODY).await? {
                    Written::Saved(version) => version,
                    Written::Stale | Written::Gone => return Err(Error::Other(format!("{} refused the event", to_calendar.name))),
                },
                None => None,
            };
            let (Some(f), Some(held)) = (from, &op.held) else { return Ok(Delivered::Done { version: made, merged: false }) };
            let mut held = held_as(held);
            for _ in 0..MERGE_ROUNDS {
                match f.write(&OpWrite::Delete { held: &held }, Tell::NOBODY).await? {
                    Written::Saved(_) => return Ok(Delivered::Done { version: made, merged: false }),
                    Written::Gone => {
                        take_back_copy(to, made.as_ref()).await?;
                        return Ok(Delivered::Gone);
                    }
                    Written::Stale => {
                        let now = f.fetch(&held.remote_id, &first.uid, &first.alarms).await?;
                        let left = match &now {
                            None => Some(None),
                            Some(now) => changed(now),
                        };
                        match left {
                            None => {
                                let now = now.expect("a version that only changed its ETag");
                                held = held_as(&Held { remote_id: now.remote_id, etag: now.etag });
                            }
                            Some(theirs) => {
                                take_back_copy(to, made.as_ref()).await?;
                                return Ok(match theirs {
                                    Some(theirs) => Delivered::Stayed { version: Some(theirs) },
                                    None => Delivered::Gone,
                                });
                            }
                        }
                    }
                }
            }
            Err(Error::Other("it kept changing on the server".into()))
        }
    }
}

/// Deletes the copy a move made, which nobody heard of, at the version the
/// server has now: Exchange gives an event a new change key right after it
/// makes it.
async fn take_back_copy<R: Remote>(to: Option<&R>, made: Option<&Version>) -> Result<()> {
    let (Some(to), Some(made)) = (to, made) else { return Ok(()) };
    let Some(first) = made.events.first() else { return Ok(()) };
    let mut now = Some(made.clone());
    for _ in 0..MERGE_ROUNDS {
        let Some(v) = now else { return Ok(()) };
        let held = rsvp::Stored { id: EventId(0), remote_id: v.remote_id.clone(), etag: v.etag.clone(), raw: None, data: first.clone() };
        match to.write(&OpWrite::Delete { held: &held }, Tell::NOBODY).await? {
            Written::Saved(_) | Written::Gone => return Ok(()),
            Written::Stale => now = to.fetch(&v.remote_id, &first.uid, &first.alarms).await?,
        }
    }
    Err(Error::Other("the copy kept changing on the server".into()))
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
        video: None,
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

    /// A calendar on this Mac has no server to make a video call, so the
    /// queue refuses one before the store changes, on a create or an edit.
    #[test]
    fn a_calendar_without_video_calls_refuses_one() {
        let (s, cal) = setup("no-video");
        let call = EventData { video: Some(crate::calendar::MeetingProvider::Teams), ..event("u8", "Review", "2026-11-24 14:00") };
        let refused = s.write(|tx| queue(tx, &Edit::quiet(cal, Change::Create(vec![call.clone()])), NOW, &horizon()));
        assert!(refused.unwrap_err().to_string().contains("can't have this video call"));
        q(&s, cal, Change::Create(vec![event("u8", "Review", "2026-11-24 14:00")]));
        assert!(s.write(|tx| queue(tx, &Edit::quiet(cal, Change::Update(call)), NOW, &horizon())).is_err());
        assert_eq!(shown(&s), pairs(&[("24 14:00", "Review")]));
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
            store::add_calendar(tx, source, &RemoteCalendar { remote_id: format!("{source}-{access}"), name: format!("{source}"), color: None, zone: None, access, primary: true, refresh: None, video: None })
        })
        .unwrap()
        .id
    }

    /// Check 1: a write only goes to a calendar the user can write. A
    /// meeting's change that the server mails its guests about anyway is
    /// never queued as telling nobody, and one that mails waits the undo
    /// window.
    #[test]
    fn only_writable_calendars_take_ops_and_meetings_wait_to_send() {
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
        let graph = remote_calendar(&s, CalendarSource::Graph, Access::Owner);
        let err = s.write(|tx| queue(tx, &Edit::quiet(graph, Change::Create(vec![meeting.clone()])), NOW, &horizon())).unwrap_err();
        assert!(err.to_string().contains("can't save it without sending"), "Exchange invites whatever Jolta asks: {err}");
        let err = s.write(|tx| queue(tx, &Edit { notify: Notify::Outside, ..Edit::quiet(graph, Change::Create(vec![meeting.clone()])) }, NOW, &horizon())).unwrap_err();
        assert!(err.to_string().contains("only some guests"), "{err}");
        let sent = s.write(|tx| queue(tx, &Edit { notify: Notify::All, ..Edit::quiet(graph, Change::Create(vec![meeting.clone()])) }, NOW, &horizon())).unwrap().unwrap();
        assert_eq!(s.read(|c| store::calendar_op(c, sent)).unwrap().send_after, NOW + UNDO_SEND_SECONDS, "an invitation waits the undo window");
        let quiet = s.write(|tx| queue(tx, &Edit::quiet(google, Change::Create(vec![EventData { uid: "u8g".into(), ..meeting }])), NOW, &horizon())).unwrap().unwrap();
        assert_eq!(s.read(|c| store::calendar_op(c, quiet)).unwrap().send_after, NOW, "Google saves a meeting without sending, at once");
        let oof = EventData { categories: vec!["Away".into()], ..event("u7", "Away", "2026-11-24 09:00") };
        let err = s.write(|tx| queue(tx, &Edit::quiet(google, Change::Create(vec![oof])), NOW, &horizon())).unwrap_err();
        assert!(err.to_string().contains("can't hold categories"), "refused at once, not after five tries: {err}");
        assert_eq!(shown(&s).len(), 2, "only the two meetings queued were written");
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
        /// The notify of each write the server took: what mailed people.
        told: std::sync::Mutex<Vec<Notify>>,
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
            self.objects.lock().unwrap().get(id).map(|(v, e)| Version { remote_id: id.into(), etag: Some(v.to_string()), events: vec![e.clone()], raw: None, instances: Vec::new() })
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

    const TEAMS_BLOCK: &str = "Microsoft Teams meeting\nJoin: https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0\nMeeting ID: 123 456 789\n________________";

    /// The event as Exchange makes it when asked for a Teams meeting: the
    /// meeting, and its block after the description.
    fn made_call(mut e: EventData) -> EventData {
        if e.video.take().is_some() {
            e.description = if e.description.is_empty() { TEAMS_BLOCK.into() } else { format!("{}\n\n{TEAMS_BLOCK}", e.description) };
            e.meeting = super::super::meeting::detect(&[], None, "", &e.description);
        }
        e
    }

    impl Remote for Server {
        async fn write(&self, write: &OpWrite<'_>, tell: Tell<'_>) -> Result<Written> {
            let took = || self.told.lock().unwrap().push(tell.notify);
            match *write {
                OpWrite::Create { events, key, .. } => {
                    self.log.lock().unwrap().push(format!("create {key}"));
                    let id = if self.names_itself { format!("server-{key}") } else { key.to_string() };
                    match self.get(&id) {
                        Some(e) if e.uid == events[0].uid => {}
                        Some(_) => return Err(Error::Other("taken".into())),
                        None => self.put(&id, made_call(events[0].clone())),
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
                    took();
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
                OpWrite::Move { held, .. } => {
                    self.log.lock().unwrap().push(format!("move {} at {}", held.remote_id, held.etag.as_deref().unwrap_or("-")));
                    let Some(now) = self.version(&held.remote_id) else { return Ok(Written::Gone) };
                    if now.etag != held.etag {
                        return Ok(Written::Stale);
                    }
                    self.edit(&held.remote_id, |_| {});
                    self.answer(self.version(&held.remote_id))
                }
            }
        }

        async fn fetch(&self, remote_id: &str, _: &str, _: &[Alarm]) -> Result<Option<Version>> {
            self.log.lock().unwrap().push(format!("fetch {remote_id}"));
            Ok(self.version(remote_id))
        }

        async fn fetch_occurrence(&self, series: &rsvp::Stored, _: &Moment, _: &[Alarm]) -> Result<Option<Version>> {
            let id = format!("{}_instance", series.remote_id);
            self.log.lock().unwrap().push(format!("fetch {id}"));
            Ok(self.version(&id).or_else(|| self.version(&series.remote_id)))
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
                    Delivered::Stayed { version } => Sent::Stayed { version: version.map(|v| (v, None)) },
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

    /// Check 4: an occurrence changed elsewhere while Jolta's change of it
    /// waited is read as that instance, so its change stays.
    #[test]
    fn an_occurrence_changed_elsewhere_keeps_that_change() {
        let (s, cal, server, series) = synced_series("instance-stale");
        let rid = Moment::Time(at("2026-12-01 09:00"));
        let mine = EventData { recurrence_id: Some(rid), recurrence: None, ..event("u1", "Standup", "2026-12-01 11:00") };
        let op = q(&s, cal, Change::Update(mine)).unwrap();
        let theirs = EventData { recurrence_id: Some(rid), recurrence: None, ..event("u1", "Standup, theirs", "2026-12-01 09:00") };
        server.put("g1_instance", theirs);
        server.put("g1", series);
        run(&s, &server);
        assert_eq!(state(&s, op).0, OpState::Done);
        let now = server.get("g1_instance").unwrap();
        assert_eq!((now.title.as_str(), now.span), ("Standup, theirs", event("u1", "x", "2026-12-01 11:00").span), "their title and my time: {:?}", server.log());
    }

    /// A series created with a changed occurrence keeps it once settled:
    /// the instance the writer made lands under its own id.
    #[test]
    fn a_created_series_keeps_its_changed_occurrence_when_it_lands() {
        let (s, _) = setup("create-instances");
        let cal = remote_calendar(&s, CalendarSource::Google, Access::Owner);
        let series = EventData { recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=3".into()], ..Default::default() }), ..event("u9", "Review", "2026-11-10 09:00") };
        let moved = EventData { recurrence_id: Some(Moment::Time(at("2026-11-17 09:00"))), recurrence: None, ..event("u9", "Review", "2026-11-17 15:00") };
        let op = q(&s, cal, Change::Create(vec![series.clone(), moved.clone()])).unwrap();
        let shown_before = shown(&s);
        let claimed = claim(&s);
        let instance = Version { remote_id: "g9_20261117T080000Z".into(), etag: Some("2".into()), events: vec![moved], raw: None, instances: Vec::new() };
        let version = Version { remote_id: "g9".into(), etag: Some("2".into()), events: vec![series], raw: None, instances: vec![instance] };
        s.write(|tx| settle(tx, &claimed[0], Ok(Sent::Done { version: Some((version, None)), merged: false }), NOW, &horizon())).unwrap();
        assert_eq!(state(&s, op).0, OpState::Done);
        assert_eq!(shown(&s), shown_before, "the moved occurrence stays at 15:00");
        assert_eq!(s.read(|c| series_rows(c, cal)).unwrap(), 2);
    }

    /// An occurrence changed elsewhere after the user deleted it stays, as
    /// the server has it, and its series with it.
    #[test]
    fn deleting_an_occurrence_changed_elsewhere_keeps_it_and_its_series() {
        let (s, cal, server, series) = synced_series("cancel-stale");
        let rid = Moment::Time(at("2026-12-01 09:00"));
        let op = q(&s, cal, Change::DeleteOccurrence { uid: "u1".into(), recurrence_id: rid }).unwrap();
        let theirs = EventData { recurrence_id: Some(rid), recurrence: None, ..event("u1", "Standup, theirs", "2026-12-01 09:00") };
        server.put("g1_instance", theirs);
        server.put("g1", series.clone());
        let settled = s.write(|tx| store::claim_calendar_ops(tx, NOW + 3600, AT_ONCE)).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let sent = rt.block_on(deliver(&server, &settled[0], None)).map_err(|e| e.to_string()).map(|d| match d {
            Delivered::Done { version, merged } => Sent::Done { version: version.map(|v| (v, None)), merged },
            Delivered::Conflict { version, lost } => Sent::Conflict { version: version.map(|v| (v, None)), lost },
            Delivered::Gone => Sent::Gone,
            Delivered::Stayed { version } => Sent::Stayed { version: version.map(|v| (v, None)) },
        });
        assert!(matches!(sent, Ok(Sent::Conflict { .. })), "{sent:?}");
        s.write(|tx| settle(tx, &settled[0], sent, NOW, &horizon())).unwrap();
        assert_eq!(state(&s, op).0, OpState::Conflict);
        let master = s.read(|c| stored(c, cal, "u1", None)).unwrap().unwrap().event;
        assert!(master.recurrence.as_ref().unwrap().exdates.is_empty(), "the series is whole again");
        assert_eq!(s.read(|c| stored(c, cal, "u1", Some(&rid))).unwrap().unwrap().event.title, "Standup, theirs");
        assert_eq!(s.read(|c| conflict(c, cal, "u1")).unwrap().unwrap().action, "Delete anyway");
        assert!(s.write(|tx| apply_mine(tx, op, NOW, &horizon())).unwrap().is_some(), "and Delete anyway queues the delete again");
    }

    /// A delete of the whole series queued behind a delete of one of its
    /// occurrences: the first's conflict has no series to put back and
    /// settles, so the second goes.
    #[test]
    fn a_series_delete_behind_a_conflicting_occurrence_delete_still_goes() {
        let (s, cal, server, series) = synced_series("cancel-then-delete");
        let rid = Moment::Time(at("2026-12-01 09:00"));
        let one = q(&s, cal, Change::DeleteOccurrence { uid: "u1".into(), recurrence_id: rid }).unwrap();
        let other = Moment::Time(at("2026-12-08 09:00"));
        q(&s, cal, Change::DeleteOccurrence { uid: "u1".into(), recurrence_id: other }).unwrap();
        let all = q(&s, cal, Change::Delete { uid: "u1".into() }).unwrap();
        server.put("g1_instance", EventData { recurrence_id: Some(rid), recurrence: None, ..event("u1", "Standup, theirs", "2026-12-01 09:00") });
        server.put("g1", series);
        let claimed = s.write(|tx| store::claim_calendar_ops(tx, NOW + 3600, AT_ONCE)).unwrap();
        assert_eq!(claimed.iter().map(|o| o.id).collect::<Vec<_>>(), [one]);
        let version = server.version("g1_instance").map(|v| (v, None));
        s.write(|tx| settle(tx, &claimed[0], Ok(Sent::Conflict { version, lost: vec![Field::Title] }), NOW, &horizon())).unwrap();
        assert_eq!(state(&s, one).0, OpState::Conflict);
        assert_eq!(claim(&s).iter().map(|o| o.recurrence_id).collect::<Vec<_>>(), [Some(other)], "the next op of the series goes");
        let Write::Delete(kept) = s.read(|c| store::calendar_op(c, all)).unwrap().write else { panic!("a delete") };
        let master = kept.iter().find(|e| e.recurrence_id.is_none()).unwrap();
        assert_eq!(master.recurrence.as_ref().unwrap().exdates, [other], "its Undo brings back the rejected occurrence and keeps the other taken out");
        assert!(kept.iter().any(|e| e.recurrence_id == Some(rid) && e.title == "Standup, theirs"), "with the occurrence the server kept");
    }

    fn series_rows(c: &Connection, cal: CalendarId) -> Result<i64> {
        Ok(c.query_row("SELECT count(*) FROM event WHERE calendar_id = ?1 AND uid IN ('u1', 'u9')", [cal], |r| r.get(0))?)
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

    /// An edit of the description saved while the create that asked for a
    /// Teams meeting waits keeps the block Exchange writes (Sol, U8 S2).
    #[test]
    fn an_edit_behind_a_create_with_a_call_keeps_its_block() {
        let (s, _) = setup("call-behind");
        let cal = remote_calendar(&s, CalendarSource::Graph, Access::Owner);
        let server = Server { names_itself: true, ..Server::default() };
        let asked = EventData { video: Some(crate::calendar::MeetingProvider::Teams), ..event("u1", "Review", "2026-11-24 09:00") };
        q(&s, cal, Change::Create(vec![asked.clone()])).unwrap();
        let edit = q(&s, cal, Change::Update(EventData { description: "Agenda".into(), ..asked.clone() })).unwrap();
        let cleared = q(&s, cal, Change::Update(asked)).unwrap();
        for _ in 0..3 {
            run(&s, &server);
        }
        assert_eq!((state(&s, edit).0, state(&s, cleared).0), (OpState::Done, OpState::Done));
        let id = server.objects.lock().unwrap().keys().next().unwrap().clone();
        assert_eq!(server.log().len(), 3, "the create and both edits reach the server");
        let now = server.get(&id).unwrap();
        assert_eq!(now.description, TEAMS_BLOCK, "the text cleared after it is gone, the block stays as the server wrote it (Sol, U8 S3)");
        assert!(now.meeting.is_some_and(|m| m.join_url.contains("meetup-join")));
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
    fn guest(address: &str) -> crate::calendar::Attendee {
        crate::calendar::Attendee {
            person: crate::calendar::Person { address: address.into(), name: String::new() },
            role: crate::calendar::Role::Required,
            response: crate::calendar::Response::Accepted,
            comment: String::new(),
            is_me: false,
        }
    }

    /// Check 4 for a meeting: Undo inside the undo window sends nothing, the
    /// same save twice queues one update, and a retry after the server took
    /// the update but the answer was lost writes nothing more, so the guests
    /// get one mail.
    #[test]
    fn an_update_to_guests_goes_out_once() {
        let (s, _) = setup("tell-once");
        let cal = remote_calendar(&s, CalendarSource::Google, Access::Owner);
        let server = Server { lose_answers: std::sync::Mutex::new(1), ..Server::default() };
        server.put("g1", EventData { attendees: vec![guest("ann@example.com")], ..event("u1", "Standup", "2026-11-24 09:00") });
        sync(&s, cal, &server);
        let mut moved = server.get("g1").unwrap();
        moved.title = "Daily".into();
        let update = Edit { notify: Notify::All, ..Edit::quiet(cal, Change::Update(moved.clone())) };

        let first = s.write(|tx| queue(tx, &update, NOW, &horizon())).unwrap().unwrap();
        assert_eq!(s.write(|tx| undo(tx, first, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!((claim(&s).len(), server.log().len()), (0, 0), "nothing went out");
        assert_eq!(shown(&s), pairs(&[("24 09:00", "Standup")]), "back as it was");

        let op = s.write(|tx| queue(tx, &update, NOW, &horizon())).unwrap().unwrap();
        assert_eq!(s.write(|tx| queue(tx, &update, NOW, &horizon())).unwrap(), None, "the same save again queues nothing");
        assert!(run(&s, &server)[0].failure.is_none(), "a lost answer is tried again");
        run(&s, &server);
        assert_eq!(server.log(), ["update g1 at 1", "update g1 at 1", "fetch g1"], "the retry finds its change made and writes nothing");
        assert_eq!(*server.told.lock().unwrap(), [Notify::All], "one update reached the guests");
        assert_eq!(state(&s, op).0, OpState::Done);
        assert!(s.write(|tx| undo(tx, op, NOW, &horizon())).is_err(), "an update that went out can't be taken back");
    }

    /// Check 1: a guest's copy takes the user's own settings and nothing
    /// that would change the meeting, and is never deleted through the
    /// queue: the user declines instead.
    #[test]
    fn a_guests_copy_takes_only_the_users_own_settings() {
        let (s, _) = setup("guest-copy");
        let cal = remote_calendar(&s, CalendarSource::Graph, Access::Owner);
        let server = Server::default();
        let theirs = EventData {
            organizer: Some(crate::calendar::Person { address: "jan@example.com".into(), name: "Jan Peeters".into() }),
            attendees: vec![guest("jan@example.com"), crate::calendar::Attendee { is_me: true, ..guest("me@example.com") }],
            ..event("u1", "Planning", "2026-11-24 09:00")
        };
        server.put("m1", theirs.clone());
        sync(&s, cal, &server);
        let renamed = EventData { title: "Mine now".into(), ..theirs.clone() };
        let err = s.write(|tx| queue(tx, &Edit { notify: Notify::All, ..Edit::quiet(cal, Change::Update(renamed)) }, NOW, &horizon())).unwrap_err();
        assert!(err.to_string().contains("Jan Peeters organizes Planning"), "{err}");
        let err = s.write(|tx| queue(tx, &Edit::quiet(cal, Change::Delete { uid: "u1".into() }), NOW, &horizon())).unwrap_err();
        assert!(err.to_string().contains("decline it instead"), "{err}");
        let free = EventData { show_as: crate::calendar::ShowAs::Free, ..theirs };
        assert!(s.write(|tx| queue(tx, &Edit::quiet(cal, Change::Update(free)), NOW, &horizon())).unwrap().is_some(), "show as is the user's own");
    }


    /// Sends every due op as the worker does, each calendar to its own
    /// server: a move's create to the server of its calendar, its delete or
    /// native move to the one it leaves.
    fn run_on(s: &Store, servers: &[(CalendarId, &Server)]) -> Vec<Settled> {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let of = |c: CalendarId| servers.iter().find(|(id, _)| *id == c).map(|(_, s)| *s);
        claim(s)
            .iter()
            .map(|op| {
                let to = s.read(|c| store::calendar(c, op.calendar)).unwrap();
                let delivered = match op.moved_from() {
                    Some(from) => rt.block_on(deliver_move(of(from), of(op.calendar), &to, op)),
                    None => rt.block_on(deliver(of(op.calendar).unwrap(), op, None)),
                };
                let sent = delivered.map_err(|e| e.to_string()).map(|d| match d {
                    Delivered::Done { version, merged } => Sent::Done { version: version.map(|v| (v, None)), merged },
                    Delivered::Conflict { version, lost } => Sent::Conflict { version: version.map(|v| (v, None)), lost },
                    Delivered::Gone => Sent::Gone,
                    Delivered::Stayed { version } => Sent::Stayed { version: version.map(|v| (v, None)) },
                });
                s.write(|tx| settle(tx, op, sent, NOW, &horizon())).unwrap()
            })
            .collect()
    }

    /// (calendar, title, remote id) of every stored event.
    fn rows_where(s: &Store) -> Vec<(i64, String, String)> {
        s.read(|c| Ok(c.prepare("SELECT calendar_id, title, remote_id FROM event ORDER BY calendar_id, id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?))
            .unwrap()
    }

    fn at_(list: &[(CalendarId, &str, &str)]) -> Vec<(i64, String, String)> {
        list.iter().map(|(c, t, r)| (c.0, t.to_string(), r.to_string())).collect()
    }

    fn moving(s: &Store, from: CalendarId, uid: &str, to: CalendarId) -> CalendarOpId {
        q(s, from, Change::Move { uid: uid.into(), to }).unwrap()
    }

    /// Two calendars of one account on `source`, the first holding
    /// "Standup" as `g1` at version 1, synced.
    fn two(name: &str, source: CalendarSource) -> (Store, CalendarId, CalendarId, Server) {
        let (s, _) = setup(name);
        let account: i64 = s
            .write(|tx| {
                let kind = if source == CalendarSource::Graph { "graph" } else { "gmail" };
                tx.execute("INSERT INTO account (kind, address, username, secret_ref, created) VALUES (?1, 'me@example.com', 'me@example.com', 'r', 0)", [kind])?;
                Ok(tx.last_insert_rowid())
            })
            .unwrap();
        let add = |remote: &str| {
            s.write(|tx| {
                let c = store::add_calendar(tx, source, &RemoteCalendar { remote_id: remote.into(), name: remote.into(), color: None, zone: None, access: Access::Owner, primary: false, refresh: None, video: None })?;
                tx.execute("UPDATE calendar SET account_id = ?2 WHERE id = ?1", params![c.id, account])?;
                Ok(c.id)
            })
            .unwrap()
        };
        let (a, b) = (add("Work"), add("Home"));
        let server = Server { names_itself: source == CalendarSource::Graph, ..Server::default() };
        server.put("g1", event("u1", "Standup", "2026-11-24 09:00"));
        sync(&s, a, &server);
        (s, a, b, server)
    }

    #[test]
    fn a_move_on_this_mac_shows_at_once_and_undo_puts_it_back() {
        let (s, a) = setup("move-local");
        let b = s.write(|tx| new_local_calendar(tx, "Home", None)).unwrap().id;
        q(&s, a, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")]));
        send_all(&s, Ok(()));
        let moved = moving(&s, a, "u1", b);
        assert_eq!(rows_where(&s), at_(&[(b, "Standup", "u1")]), "the event shows in its new calendar before anything is sent");
        assert_eq!(not_synced(&s, b, "u1"), [true]);
        assert_eq!(s.write(|tx| undo(tx, moved, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(rows_where(&s), at_(&[(a, "Standup", "u1")]));

        let moved = moving(&s, a, "u1", b);
        send_all(&s, Ok(()));
        assert_eq!((state(&s, moved), not_synced(&s, b, "u1")), ((OpState::Done, true), vec![false]));
        assert!(matches!(s.write(|tx| undo(tx, moved, NOW, &horizon())).unwrap(), Undone::Queued(_)), "a move that went is moved back");
        assert_eq!(rows_where(&s), at_(&[(a, "Standup", "u1")]));
        assert!(s.write(|tx| queue(tx, &Edit::quiet(a, Change::Move { uid: "u1".into(), to: a }), NOW, &horizon())).is_err(), "a move to its own calendar");
    }

    /// Check 4 for Microsoft 365's copy: the delete waits for the create,
    /// never runs when the create failed, and a retry after the create
    /// reached the server makes no second event.
    #[test]
    fn a_copy_deletes_the_event_only_after_its_one_create() {
        let (s, a, b, from) = two("move-copy", CalendarSource::Graph);
        let to = Server { names_itself: true, ..Server::default() };
        *to.lose_answers.lock().unwrap() = 1;
        let moved = moving(&s, a, "u1", b);
        let key = s.read(|c| store::calendar_op(c, moved)).unwrap().remote_key;
        assert_eq!(rows_where(&s), at_(&[(b, "Standup", &key)]), "filed under the create's key until it lands");

        let settled = run_on(&s, &[(a, &from), (b, &to)]);
        assert!(settled[0].failure.is_none(), "an attempt that timed out is tried again");
        assert_eq!((from.log(), to.log()), (Vec::<String>::new(), vec![format!("create {key}")]), "no delete after a create without an answer");
        assert!(from.get("g1").is_some());

        s.write(|tx| Ok(tx.execute("UPDATE calendar_op SET send_after = 0", [])?)).unwrap();
        let settled = run_on(&s, &[(a, &from), (b, &to)]);
        assert_eq!((settled[0].failure.clone(), settled[0].moved_from), (None, Some(a)));
        assert_eq!(to.log(), [format!("create {key}")]);
        assert_eq!(to.objects.lock().unwrap().len(), 1, "the retry found the event its first try made");
        assert_eq!(from.log(), ["delete g1 at 1"]);
        assert!(from.get("g1").is_none());
        assert_eq!(rows_where(&s), at_(&[(b, "Standup", &format!("server-{key}"))]), "the event lands where the server put it");

        let (s, a, b, from) = two("move-copy-fails", CalendarSource::Graph);
        let to = Server { names_itself: true, ..Server::default() };
        *to.lose_answers.lock().unwrap() = MAX_ATTEMPTS;
        let moved = moving(&s, a, "u1", b);
        for _ in 0..MAX_ATTEMPTS {
            s.write(|tx| Ok(tx.execute("UPDATE calendar_op SET send_after = 0", [])?)).unwrap();
            run_on(&s, &[(a, &from), (b, &to)]);
        }
        assert_eq!(state(&s, moved).0, OpState::Failed);
        assert!(from.log().is_empty() && from.get("g1").is_some(), "the event never left");
        assert_eq!(rows_where(&s), at_(&[(a, "Standup", "g1")]), "it shows where it was");
    }

    /// A move that finds the event changed in the calendar it leaves
    /// leaves it there, at the server's version, and takes its copy back.
    #[test]
    fn a_copy_of_an_event_changed_meanwhile_stays_and_takes_its_copy_back() {
        let (s, a, b, from) = two("move-changed", CalendarSource::Graph);
        let to = Server { names_itself: true, ..Server::default() };
        let moved = moving(&s, a, "u1", b);
        from.edit("g1", |e| e.title = "Daily".into());
        let settled = run_on(&s, &[(a, &from), (b, &to)]);
        assert_eq!(settled[0].failure.as_ref().unwrap().message, "Standup changed in Outlook while you moved it, so it stayed in Work.");
        assert!(to.objects.lock().unwrap().is_empty(), "the copy is deleted again");
        assert_eq!(from.log(), ["delete g1 at 1", "fetch g1"]);
        assert_eq!(rows_where(&s), at_(&[(a, "Daily", "g1")]));
        assert_eq!(state(&s, moved).0, OpState::Failed);

        let (s, a, b, from) = two("move-gone", CalendarSource::Graph);
        let to = Server { names_itself: true, ..Server::default() };
        moving(&s, a, "u1", b);
        from.objects.lock().unwrap().clear();
        let settled = run_on(&s, &[(a, &from), (b, &to)]);
        assert_eq!(settled[0].failure.as_ref().unwrap().message, "Standup was deleted in Outlook while you moved it.");
        assert!(to.objects.lock().unwrap().is_empty() && rows_where(&s).is_empty(), "an event deleted meanwhile is never made again");
    }

    /// A native move keeps the server's object, lands its new version in the
    /// new calendar, and an edit made after it writes over that version.
    #[test]
    fn a_native_move_lands_in_its_calendar_and_the_next_edit_follows_it() {
        let (s, a, b, server) = two("move-native", CalendarSource::Google);
        moving(&s, a, "u1", b);
        let edit = q(&s, b, Change::Update(event("u1", "Daily", "2026-11-24 09:00"))).unwrap();
        run_on(&s, &[(a, &server), (b, &server)]);
        run_on(&s, &[(a, &server), (b, &server)]);
        assert_eq!(server.log(), ["move g1 at 1", "update g1 at 2"], "the edit waited for the move and holds its version");
        assert_eq!(state(&s, edit).0, OpState::Done);
        assert_eq!(rows_where(&s), at_(&[(b, "Daily", "g1")]));

        let (s, a, b, server) = two("move-native-changed", CalendarSource::Google);
        moving(&s, a, "u1", b);
        server.edit("g1", |e| e.location = "Room 9".into());
        let settled = run_on(&s, &[(a, &server), (b, &server)]);
        assert!(settled[0].failure.is_some());
        assert_eq!(server.log(), ["move g1 at 1", "fetch g1"]);
        assert_eq!(shown_where(&s), triples(&[("Standup", "Room 9", "2")]), "it stays where it was, as the server has it");
        assert_eq!(rows_where(&s)[0].0, a.0);
    }

    /// While a move waits, a sync of the calendar it leaves doesn't bring the
    /// event back there, and one of the calendar it goes to keeps it.
    #[test]
    fn syncs_while_a_move_waits_keep_the_event_where_it_goes() {
        let (s, a, b, server) = two("move-sync", CalendarSource::Google);
        moving(&s, a, "u1", b);
        sync(&s, a, &server);
        sync(&s, b, &server);
        s.write(|tx| store::remove_item(tx, b, "g1", store::Removal::Unlisted, &horizon()).map(drop)).unwrap();
        assert_eq!(rows_where(&s), at_(&[(b, "Standup", "g1")]));
    }

    /// An edit saved with a move waits for it, and Undo takes back both:
    /// dropped before they were sent, put back by their opposites after.
    #[test]
    fn undo_of_an_edit_saved_with_a_move_takes_back_both() {
        let (s, a) = setup("move-edit");
        let b = s.write(|tx| new_local_calendar(tx, "Home", None)).unwrap().id;
        q(&s, a, Change::Create(vec![event("u1", "Standup", "2026-11-24 09:00")]));
        send_all(&s, Ok(()));
        let save = |s: &Store| {
            let moved = moving(s, a, "u1", b);
            let edit = Edit::quiet(b, Change::Update(event("u1", "Daily", "2026-11-24 10:00")));
            s.write(|tx| queue_after(tx, &edit, moved, NOW, &horizon())).unwrap().unwrap()
        };
        let edit = save(&s);
        assert_eq!(rows_where(&s), at_(&[(b, "Daily", "u1")]));
        assert_eq!(s.write(|tx| undo(tx, edit, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(rows_where(&s), at_(&[(a, "Standup", "u1")]));
        assert!(s.read(|c| store::calendar_ops(c, 10)).unwrap().iter().all(|o| o.write.kind() != "move"), "the move went with it");

        let edit = save(&s);
        send_all(&s, Ok(()));
        send_all(&s, Ok(()));
        assert!(matches!(s.write(|tx| undo(tx, edit, NOW, &horizon())).unwrap(), Undone::Queued(_)));
        assert_eq!(rows_where(&s), at_(&[(a, "Standup", "u1")]), "the old title, back where it was");
        send_all(&s, Ok(()));
        send_all(&s, Ok(()));
        assert!(s.read(|c| store::calendar_ops(c, 10)).unwrap().iter().all(|o| o.state == OpState::Done));
    }

    /// A move waits for the edits still to be sent where the event was, and
    /// one of them failing for good puts nothing back there.
    #[test]
    fn a_move_waits_for_the_edits_before_it() {
        let (s, a, b, server) = two("move-after-edit", CalendarSource::Google);
        let edit = q(&s, a, Change::Update(event("u1", "Daily", "2026-11-24 09:00"))).unwrap();
        let moved = moving(&s, a, "u1", b);
        assert_eq!(claim(&s).iter().map(|o| o.id).collect::<Vec<_>>(), [edit], "the move waits");
        s.write(|tx| Ok(tx.execute("UPDATE calendar_op SET attempts = ?2 WHERE id = ?1", params![edit.0, MAX_ATTEMPTS - 1])?)).unwrap();
        let op = s.read(|c| store::calendar_op(c, edit)).unwrap();
        s.write(|tx| settle(tx, &op, Err("offline".into()), NOW, &horizon())).unwrap();
        assert_eq!(rows_where(&s), at_(&[(b, "Daily", "g1")]), "nothing comes back where the event was");
        let settled = run_on(&s, &[(a, &server), (b, &server)]);
        assert!(settled[0].failure.is_none(), "the server's version is the one the move held");
        assert_eq!(state(&s, moved).0, OpState::Done);
        assert_eq!(rows_where(&s), at_(&[(b, "Standup", "g1")]), "it lands as the server has it, without the edit that failed");
    }
}
