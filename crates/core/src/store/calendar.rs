//! Calendars, their events and the expanded occurrences the views read.
//!
//! Sources deliver remote objects: a CalDAV resource, one UID of a feed, a
//! Google or Graph event. `apply_item` replaces the event rows of one object
//! and rebuilds the occurrences of every series it touched. A series is all
//! events with one UID in one calendar, whichever objects they came from.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{NaiveDate, NaiveTime};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::calendar::recur::Source;
use crate::calendar::series::{self, Horizon};
use crate::calendar::{
    Access, Bounds, Calendar, CalendarId, CalendarSource, EventData, EventId, EventStatus, MeetingProvider, Moment,
    Response, ShowAs, Zone, tz,
};
use crate::{AccountId, Error, Result};

/// A calendar as its source lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteCalendar {
    pub remote_id: String,
    pub name: String,
    pub color: Option<String>,
    pub zone: Option<String>,
    pub access: Access,
    /// The account's main calendar, shown from the start. Others start hidden.
    pub primary: bool,
    pub refresh: Option<i64>,
}

const CALENDAR_COLS: &str = "id, account_id, source, remote_id, name, coalesce(user_color, color), zone, access, shown,
     counts_as_busy, notify, default_reminder, is_default, cursor, refresh, synced";

fn calendar_row(r: &Row) -> rusqlite::Result<Calendar> {
    Ok(Calendar {
        id: r.get(0)?,
        account_id: r.get(1)?,
        source: r.get(2)?,
        remote_id: r.get(3)?,
        name: r.get(4)?,
        color: r.get(5)?,
        zone: r.get(6)?,
        access: r.get(7)?,
        shown: r.get(8)?,
        counts_as_busy: r.get(9)?,
        notify: r.get(10)?,
        default_reminder: r.get(11)?,
        is_default: r.get(12)?,
        cursor: r.get(13)?,
        refresh: r.get(14)?,
        synced: r.get(15)?,
    })
}

