//! The calendar op queue's table. The rules live in `calendar::ops`.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::calendar::{moment_from_text, moment_text};
use crate::calendar::change::Field;
use crate::calendar::ops::{CalendarOp, CalendarOpId, Extra, Held, OpState, Row as KeptRow, Write};
use crate::calendar::{CalendarId, EventData};
use crate::{Error, Result};

/// What an op stores in its `target` column: the events as the user saved
/// them, none for a delete, with the provider fields Jolta doesn't model
/// and the server object an update or delete writes over.
#[derive(serde::Serialize, serde::Deserialize)]
struct Target {
    events: Vec<EventData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extra: Option<Extra>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    held: Option<Held>,
    /// A sync passed over a server version of the op's rows (`apply_item`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    passed_over: bool,
    /// An update's changed occurrences that leave with it, or a cancelled
    /// occurrence's own row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rows: Vec<KeptRow>,
    /// An update of an occurrence that had no row of its own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    fresh: bool,
    /// The op this one waits for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<i64>,
}

const COLS: &str = "id, calendar_id, uid, recurrence_id, kind, scope, notify, message, base, target, remote_key,
     send_after, state, tried, attempts, error, lost, created";

fn json_error(e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

fn op_row(r: &Row) -> rusqlite::Result<CalendarOp> {
    let kind: String = r.get(4)?;
    let base: Option<Vec<EventData>> = r.get::<_, Option<String>>(8)?.map(|t| serde_json::from_str(&t)).transpose().map_err(json_error)?;
    let target: Target = serde_json::from_str(&r.get::<_, String>(9)?).map_err(json_error)?;
    let broken = || rusqlite::Error::InvalidColumnType(4, format!("{kind} op without its events"), rusqlite::types::Type::Text);
    let write = match kind.as_str() {
        "create" => Write::Create(target.events),
        "update" => {
            let (Some(base), Some(event)) = (base.and_then(|b| b.into_iter().next()), target.events.into_iter().next()) else {
                return Err(broken());
            };
            Write::Update { base, event, dropped: target.rows, fresh: target.fresh }
        }
        "delete" => Write::Delete(base.ok_or_else(broken)?),
        "cancel" => {
            let Some([series, occurrence]) = base.and_then(|b| <[EventData; 2]>::try_from(b).ok()) else { return Err(broken()) };
            Write::Cancel { series, occurrence, row: target.rows.into_iter().next() }
        }
        _ => return Err(broken()),
    };
    let lost = r.get::<_, Option<String>>(16)?.map(|t| serde_json::from_str(&t)).transpose().map_err(json_error)?.unwrap_or_default();
    Ok(CalendarOp {
        id: CalendarOpId(r.get(0)?),
        calendar: r.get(1)?,
        uid: r.get(2)?,
        recurrence_id: moment_from_text(&r.get::<_, String>(3)?),
        write,
        scope: r.get(5)?,
        notify: r.get(6)?,
        message: r.get(7)?,
        extra: target.extra,
        held: target.held,
        passed_over: target.passed_over,
        remote_key: r.get(10)?,
        send_after: r.get(11)?,
        state: r.get(12)?,
        tried: r.get(13)?,
        attempts: r.get(14)?,
        error: r.get(15)?,
        lost,
        created: r.get(17)?,
        after: target.after.map(CalendarOpId),
    })
}

/// Stores a new op, waiting as `pending` until `op.send_after`. The id,
/// state and counters of `op` are ignored.
pub fn insert_calendar_op(conn: &Connection, op: &CalendarOp) -> Result<CalendarOpId> {
    let (base, target) = written(op)?;
    conn.execute(
        "INSERT INTO calendar_op (calendar_id, uid, recurrence_id, kind, scope, notify, message, base, target, remote_key, send_after, created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            op.calendar,
            op.uid,
            op.recurrence_id.as_ref().map(moment_text).unwrap_or_default(),
            op.write.kind(),
            op.scope,
            op.notify,
            op.message,
            base,
            target,
            op.remote_key,
            op.send_after,
            op.created,
        ],
    )?;
    Ok(CalendarOpId(conn.last_insert_rowid()))
}

/// An op's `base` and `target` columns.
fn written(op: &CalendarOp) -> Result<(Option<String>, String)> {
    let (base, events, rows, fresh) = match &op.write {
        Write::Create(events) => (None, events.clone(), Vec::new(), false),
        Write::Update { base, event, dropped, fresh } => (Some(vec![base.clone()]), vec![event.clone()], dropped.clone(), *fresh),
        Write::Delete(events) => (Some(events.clone()), Vec::new(), Vec::new(), false),
        Write::Cancel { series, occurrence, row } => (Some(vec![series.clone(), occurrence.clone()]), Vec::new(), row.iter().cloned().collect(), false),
    };
    let target = Target { events, extra: op.extra.clone(), held: op.held.clone(), passed_over: op.passed_over, rows, fresh, after: op.after.map(|a| a.0) };
    Ok((base.as_deref().map(to_json).transpose()?, to_json(&target)?))
}