pub fn calendars(conn: &Connection) -> Result<Vec<Calendar>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {CALENDAR_COLS} FROM calendar ORDER BY ifnull(account_id, 1 << 62), source, name"))?;
    let rows = stmt.query_map([], calendar_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn account_calendars(conn: &Connection, account: AccountId) -> Result<Vec<Calendar>> {
    let mut stmt = conn.prepare(&format!("SELECT {CALENDAR_COLS} FROM calendar WHERE account_id = ?1 ORDER BY name"))?;
    let rows = stmt.query_map([account], calendar_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn calendar(conn: &Connection, id: CalendarId) -> Result<Calendar> {
    conn.query_row(&format!("SELECT {CALENDAR_COLS} FROM calendar WHERE id = ?1"), [id], calendar_row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("calendar {id}")))
}

/// Inserts or refreshes one calendar. A known calendar keeps what the user
/// chose: shown, color, counts as busy, notifications.
fn upsert_calendar(conn: &Connection, account: Option<AccountId>, source: CalendarSource, c: &RemoteCalendar) -> Result<CalendarId> {
    let busy_by_default = source != CalendarSource::Webcal;
    Ok(conn.query_row(
        "INSERT INTO calendar (account_id, source, remote_id, name, color, zone, access, shown, counts_as_busy, refresh)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (ifnull(account_id, 0), source, remote_id) DO UPDATE SET
           name = excluded.name, color = excluded.color, zone = excluded.zone, access = excluded.access,
           refresh = coalesce(excluded.refresh, refresh)
         RETURNING id",
        params![account, source, c.remote_id, c.name, c.color, c.zone, c.access, c.primary, busy_by_default, c.refresh],
        |r| r.get(0),
    )?)
}

/// Brings an account's calendars from one source in line with the server's
/// list. Calendars that disappeared are deleted with their events.
pub fn sync_calendar_list(
    conn: &Connection,
    account: AccountId,
    source: CalendarSource,
    list: &[RemoteCalendar],
) -> Result<Vec<Calendar>> {
    let keep: BTreeSet<CalendarId> =
        list.iter().map(|c| upsert_calendar(conn, Some(account), source, c)).collect::<Result<_>>()?;
    for old in account_calendars(conn, account)? {
        if old.source == source && !keep.contains(&old.id) {
            delete_calendar(conn, old.id)?;
        }
    }
    account_calendars(conn, account)
}

/// Adds a calendar that belongs to no account, such as a feed subscription.
/// Adding the same one again changes nothing the user chose.
pub fn add_calendar(conn: &Connection, source: CalendarSource, c: &RemoteCalendar) -> Result<Calendar> {
    let id = upsert_calendar(conn, None, source, c)?;
    calendar(conn, id)
}

pub fn delete_calendar(conn: &Connection, id: CalendarId) -> Result<()> {
    conn.execute("DELETE FROM calendar WHERE id = ?1", [id])?;
    Ok(())
}

pub fn set_calendar_shown(conn: &Connection, id: CalendarId, shown: bool) -> Result<()> {
    conn.execute("UPDATE calendar SET shown = ?2 WHERE id = ?1", params![id, shown])?;
    Ok(())
}

/// The working hours the user set in Settings › Calendar, if any.
pub fn working_hours(conn: &Connection) -> Result<Option<crate::calendar::availability::WorkingHours>> {
    let text: Option<String> = conn.query_row("SELECT value FROM setting WHERE key = 'working_hours'", [], |r| r.get(0)).optional()?;
    Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
}

/// Sets the working hours, or with None goes back to the provider's or the default.
pub fn set_working_hours(conn: &Connection, hours: Option<&crate::calendar::availability::WorkingHours>) -> Result<()> {
    match hours {
        Some(h) => conn.execute(
            "INSERT INTO setting (key, value) VALUES ('working_hours', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [serde_json::to_string(h).map_err(|e| Error::Other(format!("working hours: {e}")))?],
        )?,
        None => conn.execute("DELETE FROM setting WHERE key = 'working_hours'", [])?,
    };
    Ok(())
}

/// Minutes a new event lasts, as Settings › Calendar sets it: 30 unless
/// changed, Outlook's default.
pub fn default_event_minutes(conn: &Connection) -> Result<i64> {
    let text: Option<String> = conn.query_row("SELECT value FROM setting WHERE key = 'calendar_default_length'", [], |r| r.get(0)).optional()?;
    Ok(text.and_then(|t| t.parse().ok()).filter(|m: &i64| *m > 0).unwrap_or(30))
}

pub fn set_default_event_minutes(conn: &Connection, minutes: i64) -> Result<()> {
    if !(5..=24 * 60).contains(&minutes) {
        return Err(Error::Other(format!("{minutes} minutes isn't a length for a new event")));
    }
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('calendar_default_length', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [minutes.to_string()],
    )?;
    Ok(())
}

/// Makes `id` the calendar new events go to, and no other; None goes back
/// to the first shown calendar Jolta can write to.
pub fn set_default_calendar(conn: &Connection, id: Option<CalendarId>) -> Result<()> {
    if let Some(id) = id {
        calendar(conn, id)?;
    }
    conn.execute("UPDATE calendar SET is_default = (id IS ?1)", [id])?;
    Ok(())
}

pub fn set_calendar_busy(conn: &Connection, id: CalendarId, busy: bool) -> Result<()> {
    conn.execute("UPDATE calendar SET counts_as_busy = ?2 WHERE id = ?1", params![id, busy])?;
    Ok(())
}

pub fn set_calendar_notify(conn: &Connection, id: CalendarId, notify: bool) -> Result<()> {
    conn.execute("UPDATE calendar SET notify = ?2 WHERE id = ?1", params![id, notify])?;
    Ok(())
}

/// None goes back to the server's color.
pub fn set_calendar_color(conn: &Connection, id: CalendarId, color: Option<&str>) -> Result<()> {
    conn.execute("UPDATE calendar SET user_color = ?2 WHERE id = ?1", params![id, color])?;
    Ok(())
}

/// Saves where the next sync starts, together with the changes it covers.
pub fn set_calendar_cursor(conn: &Connection, id: CalendarId, cursor: Option<&str>, refresh: Option<i64>) -> Result<()> {
    conn.execute(
        "UPDATE calendar SET cursor = ?2, refresh = coalesce(?3, refresh), synced = unixepoch() WHERE id = ?1",
        params![id, cursor, refresh],
    )?;
    Ok(())
}

pub fn account_calendar_on(conn: &Connection, account: AccountId) -> Result<bool> {
    Ok(conn.query_row("SELECT calendar FROM account WHERE id = ?1", [account], |r| r.get(0))?)
}

/// Turning calendar off deletes the account's calendars, events and sync
/// state here. The server keeps everything.
pub fn set_account_calendar(conn: &Connection, account: AccountId, on: bool) -> Result<()> {
    conn.execute("UPDATE account SET calendar = ?2 WHERE id = ?1", params![account, on])?;
    if !on {
        conn.execute("DELETE FROM calendar WHERE account_id = ?1", [account])?;
    }
    Ok(())
}

pub fn caldav_url(conn: &Connection, account: AccountId) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT caldav_url FROM account WHERE id = ?1", [account], |r| r.get(0))?)
}

pub fn set_caldav_url(conn: &Connection, account: AccountId, url: Option<&str>) -> Result<()> {
    conn.execute("UPDATE account SET caldav_url = ?2 WHERE id = ?1", params![account, url])?;
    Ok(())
}

/// The remote objects stored for a calendar with their ETags, so a sync
/// fetches only what changed and can tell what disappeared.
pub fn known_items(conn: &Connection, calendar: CalendarId) -> Result<BTreeMap<String, Option<String>>> {
    let mut stmt = conn.prepare("SELECT remote_id, max(etag) FROM event WHERE calendar_id = ?1 GROUP BY remote_id")?;
    let rows = stmt.query_map([calendar], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// How a recurrence id is stored: the same moment always gives the same text.
pub fn moment_text(m: &Moment) -> String {
    match m {
        Moment::Date(d) => d.format("%Y%m%d").to_string(),
        Moment::Time(t) => match t.zone {
            Zone::Tz(tz) => format!("{} {tz}", t.wall.format("%Y%m%dT%H%M%S")),
            Zone::Floating => t.wall.format("%Y%m%dT%H%M%S").to_string(),
        },
    }
}

/// The moment `moment_text` wrote; None for '' and anything it didn't write.
pub(super) fn moment_from_text(text: &str) -> Option<Moment> {
    let (wall, zone) = text.split_once(' ').map_or((text, None), |(wall, zone)| (wall, Some(zone)));
    if wall.len() == 8 && zone.is_none() {
        return NaiveDate::parse_from_str(wall, "%Y%m%d").ok().map(Moment::Date);
    }
    let wall = chrono::NaiveDateTime::parse_from_str(wall, "%Y%m%dT%H%M%S").ok()?;
    let zone = match zone {
        Some(name) => Zone::Tz(name.parse().ok()?),
        None => Zone::Floating,
    };
    Some(Moment::Time(crate::calendar::Local { wall, zone }))
}

/// Rows a calendar op still has to send: an edit of that event or
/// occurrence, or a create or delete of its whole series. A sync, or any
/// other version from the server, leaves them as the user saved them, with
/// the ETag and text the op's edit started from, so its write stays guarded
/// by that version and a change made elsewhere meanwhile goes through the
/// op's merge instead of being lost or overwriting the edit.
const HELD: &str = "EXISTS (SELECT 1 FROM calendar_op o WHERE o.calendar_id = event.calendar_id AND o.uid = event.uid
       AND o.state IN ('pending', 'sending') AND (o.recurrence_id = event.recurrence_id OR o.kind IN ('create', 'delete', 'cancel')
                                   OR json_array_length(o.target, '$.rows') > 0))";

/// Whether a calendar op still has to send the row with this UID and
/// recurrence id (`HELD`).
fn held(conn: &Connection, calendar: CalendarId, uid: &str, recurrence_id: Option<&Moment>) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM calendar_op WHERE calendar_id = ?1 AND uid = ?2 AND state IN ('pending', 'sending')
               AND (recurrence_id = ?3 OR kind IN ('create', 'delete', 'cancel') OR json_array_length(target, '$.rows') > 0))",
        )?
        .query_row(params![calendar, uid, recurrence_id.map(moment_text).unwrap_or_default()], |r| r.get(0))?)
}

/// Marks the ops whose rows of `remote_id` a server version just passed
/// over: the rows still there after its delete, and a queued delete of the
/// object, whose rows are gone already.
fn passed_over(conn: &Connection, calendar: CalendarId, remote_id: &str) -> Result<()> {
    conn.prepare_cached(
        "UPDATE calendar_op SET target = json_set(target, '$.passed_over', json('true'))
         WHERE calendar_id = ?1 AND state IN ('pending', 'sending')
           AND (uid IN (SELECT uid FROM event WHERE calendar_id = ?1 AND remote_id = ?2)
                OR json_extract(target, '$.held.remote_id') = ?2)",
    )?
    .execute(params![calendar, remote_id])?;
    Ok(())
}

/// Marks the waiting ops of `uid`: a server version of their rows went
/// unstored. An op that lands stores the server's version anyway; one that
/// is dropped or fails for good asks for the calendar to be listed again.
fn mark_passed_over(conn: &Connection, calendar: CalendarId, uid: &str) -> Result<()> {
    conn.prepare_cached(
        "UPDATE calendar_op SET target = json_set(target, '$.passed_over', json('true'))
         WHERE calendar_id = ?1 AND uid = ?2 AND state IN ('pending', 'sending')",
    )?
    .execute(params![calendar, uid])?;
    Ok(())
}

/// Drops a calendar's sync cursor, so its next sync lists it whole again
/// and stores what earlier rounds passed over.
pub fn forget_calendar_cursor(conn: &Connection, calendar: CalendarId) -> Result<()> {
    conn.execute("UPDATE calendar SET cursor = NULL WHERE id = ?1", [calendar])?;
    Ok(())
}

/// Replaces the events of one remote object and rebuilds the occurrences of
/// every series it touched, except rows a calendar op still has to send
/// (`HELD`). An unchanged ETag skips the work, so applying the same batch
/// twice is harmless. False when nothing changed.
pub fn apply_item(
    conn: &Connection,
    calendar: CalendarId,
    remote_id: &str,
    etag: Option<&str>,
    events: &[EventData],
    blob: Option<&str>,
    horizon: &Horizon,
) -> Result<bool> {
    if let Some(etag) = etag {
        let (rows, same): (i64, i64) = conn.query_row(
            "SELECT count(*), count(*) FILTER (WHERE etag = ?3) FROM event WHERE calendar_id = ?1 AND remote_id = ?2",
            params![calendar, remote_id, etag],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if rows > 0 && rows == same {
            return Ok(false);
        }
    }
    let mut uids = item_uids(conn, calendar, remote_id)?;
    conn.prepare_cached(&format!("DELETE FROM event WHERE calendar_id = ?1 AND remote_id = ?2 AND NOT {HELD}"))?.execute(params![calendar, remote_id])?;
    passed_over(conn, calendar, remote_id)?;
    for e in events {
        uids.insert(e.uid.clone());
        if held(conn, calendar, &e.uid, e.recurrence_id.as_ref())? {
            mark_passed_over(conn, calendar, &e.uid)?;
        } else {
            insert_event(conn, calendar, remote_id, etag, e, blob)?;
        }
    }
    for uid in &uids {
        rebuild_series(conn, calendar, uid, horizon)?;
    }
    Ok(true)
}

/// Stores one event of a remote object beside the others, replacing only an
/// event with the same recurrence id. The caller rebuilds the series.
pub fn insert_event(
    conn: &Connection,
    calendar: CalendarId,
    remote_id: &str,
    etag: Option<&str>,
    e: &EventData,
    blob: Option<&str>,
) -> Result<()> {
    let data = serde_json::to_string(e).map_err(|err| Error::Other(format!("event {}: {err}", e.uid)))?;
    conn.prepare_cached(
        "INSERT OR REPLACE INTO event (calendar_id, remote_id, etag, uid, recurrence_id, data, title, location, status,
                                       show_as, my_response, sequence, meeting_provider, join_url, has_attendees, blob)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
    )?
    .execute(params![
        calendar,
        remote_id,
        etag,
        e.uid,
        e.recurrence_id.as_ref().map(moment_text).unwrap_or_default(),
        data,
        e.title,
        e.location,
        e.status,
        e.show_as,
        e.my_response(),
        e.sequence,
        e.meeting.as_ref().map(|m| m.provider),
        e.meeting.as_ref().map(|m| m.join_url.as_str()),
        !e.attendees.is_empty(),
        blob,
    ])?;
    Ok(())
}

/// Rewrites a stored event's data and answer in place. Its ETag stays, so
/// the next sync still takes the server's version over this one. The caller
/// rebuilds the series.
pub fn set_event_data(conn: &Connection, id: EventId, e: &EventData) -> Result<()> {
    let data = serde_json::to_string(e).map_err(|err| Error::Other(format!("event {}: {err}", e.uid)))?;
    conn.execute("UPDATE event SET data = ?2, my_response = ?3 WHERE id = ?1", params![id, data, e.my_response()])?;
    Ok(())
}

/// Why a remote object leaves the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The server said the object was deleted. A deleted series takes the
    /// changed occurrences filed under it (`master_id`) along, since Graph's
    /// delta reports a deleted series by its master's id alone.
    Deleted,
    /// A full listing left the object out, or the server wouldn't give it.
    /// What is filed under it stays: the server may still list it.
    Unlisted,
}