fn to_json<T: serde::Serialize + ?Sized>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Other(format!("calendar op: {e}")))
}

pub fn calendar_op(conn: &Connection, id: CalendarOpId) -> Result<CalendarOp> {
    conn.query_row(&format!("SELECT {COLS} FROM calendar_op WHERE id = ?1"), [id.0], op_row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("calendar op {}", id.0)))
}

/// The newest ops first, for `jolta cal ops`.
pub fn calendar_ops(conn: &Connection, limit: usize) -> Result<Vec<CalendarOp>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM calendar_op ORDER BY id DESC LIMIT ?1"))?;
    let rows = stmt.query_map([limit as i64], op_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// An op `o` that nothing holds back: no older op of its event still to
/// settle, and the op it waits for, if any, done.
const READY: &str = "NOT EXISTS (SELECT 1 FROM calendar_op p WHERE p.calendar_id = o.calendar_id AND p.uid = o.uid
                                 AND p.id < o.id AND p.state IN ('pending', 'sending'))
     AND NOT EXISTS (SELECT 1 FROM calendar_op a WHERE a.id = json_extract(o.target, '$.after') AND a.state <> 'done')";

/// Claims up to `limit` ops that are due, oldest first, for sending. An op
/// waits while an older op for the same event hasn't settled, so one
/// event's ops reach the server one at a time and in order. A claimed op
/// counts as tried from here on, also after it goes back to `pending`.
pub fn claim_calendar_ops(conn: &Connection, now: i64, limit: usize) -> Result<Vec<CalendarOp>> {
    let mut stmt = conn.prepare(&format!(
        "UPDATE calendar_op SET state = 'sending', tried = 1
         WHERE id IN (
           SELECT o.id FROM calendar_op o
           WHERE o.state = 'pending' AND o.send_after <= ?1 AND {READY}
           ORDER BY o.id LIMIT ?2)
         RETURNING {COLS}"
    ))?;
    let rows = stmt.query_map(params![now, limit as i64], op_row)?;
    let mut ops: Vec<CalendarOp> = rows.collect::<rusqlite::Result<_>>()?;
    ops.sort_by_key(|o| o.id.0);
    Ok(ops)
}

/// Puts ops a stopped worker left claimed back in the queue. They stay
/// tried: they may have reached the server.
pub fn release_calendar_claims(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE calendar_op SET state = 'pending' WHERE state = 'sending'", [])?;
    Ok(())
}

/// Whether a stopped worker left ops claimed.
pub fn has_calendar_claims(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS (SELECT 1 FROM calendar_op WHERE state = 'sending')", [], |r| r.get(0))?)
}

/// When the next op the worker could claim is due, if one waits. An op
/// waiting behind an older op of its event counts from that op.
pub fn next_calendar_op_due(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn.query_row(
        &format!("SELECT min(o.send_after) FROM calendar_op o WHERE o.state = 'pending' AND {READY}"),
        [],
        |r| r.get(0),
    )?)
}

/// The oldest newer op that waits or is on its way and rewrites the row
/// `op` wrote: an edit of the same event or occurrence, or a create or
/// delete of the whole series.
pub fn next_calendar_op_over(conn: &Connection, op: &CalendarOp) -> Result<Option<CalendarOp>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLS} FROM calendar_op
                 WHERE calendar_id = ?1 AND uid = ?2 AND id > ?3 AND state IN ('pending', 'sending')
                   AND (recurrence_id = ?4 OR (recurrence_id = '' AND kind IN ('create', 'delete')))
                 ORDER BY id LIMIT 1"
            ),
            params![op.calendar, op.uid, op.id.0, op.recurrence_id.as_ref().map(moment_text).unwrap_or_default()],
            op_row,
        )
        .optional()?)
}

/// Ends every newer op of the same event that waits in `state`, for an
/// event whose create failed for good or that the server deleted: none of
/// them can reach it.
pub fn end_later_calendar_ops(conn: &Connection, op: &CalendarOp, state: OpState, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE calendar_op SET state = ?4, error = ?5
         WHERE calendar_id = ?1 AND uid = ?2 AND id > ?3 AND state = 'pending'",
        params![op.calendar, op.uid, op.id.0, state, error],
    )?;
    Ok(())
}