/// Deletes the events of one remote object, except rows a calendar op
/// still has to send (`HELD`). Series they belonged to keep their other
/// events, rebuilt without them. False when there were none.
pub fn remove_item(conn: &Connection, calendar: CalendarId, remote_id: &str, removal: Removal, horizon: &Horizon) -> Result<bool> {
    let mut uids = item_uids(conn, calendar, remote_id)?;
    let mut deleted = conn
        .prepare_cached(&format!("DELETE FROM event WHERE calendar_id = ?1 AND remote_id = ?2 AND NOT {HELD}"))?
        .execute(params![calendar, remote_id])?;
    passed_over(conn, calendar, remote_id)?;
    // The series may be gone already, left out of an earlier listing, while
    // its changed occurrences stayed: find them by the id they are filed under.
    if removal == Removal::Deleted {
        uids.extend(filed_under_uids(conn, calendar, remote_id)?);
        deleted += conn
            .prepare_cached(&format!("DELETE FROM event WHERE calendar_id = ?1 AND json_extract(data, '$.master_id') = ?2 AND NOT {HELD}"))?
            .execute(params![calendar, remote_id])?;
    }
    for uid in &uids {
        rebuild_series(conn, calendar, uid, horizon)?;
    }
    Ok(deleted > 0)
}

fn filed_under_uids(conn: &Connection, calendar: CalendarId, master: &str) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT uid FROM event WHERE calendar_id = ?1 AND json_extract(data, '$.master_id') = ?2")?;
    let rows = stmt.query_map(params![calendar, master], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn item_uids(conn: &Connection, calendar: CalendarId, remote_id: &str) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT uid FROM event WHERE calendar_id = ?1 AND remote_id = ?2")?;
    let rows = stmt.query_map(params![calendar, remote_id], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The series, as (calendar, UID), whose occurrences were placed in another
/// zone than `zone`, with a window starting before `oldest`, or never.
pub fn stale_series(conn: &Connection, zone: Tz, oldest: i64) -> Result<Vec<(CalendarId, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT e.calendar_id, e.uid FROM event e
         LEFT JOIN series_built b ON b.calendar_id = e.calendar_id AND b.uid = e.uid
         WHERE b.uid IS NULL OR b.zone <> ?1 OR b.start < ?2
         ORDER BY e.calendar_id, e.uid",
    )?;
    let rows = stmt.query_map(params![zone.name(), oldest], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Rewrites the occurrences of one series from all its events in the
/// calendar, and records the horizon they were placed with beside them.
/// `occurrence.busy` holds the event's own answer; readers combine it with
/// the calendar's "counts as busy", so changing that needs no rewrite.
pub fn rebuild_series(conn: &Connection, calendar: CalendarId, uid: &str, horizon: &Horizon) -> Result<()> {
    conn.prepare_cached(
        "DELETE FROM occurrence WHERE series_id IN (SELECT id FROM event WHERE calendar_id = ?1 AND uid = ?2)",
    )?
    .execute(params![calendar, uid])?;

    let mut stmt =
        conn.prepare_cached("SELECT id, recurrence_id = '', data FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY id")?;
    let rows = stmt.query_map(params![calendar, uid], |r| {
        Ok((r.get::<_, EventId>(0)?, r.get::<_, bool>(1)?, r.get::<_, String>(2)?))
    })?;
    let mut master: Option<(EventId, EventData)> = None;
    let mut overrides: Vec<(EventId, EventData)> = Vec::new();
    for row in rows {
        let (id, is_master, data) = row?;
        let event: EventData =
            serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))?;
        if is_master && master.is_none() {
            master = Some((id, event));
        } else if !is_master {
            overrides.push((id, event));
        }
    }
    let override_data: Vec<EventData> = overrides.iter().map(|(_, e)| e.clone()).collect();

    let mut insert = conn.prepare_cached(
        "INSERT INTO occurrence (calendar_id, series_id, event_id, start_utc, end_utc, start_day, end_day, busy)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for occ in series::place(master.as_ref().map(|(_, e)| e), &override_data, horizon) {
        let (event_id, event) = match occ.source {
            Source::Master => master.as_ref().map(|(id, e)| (*id, e)),
            Source::Override(i) => overrides.get(i).map(|(id, e)| (*id, e)),
        }
        .expect("place names one of the events it was given");
        let series_id = master.as_ref().map_or(event_id, |(id, _)| *id);
        let (start_utc, end_utc, start_day, end_day) = match occ.span.bounds(horizon.here) {
            Bounds::Utc { start, end } => (Some(start.timestamp()), Some(end.timestamp()), None, None),
            Bounds::Days { start, end } => (None, None, Some(start), Some(end)),
        };
        insert.execute(params![
            calendar,
            series_id,
            event_id,
            start_utc,
            end_utc,
            start_day,
            end_day,
            series::busy(event)
        ])?;
    }
    if master.is_none() && overrides.is_empty() {
        conn.prepare_cached("DELETE FROM series_built WHERE calendar_id = ?1 AND uid = ?2")?.execute(params![calendar, uid])?;
    } else {
        conn.prepare_cached(
            "INSERT INTO series_built (calendar_id, uid, start, zone) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (calendar_id, uid) DO UPDATE SET start = excluded.start, zone = excluded.zone",
        )?
        .execute(params![calendar, uid, horizon.window.start.timestamp(), horizon.here.name()])?;
    }
    Ok(())
}

/// An online meeting's provider and join link, as a list row shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinLink {
    pub provider: MeetingProvider,
    pub url: String,
}

/// One occurrence with what a week view draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OccurrenceRow {
    /// The override when this occurrence was changed, else the series.
    pub event_id: EventId,
    pub calendar_id: CalendarId,
    /// With the calendar and the bounds, names the occurrence across syncs,
    /// which replace event rows and their ids.
    pub uid: String,
    pub color: Option<String>,
    pub title: String,
    pub location: String,
    pub bounds: Bounds,
    pub status: EventStatus,
    pub show_as: ShowAs,
    pub my_response: Option<Response>,
    pub meeting: Option<JoinLink>,
    pub has_attendees: bool,
    /// Blocks time: the event isn't free, cancelled or declined, and its
    /// calendar counts as busy.
    pub busy: bool,
    /// How many shown calendars hold this meeting (same UID and time); the
    /// views show it once.
    pub copies: u32,
    /// An edit of it waits to reach the server.
    pub not_synced: bool,
}

impl OccurrenceRow {
    pub fn all_day(&self) -> bool {
        matches!(self.bounds, Bounds::Days { .. })
    }
}

const OCCURRENCE_COLS: &str = "o.event_id, o.calendar_id, coalesce(c.user_color, c.color), e.title, e.location,
     o.start_utc, o.end_utc, o.start_day, o.end_day, e.status, e.show_as, e.my_response, e.meeting_provider,
     e.join_url, e.has_attendees, o.busy AND c.counts_as_busy, e.uid, c.source, c.is_default, e.changed_locally";

fn occurrence_row(r: &Row) -> rusqlite::Result<OccurrenceRow> {
    let bounds = match (r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?) {
        (Some(start), Some(end)) => Bounds::Utc { start: utc(start), end: utc(end) },
        _ => Bounds::Days { start: r.get(7)?, end: r.get(8)? },
    };
    let meeting = match (r.get::<_, Option<MeetingProvider>>(12)?, r.get::<_, Option<String>>(13)?) {
        (Some(provider), Some(url)) => Some(JoinLink { provider, url }),
        _ => None,
    };
    Ok(OccurrenceRow {
        event_id: r.get(0)?,
        calendar_id: r.get(1)?,
        uid: r.get(16)?,
        color: r.get(2)?,
        title: r.get(3)?,
        location: r.get(4)?,
        bounds,
        status: r.get(9)?,
        show_as: r.get(10)?,
        my_response: r.get(11)?,
        meeting,
        has_attendees: r.get(14)?,
        busy: r.get(15)?,
        copies: 1,
        not_synced: r.get(19)?,
    })
}

fn utc(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(seconds, 0).unwrap_or_default()
}

/// The occurrences in shown calendars that overlap the days `from` up to
/// `to` (exclusive) in `here`, ordered by start. All-day events start at
/// midnight in `here` for the ordering.
///
/// The start index alone would read every occurrence before `to`, so the
/// start is also bounded from below by the longest occurrence.
pub fn occurrences(conn: &Connection, from: NaiveDate, to: NaiveDate, here: Tz) -> Result<Vec<OccurrenceRow>> {
    let midnight = |d: NaiveDate| tz::instant(d.and_time(NaiveTime::MIN), here).timestamp();
    let (from_utc, to_utc) = (midnight(from), midnight(to));
    let (from_day, to_day) = (tz::day_number(from), tz::day_number(to));
    let (longest_utc, longest_days): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT (SELECT max(end_utc - start_utc) FROM occurrence WHERE start_utc IS NOT NULL),
                (SELECT max(end_day - start_day) FROM occurrence WHERE start_day IS NOT NULL)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {OCCURRENCE_COLS} FROM occurrence o
           JOIN calendar c ON c.id = o.calendar_id JOIN event e ON e.id = o.event_id
         WHERE o.start_utc IS NOT NULL AND o.start_utc >= ?1 - ?3 AND o.start_utc < ?2
           AND (o.end_utc > ?1 OR o.start_utc = ?1) AND c.shown
         UNION ALL
         SELECT {OCCURRENCE_COLS} FROM occurrence o
           JOIN calendar c ON c.id = o.calendar_id JOIN event e ON e.id = o.event_id
         WHERE o.start_day IS NOT NULL AND o.start_day >= ?4 - ?6 AND o.start_day < ?5
           AND o.end_day > ?4 AND c.shown"
    ))?;
    let rows = stmt.query_map(
        params![from_utc, to_utc, longest_utc.unwrap_or(0), from_day, to_day, longest_days.unwrap_or(0)],
        |r| {
            let row = occurrence_row(r)?;
            let rank = crate::calendar::series::copy_rank(&row.uid, r.get(17)?, r.get(18)?, row.calendar_id);
            Ok((row, rank))
        },
    )?;
    let mut out: Vec<OccurrenceRow> = Vec::new();
    // The same meeting arrives through two accounts, such as a Google
    // invitation to an address whose calendar is in Microsoft 365; it is
    // listed once, as the copy `copy_rank` picks.
    let mut ranks = Vec::new();
    let mut seen: HashMap<(String, Bounds), usize> = HashMap::new();
    for row in rows {
        let (row, rank) = row?;
        match seen.get(&(row.uid.clone(), row.bounds)) {
            Some(&i) => {
                let (copies, busy) = (out[i].copies + 1, out[i].busy || row.busy);
                if rank < ranks[i] {
                    out[i] = row;
                    ranks[i] = rank;
                }
                out[i].copies = copies;
                out[i].busy = busy;
            }
            None => {
                seen.insert((row.uid.clone(), row.bounds), out.len());
                out.push(row);
                ranks.push(rank);
            }
        }
    }
    let epoch = NaiveDate::default();
    out.sort_by_cached_key(|o| match o.bounds {
        Bounds::Utc { start, end } => (start.timestamp(), 1, end.timestamp()),
        Bounds::Days { start, end } => {
            (midnight(epoch + chrono::Duration::days(start.into())), 0, i64::from(end - start))
        }
    });
    Ok(out)
}

/// Whether an edit of the event waits to reach the server.
pub fn event_not_synced(conn: &Connection, id: EventId) -> Result<bool> {
    Ok(conn.query_row("SELECT changed_locally FROM event WHERE id = ?1", [id], |r| r.get(0)).optional()?.unwrap_or(false))
}