/// The ops that wait for `id` and haven't been sent.
pub fn calendar_ops_after(conn: &Connection, id: CalendarOpId) -> Result<Vec<CalendarOp>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM calendar_op WHERE json_extract(target, '$.after') = ?1 AND state = 'pending' ORDER BY id"))?;
    let rows = stmt.query_map([id.0], op_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The newer ops of the same event that wait, oldest first.
pub fn later_calendar_ops(conn: &Connection, op: &CalendarOp) -> Result<Vec<CalendarOp>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM calendar_op WHERE calendar_id = ?1 AND uid = ?2 AND id > ?3 AND state = 'pending' ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![op.calendar, op.uid, op.id.0], op_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// How many creates of this UID the calendar has queued, whatever came of
/// them.
pub fn calendar_creates(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM calendar_op WHERE calendar_id = ?1 AND uid = ?2 AND kind = 'create'", params![calendar, uid], |r| r.get(0))?)
}

pub fn set_calendar_op_lost(conn: &Connection, id: CalendarOpId, lost: &[Field]) -> Result<()> {
    conn.execute("UPDATE calendar_op SET lost = ?2 WHERE id = ?1", params![id.0, to_json(lost)?])?;
    Ok(())
}

/// Rewrites an op's `base` and `target` columns from the op.
pub fn rewrite_calendar_op(conn: &Connection, op: &CalendarOp) -> Result<()> {
    let (base, target) = written(op)?;
    conn.execute("UPDATE calendar_op SET base = ?2, target = ?3 WHERE id = ?1", params![op.id.0, base, target])?;
    Ok(())
}

/// Settles an op: the server answered, so an earlier attempt's error no
/// longer stands.
pub fn settle_calendar_op(conn: &Connection, id: CalendarOpId, state: OpState) -> Result<()> {
    conn.execute("UPDATE calendar_op SET state = ?2, error = NULL WHERE id = ?1", params![id.0, state])?;
    Ok(())
}

/// Counts a failed attempt. With `retry_at` the op waits until then;
/// without, it has failed for good.
pub fn fail_calendar_attempt(conn: &Connection, id: CalendarOpId, error: &str, retry_at: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE calendar_op SET attempts = attempts + 1, error = ?2,
           state = CASE WHEN ?3 IS NULL THEN 'failed' ELSE 'pending' END, send_after = coalesce(?3, send_after)
         WHERE id = ?1",
        params![id.0, error, retry_at],
    )?;
    Ok(())
}

/// Keeps a waiting op from being sent before `until`, for `jolta cal … --hold`.
pub fn hold_calendar_op(conn: &Connection, id: CalendarOpId, until: i64) -> Result<()> {
    conn.execute("UPDATE calendar_op SET send_after = max(send_after, ?2) WHERE id = ?1 AND state = 'pending'", params![id.0, until])?;
    Ok(())
}

pub fn delete_calendar_op(conn: &Connection, id: CalendarOpId) -> Result<()> {
    conn.execute("DELETE FROM calendar_op WHERE id = ?1", [id.0])?;
    Ok(())
}

/// The newest op for an event, whatever its state.
pub fn newest_calendar_op(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<Option<CalendarOpId>> {
    Ok(conn
        .query_row("SELECT max(id) FROM calendar_op WHERE calendar_id = ?1 AND uid = ?2", params![calendar, uid], |r| r.get::<_, Option<i64>>(0))?
        .map(CalendarOpId))
}

/// Marks an event's rows "not synced yet" exactly while one of its ops
/// waits or is on its way.
pub fn refresh_changed_locally(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<()> {
    conn.prepare_cached(
        "UPDATE event SET changed_locally = EXISTS (
           SELECT 1 FROM calendar_op WHERE calendar_id = ?1 AND uid = ?2 AND state IN ('pending', 'sending'))
         WHERE calendar_id = ?1 AND uid = ?2",
    )?
    .execute(params![calendar, uid])?;
    Ok(())
}

/// Files the rows of `uid` kept under `from` under the server's id `to`,
/// for a create whose object the server named itself (Graph).
pub fn refile_event_rows(conn: &Connection, calendar: CalendarId, uid: &str, from: &str, to: &str) -> Result<()> {
    if from != to {
        conn.execute("UPDATE event SET remote_id = ?4 WHERE calendar_id = ?1 AND uid = ?2 AND remote_id = ?3", params![calendar, uid, from, to])?;
    }
    Ok(())
}

/// Files the row of one occurrence kept under `from` under `to`, the id the
/// server gave it as an instance of its series.
pub fn refile_occurrence(conn: &Connection, calendar: CalendarId, uid: &str, recurrence_id: Option<&crate::calendar::Moment>, from: &str, to: &str) -> Result<()> {
    conn.execute(
        "UPDATE OR REPLACE event SET remote_id = ?5 WHERE calendar_id = ?1 AND uid = ?2 AND recurrence_id = ?3 AND remote_id = ?4",
        params![calendar, uid, recurrence_id.map(moment_text).unwrap_or_default(), from, to],
    )?;
    Ok(())
}

/// Gives every row of the object `remote_id` the version an op's write
/// left on the server. Rows a later op holds kept that op's edit, which
/// starts from the version just written, so its write is guarded by it.
pub fn set_item_version(conn: &Connection, calendar: CalendarId, remote_id: &str, etag: Option<&str>, blob: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE event SET etag = ?3, blob = coalesce(?4, blob) WHERE calendar_id = ?1 AND remote_id = ?2",
        params![calendar, remote_id, etag, blob],
    )?;
    Ok(())
}