/// How many events each calendar holds, for listings. Cancelled rows don't
/// count: Google keeps reporting the cancelled occurrences of a deleted
/// series, which leaves a calendar of nothing but those to show.
pub fn event_counts(conn: &Connection) -> Result<BTreeMap<CalendarId, i64>> {
    let mut stmt = conn.prepare("SELECT calendar_id, count(*) FROM event WHERE status <> 'cancelled' GROUP BY calendar_id")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDateTime, TimeZone, Utc};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Attendee, Local, Person, Recurrence, Role, Span};
    use crate::store::Store;

    #[test]
    fn a_calendar_counts_only_the_events_it_shows() {
        let s = store("counts");
        let cal = work_calendar(&s);
        apply(&s, cal, "/cal/work/coffee.ics", None, &[EventData::new("coffee", timed("2026-11-10 10:00", 30))]);
        let gone = EventData { status: crate::calendar::EventStatus::Cancelled, ..moved("deleted-series", "2026-11-16 09:00", "2026-11-16 09:00") };
        apply(&s, cal, "deleted-series_20261116T080000Z", None, &[gone]);
        assert_eq!(november(&s).len(), 1);
        assert_eq!(s.read(event_counts).unwrap().get(&cal), Some(&1), "a cancelled occurrence is no event of the calendar");
    }

    #[test]
    fn a_stored_recurrence_id_reads_back_as_its_moment() {
        let wall = NaiveDateTime::parse_from_str("2026-11-05 09:30", "%Y-%m-%d %H:%M").unwrap();
        for m in [
            Moment::Date(NaiveDate::from_ymd_opt(2026, 11, 5).unwrap()),
            Moment::Time(Local { wall, zone: Zone::Tz(Brussels) }),
            Moment::Time(Local { wall, zone: Zone::Floating }),
        ] {
            assert_eq!(moment_from_text(&moment_text(&m)), Some(m));
        }
        assert_eq!(moment_from_text(""), None);
        assert_eq!(moment_from_text("20261105T093000 Mars/Olympus"), None);
    }

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("jolta-cal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        s.write(|tx| {
            tx.execute_batch(
                "INSERT INTO account (id, kind, address, username, secret_ref, created, calendar)
                 VALUES (1, 'imap', 'me@example.com', 'me', 'a1', 0, 1)",
            )?;
            Ok(())
        })
        .unwrap();
        s
    }

    fn horizon() -> Horizon {
        Horizon::around(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(), Brussels)
    }

    fn remote(id: &str, primary: bool) -> RemoteCalendar {
        RemoteCalendar {
            remote_id: id.into(),
            name: id.into(),
            color: Some("#3366ff".into()),
            zone: None,
            access: Access::Owner,
            primary,
            refresh: None,
        }
    }

    fn work_calendar(s: &Store) -> CalendarId {
        s.write(|tx| sync_calendar_list(tx, AccountId(1), CalendarSource::CalDav, &[remote("/cal/work/", true)]))
            .unwrap()[0]
            .id
    }

    fn local(s: &str) -> Local {
        Local {
            wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(),
            zone: Zone::Tz(Brussels),
        }
    }

    fn timed(start: &str, minutes: i64) -> Span {
        let start = local(start);
        Span::Timed { start, end: Local { wall: start.wall + Duration::minutes(minutes), ..start } }
    }

    fn weekly(uid: &str, start: &str, count: u32) -> EventData {
        EventData {
            title: "Standup".into(),
            recurrence: Some(Recurrence { rules: vec![format!("FREQ=WEEKLY;COUNT={count}")], ..Recurrence::default() }),
            ..EventData::new(uid, timed(start, 30))
        }
    }

    fn moved(uid: &str, original: &str, to: &str) -> EventData {
        EventData {
            title: "Standup, moved".into(),
            recurrence_id: Some(Moment::Time(local(original))),
            ..EventData::new(uid, timed(to, 30))
        }
    }

    fn apply(s: &Store, cal: CalendarId, remote_id: &str, etag: Option<&str>, events: &[EventData]) -> bool {
        s.write(|tx| apply_item(tx, cal, remote_id, etag, events, None, &horizon())).unwrap()
    }

    fn remove(s: &Store, cal: CalendarId, remote_id: &str) -> bool {
        s.write(|tx| remove_item(tx, cal, remote_id, Removal::Deleted, &horizon())).unwrap()
    }

    /// (start, title) of every occurrence in November 2026, in order.
    fn november(s: &Store) -> Vec<(String, String)> {
        let rows = s
            .read(|c| occurrences(c, NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(), Brussels))
            .unwrap();
        rows.into_iter()
            .map(|o| {
                let start = match o.bounds {
                    Bounds::Utc { start, .. } => start.with_timezone(&Brussels).format("%m-%d %H:%M").to_string(),
                    Bounds::Days { start, .. } => format!("day {start}"),
                };
                (start, o.title)
            })
            .collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    /// Every stored row without ids, which change when rows are rewritten.
    fn snapshot(s: &Store) -> (Vec<String>, Vec<String>) {
        s.read(|c| {
            let events = c
                .prepare("SELECT remote_id || '|' || uid || '|' || recurrence_id || '|' || ifnull(etag, '') || '|' || data FROM event ORDER BY 1")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let occurrences = c
                .prepare(
                    "SELECT m.remote_id || '|' || e.remote_id || '|' || e.recurrence_id || '|' || ifnull(o.start_utc, o.start_day) || '|' || o.busy
                     FROM occurrence o JOIN event e ON e.id = o.event_id JOIN event m ON m.id = o.series_id ORDER BY 1",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok((events, occurrences))
        })
        .unwrap()
    }

    #[test]
    fn series_rebuilds_across_objects_whatever_order_they_arrive_in() {
        let s = store("series");
        let cal = work_calendar(&s);

        apply(&s, cal, "override.ics", Some("o1"), &[moved("u1", "2026-11-10 09:00", "2026-11-10 14:00")]);
        assert_eq!(november(&s), pairs(&[("11-10 14:00", "Standup, moved")]), "an override without its master still shows");

        apply(&s, cal, "master.ics", Some("m1"), &[weekly("u1", "2026-11-03 09:00", 3)]);
        assert_eq!(
            november(&s),
            pairs(&[("11-03 09:00", "Standup"), ("11-10 14:00", "Standup, moved"), ("11-17 09:00", "Standup")]),
            "the master arriving later takes the override in"
        );

        let renamed = EventData { title: "Team standup".into(), ..weekly("u1", "2026-11-03 09:00", 4) };
        apply(&s, cal, "master.ics", Some("m2"), &[renamed]);
        assert_eq!(
            november(&s),
            pairs(&[
                ("11-03 09:00", "Team standup"),
                ("11-10 14:00", "Standup, moved"),
                ("11-17 09:00", "Team standup"),
                ("11-24 09:00", "Team standup")
            ]),
            "a replaced master keeps the override from the other object"
        );

        remove(&s, cal, "override.ics");
        assert_eq!(
            november(&s),
            pairs(&[
                ("11-03 09:00", "Team standup"),
                ("11-10 09:00", "Team standup"),
                ("11-17 09:00", "Team standup"),
                ("11-24 09:00", "Team standup")
            ]),
            "removing the override brings the occurrence back"
        );

        remove(&s, cal, "master.ics");
        assert!(november(&s).is_empty());
        let left: i64 = s.read(|c| Ok(c.query_row("SELECT (SELECT count(*) FROM event) + (SELECT count(*) FROM occurrence)", [], |r| r.get(0))?)).unwrap();
        assert_eq!(left, 0, "deleting the series leaves no rows");
    }

    /// Graph reports a deleted series by its master's id alone, so the
    /// changed occurrences filed under it go with it. An event that only
    /// shares the UID, such as a copy added from an invitation, stays.
    #[test]
    fn removing_a_series_removes_the_occurrences_filed_under_it() {
        let s = store("filed");
        let cal = work_calendar(&s);
        apply(&s, cal, "M1", Some("m1"), &[weekly("u1", "2026-11-03 09:00", 3)]);
        apply(&s, cal, "X1", Some("x1"), &[EventData { master_id: Some("M1".into()), ..moved("u1", "2026-11-10 09:00", "2026-11-10 14:00") }]);
        apply(&s, cal, "copy", Some("c1"), &[moved("u1", "2026-11-17 09:00", "2026-11-17 16:00")]);
        assert_eq!(november(&s), pairs(&[("11-03 09:00", "Standup"), ("11-10 14:00", "Standup, moved"), ("11-17 16:00", "Standup, moved")]));

        assert!(remove(&s, cal, "M1"));
        assert_eq!(november(&s), pairs(&[("11-17 16:00", "Standup, moved")]), "the moved occurrence leaves with its series");
    }

    /// A series first left out (its occurrences kept), then confirmed
    /// deleted: the occurrences filed under it go then, with no master row
    /// left to find them through.
    #[test]
    fn a_series_confirmed_deleted_after_it_went_unlisted_takes_its_occurrences() {
        let s = store("filed-later");
        let cal = work_calendar(&s);
        apply(&s, cal, "M1", Some("m1"), &[weekly("u1", "2026-11-03 09:00", 3)]);
        apply(&s, cal, "X1", Some("x1"), &[EventData { master_id: Some("M1".into()), ..moved("u1", "2026-11-10 09:00", "2026-11-10 14:00") }]);
        let horizon = horizon();
        assert!(s.write(|tx| remove_item(tx, cal, "M1", Removal::Unlisted, &horizon)).unwrap());
        assert_eq!(november(&s), pairs(&[("11-10 14:00", "Standup, moved")]), "unlisted: the moved occurrence stays");
        assert!(s.write(|tx| remove_item(tx, cal, "M1", Removal::Deleted, &horizon)).unwrap());
        assert!(november(&s).is_empty(), "deleted later: it goes too");
        let left: i64 = s.read(|c| Ok(c.query_row("SELECT (SELECT count(*) FROM event) + (SELECT count(*) FROM occurrence)", [], |r| r.get(0))?)).unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn applying_the_same_batch_twice_changes_nothing() {
        let s = store("idempotent");
        let cal = work_calendar(&s);
        let batch = |s: &Store, etag: Option<&str>| {
            apply(s, cal, "master.ics", etag, &[weekly("u1", "2026-11-03 09:00", 3), moved("u1", "2026-11-10 09:00", "2026-11-10 14:00")])
                | apply(s, cal, "single.ics", etag, &[EventData { title: "Lunch".into(), ..EventData::new("u2", timed("2026-11-05 12:00", 60)) }])
        };
        assert!(batch(&s, Some("e1")));
        let first = snapshot(&s);
        assert!(!batch(&s, Some("e1")), "same ETags report no change");
        assert_eq!(snapshot(&s), first, "same ETags");
        assert!(!remove(&s, cal, "never-stored.ics"));
        batch(&s, None);
        batch(&s, None);
        let without = snapshot(&s);
        assert_eq!(without.1, first.1, "without ETags the occurrences are the same");
        assert_eq!(first.1.len(), 4);
    }

    #[test]
    fn cancelled_free_and_declined_events() {
        let s = store("busy");
        let cal = work_calendar(&s);
        let declined = EventData {
            title: "Declined".into(),
            attendees: vec![Attendee {
                person: Person { address: "me@example.com".into(), name: String::new() },
                role: Role::Required,
                response: Response::Declined,
                comment: String::new(),
                is_me: true,
            }],
            ..EventData::new("d", timed("2026-11-03 11:00", 60))
        };
        let free = EventData { title: "Free".into(), show_as: ShowAs::Free, ..EventData::new("f", timed("2026-11-03 12:00", 60)) };
        let cancelled = EventData { title: "Cancelled".into(), status: EventStatus::Cancelled, ..EventData::new("c", timed("2026-11-03 13:00", 60)) };
        let busy = EventData { title: "Busy".into(), ..EventData::new("b", timed("2026-11-03 14:00", 60)) };
        for e in [&declined, &free, &cancelled, &busy] {
            apply(&s, cal, &e.uid, None, std::slice::from_ref(e));
        }
        let mut cancelled_override = moved("u1", "2026-11-10 09:00", "2026-11-10 09:00");
        cancelled_override.status = EventStatus::Cancelled;
        apply(&s, cal, "series", None, &[weekly("u1", "2026-11-03 09:00", 3), cancelled_override]);

        let rows = |s: &Store| {
            s.read(|c| occurrences(c, NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(), Brussels))
                .unwrap()
                .into_iter()
                .map(|o| (o.title, o.busy))
                .collect::<Vec<_>>()
        };
        let busy_rows = |list: &[(&str, bool)]| list.iter().map(|(t, b)| (t.to_string(), *b)).collect::<Vec<_>>();
        assert_eq!(
            rows(&s),
            busy_rows(&[("Standup", true), ("Declined", false), ("Free", false), ("Busy", true), ("Standup", true)]),
            "cancelled events and occurrences are not stored"
        );
        s.write(|tx| set_calendar_busy(tx, cal, false)).unwrap();
        assert!(rows(&s).iter().all(|(_, busy)| !busy), "a calendar that doesn't count as busy blocks nothing");
        s.write(|tx| set_calendar_shown(tx, cal, false)).unwrap();
        assert!(rows(&s).is_empty(), "hidden calendars are left out");
    }

    #[test]
    fn range_edges_long_events_and_all_day() {
        let s = store("range");
        let cal = work_calendar(&s);
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        let events = [
            EventData { title: "Conference".into(), ..EventData::new("long", timed("2026-10-01 09:00", 60 * 24 * 40)) },
            EventData { title: "Before".into(), ..EventData::new("before", timed("2026-11-08 23:00", 60)) },
            EventData { title: "Ends at the start".into(), ..EventData::new("edge", timed("2026-11-08 23:00", 60)) },
            EventData { title: "Armistice".into(), ..EventData::new("holiday", Span::AllDay { start: day(11), end: day(12) }) },
            EventData { title: "Trip".into(), ..EventData::new("trip", Span::AllDay { start: day(5), end: day(10) }) },
            EventData { title: "Next week".into(), ..EventData::new("after", timed("2026-11-16 00:00", 30)) },
        ];
        for e in &events {
            apply(&s, cal, &e.uid, None, std::slice::from_ref(e));
        }
        let titles: Vec<String> =
            s.read(|c| occurrences(c, day(9), day(16), Brussels)).unwrap().into_iter().map(|o| o.title).collect();
        assert_eq!(titles, ["Conference", "Trip", "Armistice"], "the week of 9 November in Brussels");
    }

    #[test]
    fn a_meeting_in_two_calendars_shows_and_reminds_once() {
        let s = store("copies");
        let first = work_calendar(&s);
        let second = s.write(|tx| add_calendar(tx, CalendarSource::Google, &remote("shared", false))).unwrap().id;
        s.write(|tx| set_calendar_shown(tx, second, true)).unwrap();
        let alarm = crate::calendar::Alarm { trigger: crate::calendar::AlarmTrigger::MinutesBefore(10), email: false };
        let meeting = |title: &str| EventData { title: title.into(), alarms: vec![alarm], ..EventData::new("abc@google.com", timed("2026-11-17 14:00", 60)) };
        apply(&s, second, "g1", None, &[meeting("Klium x Speed Kit")]);
        apply(&s, first, "o1", None, &[meeting("Invitation: Klium x Speed Kit")]);
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        let rows = s.read(|c| occurrences(c, day(17), day(18), Brussels)).unwrap();
        assert_eq!(
            rows.iter().map(|o| (o.title.as_str(), o.calendar_id, o.copies)).collect::<Vec<_>>(),
            [("Klium x Speed Kit", second, 2)],
            "a meeting made in Google Calendar keeps its Google copy, though the other calendar came first"
        );
        let fires = s
            .read(|c| crate::calendar::remind::between(c, Utc.with_ymd_and_hms(2026, 11, 17, 0, 0, 0).unwrap(), Utc.with_ymd_and_hms(2026, 11, 18, 0, 0, 0).unwrap(), Brussels))
            .unwrap();
        assert_eq!(fires.iter().map(|r| r.calendar_id).collect::<Vec<_>>(), [second], "one reminder, from the copy the view shows");
    }

    #[test]
    fn relisting_keeps_what_the_user_chose() {
        let s = store("relist");
        let list = [remote("/cal/home/", true), remote("/cal/shared/", false)];
        let first = s.write(|tx| sync_calendar_list(tx, AccountId(1), CalendarSource::CalDav, &list)).unwrap();
        let shown: Vec<bool> = first.iter().map(|c| c.shown).collect();
        assert_eq!(shown, [true, false], "the primary calendar starts shown, others hidden");
        let shared = first[1].id;
        s.write(|tx| {
            set_calendar_shown(tx, shared, true)?;
            set_calendar_color(tx, shared, Some("#ff0000"))
        })
        .unwrap();
        let mut renamed = list.clone();
        renamed[1].name = "Shared, renamed".into();
        renamed[1].color = Some("#00ff00".into());
        let again = s.write(|tx| sync_calendar_list(tx, AccountId(1), CalendarSource::CalDav, &renamed[1..])).unwrap();
        assert_eq!(again.len(), 1, "the calendar gone from the server is deleted");
        assert_eq!((again[0].id, again[0].shown, again[0].color.as_deref()), (shared, true, Some("#ff0000")));
        assert_eq!(again[0].name, "Shared, renamed");
    }

    #[test]
    fn turning_calendar_off_leaves_nothing_behind() {
        let s = store("off");
        let cal = work_calendar(&s);
        apply(&s, cal, "master.ics", Some("m1"), &[weekly("u1", "2026-11-03 09:00", 3)]);
        s.write(|tx| set_account_calendar(tx, AccountId(1), false)).unwrap();
        let left: i64 = s
            .read(|c| Ok(c.query_row("SELECT (SELECT count(*) FROM calendar) + (SELECT count(*) FROM event) + (SELECT count(*) FROM occurrence)", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(left, 0);
        assert!(!s.read(|c| account_calendar_on(c, AccountId(1))).unwrap());
    }
}
