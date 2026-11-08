//! Answering invitations: accept, maybe or decline.
//!
//! An event in a synced calendar is answered through that calendar's
//! provider, which sends the organizer one reply: Graph's `accept`,
//! `tentativelyAccept` and `decline`; on Google a patch of the user's own
//! attendee entry; on CalDAV the user's PARTSTAT in the stored object, after
//! which the server's scheduling sends the reply. Answers go out at once and
//! queue while the provider can't be reached (`answer_or_queue`).
//!
//! `imip_reply` writes the reply mail (RFC 6047) Jolta sends itself: for an
//! invitation no synced calendar holds, and on iCloud, which doesn't tell an
//! outside organizer. `respond` decides which an answer needs.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Tz;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::ical_edit::{Component, Object, Prop};
use super::series::{self, Horizon};
use super::sources::Change;
use super::ops::{self, Answered};
use super::{Attendee, Bounds, Calendar, CalendarSource, EventData, EventId, EventStatus, Local, Moment, Person, Response, Role, Span, Zone, sync, tz};
use crate::compose::Built;
use crate::{Error, Jolta, Result, store};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Answer {
    Accept,
    Maybe,
    Decline,
}

impl Answer {
    pub fn response(self) -> Response {
        match self {
            Answer::Accept => Response::Accepted,
            Answer::Maybe => Response::Tentative,
            Answer::Decline => Response::Declined,
        }
    }

    pub(crate) fn partstat(self) -> &'static str {
        match self {
            Answer::Accept => "ACCEPTED",
            Answer::Maybe => "TENTATIVE",
            Answer::Decline => "DECLINED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The whole series, or the single event.
    Series,
    /// One occurrence of a series, named by its recurrence id: its original
    /// start on the series' own clock. `occurrence` finds it from a day.
    Occurrence(Moment),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rsvp {
    pub event: EventId,
    pub answer: Answer,
    /// Goes with the answer. Without one, an earlier note is cleared.
    pub note: Option<String>,
    /// False for "Don't send a reply": the answer is saved and the organizer
    /// isn't told.
    pub notify: bool,
    pub scope: Scope,
    /// A new time proposed with a Maybe or a Decline, which Graph sends as
    /// `proposedNewTime`. The other providers have no field for it; Jolta
    /// mails those a COUNTER.
    pub proposal: Option<Proposal>,
}

/// A stored event, as a source needs it to answer.
#[derive(Clone, Debug)]
pub struct Stored {
    pub id: EventId,
    pub remote_id: String,
    pub etag: Option<String>,
    /// The object as the server gave it at the last sync: the iCalendar
    /// resource for CalDAV, the event's JSON for Graph and Google.
    pub raw: Option<Vec<u8>>,
    pub data: EventData,
}

/// What one answer goes to.
#[derive(Clone, Debug)]
pub struct Target {
    /// The series master or single event. An occurrence stored without its
    /// master stands in for it.
    pub event: Stored,
    /// Set when one occurrence of a series is answered.
    pub occurrence: Option<Occurrence>,
    /// The user's address on the calendar's account.
    pub me: String,
}

#[derive(Clone, Debug)]
pub struct Occurrence {
    pub recurrence_id: Moment,
    /// The occurrence's override, when it was changed before.
    pub stored: Option<Stored>,
}

impl Target {
    /// The recurrence id of the VEVENT the answer changes: the occurrence's,
    /// or the event's own (None for a master or a single event).
    pub fn recurrence_id(&self) -> Option<Moment> {
        self.occurrence.as_ref().map(|o| o.recurrence_id).or(self.event.data.recurrence_id)
    }
}

/// Answers through the provider of the event's calendar, then sets the
/// stored answer so views change at once. The stored ETag stays, so the
/// next sync replaces the stored event with the server's version.
pub async fn answer(jolta: &Jolta, rsvp: &Rsvp) -> Result<()> {
    let here = tz::here();
    let (calendar, target) = target_of(jolta, rsvp.event, &rsvp.scope, None).await?;
    answerable(&calendar, &target)?;
    let source = sync::calendar_source(jolta, &calendar).await?;
    let (landed, written) = source.answer(&calendar, &target, rsvp).await?;

    let (rsvp, calendar_id) = (rsvp.clone(), calendar.id);
    let written = match written {
        Some(Change::Upsert { etag, events, raw, .. }) => Some(Written { etag, events, blob: raw.map(|r| jolta.blobs.put(&r)).transpose()? }),
        _ => None,
    };
    jolta.store.write_async(move |tx| store_answer(tx, calendar_id, &target, &rsvp, &landed, written, here)).await
}

/// `answer`, unless the provider can't be reached or an earlier change to
/// the copy still waits to be sent: then the answer is kept in the store
/// and queued behind it, to go through the provider once (`ops::queue_answer`).
/// True when it was queued.
pub async fn answer_or_queue(jolta: &Jolta, rsvp: &Rsvp) -> Result<bool> {
    let event = rsvp.event;
    let waits = jolta
        .store
        .read_async(move |c| {
            let (calendar, uid): (super::CalendarId, String) = c
                .query_row("SELECT calendar_id, uid FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("event {event}")))?;
            store::calendar_op_waiting(c, calendar, &uid)
        })
        .await?;
    if !waits {
        match answer(jolta, rsvp).await {
            Err(Error::Offline(_)) => {}
            done => return done.map(|()| false),
        }
    }
    let here = tz::here();
    let (calendar, target) = target_of(jolta, rsvp.event, &rsvp.scope, None).await?;
    answerable(&calendar, &target)?;
    let rsvp = rsvp.clone();
    jolta
        .store
        .write_async(move |tx| {
            // An occurrence without a row of its own gets one only from the
            // provider, under the id it gives it: one made here would send
            // the answer to the series' object.
            let fresh = matches!(target.occurrence, Some(Occurrence { stored: None, .. }));
            if !fresh {
                record(tx, calendar.id, &target, &rsvp, &target.event.remote_id, here)?;
            }
            let answered = Answered {
                answer: rsvp.answer,
                note: rsvp.note.clone(),
                notify: rsvp.notify,
                occurrence: target.occurrence.as_ref().map(|o| o.recurrence_id),
                proposal: rsvp.proposal,
            };
            ops::queue_answer(tx, calendar.id, &with_answer(&target.event.data, &rsvp, &target.me), answered, chrono::Utc::now().timestamp())
        })
        .await?;
    Ok(true)
}

/// Sends an answer `answer_or_queue` queued, through the provider of
/// `calendar`'s copy of `uid` as it is stored now.
pub(crate) async fn send_answer(jolta: &Jolta, calendar: super::CalendarId, uid: &str, answered: &Answered) -> Result<()> {
    let uid = uid.to_string();
    let event = jolta
        .store
        .read_async(move |c| {
            Ok(c.query_row(
                "SELECT id FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY recurrence_id <> '', id LIMIT 1",
                params![calendar, uid],
                |r| r.get::<_, EventId>(0),
            )
            .optional()?)
        })
        .await?
        .ok_or_else(|| Error::Other("the invitation is no longer in your calendar".into()))?;
    let scope = answered.occurrence.map_or(Scope::Series, Scope::Occurrence);
    let rsvp = Rsvp { event, answer: answered.answer, note: answered.note.clone(), notify: answered.notify, scope, proposal: answered.proposal };
    answer(jolta, &rsvp).await
}

/// Refuses an answer Jolta can't send through the copy's calendar, before
/// anything changes.
fn answerable(calendar: &Calendar, target: &Target) -> Result<()> {
    match calendar.source {
        CalendarSource::Local => return Err(Error::Other("This event is in a calendar on this Mac only. Answering it by mail isn't built yet.".into())),
        CalendarSource::EventKit => {
            return Err(Error::Other("Answer this invitation in the Calendar app: macOS doesn't let other apps answer for its calendars.".into()));
        }
        _ => {}
    }
    own_calendar(&calendar, &target.me)?;
    let organizer = target.event.data.organizer.as_ref().map(|o| o.address.as_str());
    if organizer.is_some_and(|o| o.eq_ignore_ascii_case(&target.me)) {
        return Err(Error::Other("You organize this meeting, so there is no invitation to answer.".into()));
    }
    if organizer.is_none() && target.event.data.attendees.is_empty() {
        return Err(Error::Other("This event has no organizer or guests, so there is no invitation to answer.".into()));
    }
    Ok(())
}

/// Refuses an answer through a calendar the account with address `account`
/// doesn't own (`Calendar::is_own`): the copy there is its owner's, whom
/// Google marks as the copy's self, so the answer would be the owner's.
pub(crate) fn own_calendar(calendar: &Calendar, account: &str) -> Result<()> {
    match calendar.is_own(account) {
        true => Ok(()),
        false => Err(Error::Other(
            "This meeting is in a calendar shared with you, so its answer is the calendar owner's. Answer from the invitation in your mail.".into(),
        )),
    }
}

/// The calendar of `event`, and what a change in `scope` goes to, with the
/// objects as the server last gave them. For a copy the user added,
/// `object` is the remote id of the object Jolta created, which is then the
/// series or single event, with only its own changed occurrences, whatever
/// else in the calendar has its UID.
pub(crate) async fn target_of(jolta: &Jolta, event: EventId, scope: &Scope, object: Option<String>) -> Result<(Calendar, Target)> {
    let here = tz::here();
    let (calendar, rows, me) = jolta.store.read_async(move |c| series_of(c, event, object.as_deref())).await?;
    let mut target = target(rows, event, scope, here, me)?;
    for stored in std::iter::once(&mut target.event).chain(target.occurrence.as_mut().and_then(|o| o.stored.as_mut())) {
        stored.raw = jolta.store.read_async({
            let id = stored.id;
            move |c| Ok(c.query_row("SELECT blob FROM event WHERE id = ?1", [id], |r| r.get::<_, Option<String>>(0))?)
        })
        .await?
        .map(|hash| jolta.blobs.get(&hash))
        .transpose()?;
    }
    Ok((calendar, target))
}

/// A resource as the answer wrote it to the server, read with every address
/// of the user.
struct Written {
    etag: Option<String>,
    events: Vec<EventData>,
    blob: Option<String>,
}

/// Stores the answer. CalDAV hands back the resource it wrote, which is
/// stored as a sync would, so aliases an earlier sync didn't know count;
/// the other providers' answers are set on the stored events.
fn store_answer(conn: &Connection, calendar: super::CalendarId, target: &Target, rsvp: &Rsvp, landed: &str, written: Option<Written>, here: Tz) -> Result<()> {
    match written {
        Some(w) => {
            // An answer that changed nothing on the server keeps its ETag, which
            // would make `apply_item` skip the copy read with the aliases.
            conn.execute("UPDATE event SET etag = NULL WHERE calendar_id = ?1 AND remote_id = ?2", params![calendar, landed])?;
            store::apply_item(conn, calendar, landed, w.etag.as_deref(), &w.events, w.blob.as_deref(), &Horizon::now()).map(|_| ())
        }
        None => record(conn, calendar, target, rsvp, landed, here),
    }
}

/// The changed occurrences stored of the copy whose object Jolta created
/// is `object`, the series of `event`, as the server last gave them.
pub(crate) fn changed_occurrences(conn: &Connection, event: EventId, object: &str) -> Result<Vec<Stored>> {
    let (_, rows, _) = series_of(conn, event, Some(object))?;
    Ok(rows.into_iter().filter(|r| !r.is_master).map(|r| r.stored).collect())
}

/// One stored row of a series.
struct Row {
    stored: Stored,
    is_master: bool,
}

/// The calendar of `event`, every stored event of its series, only those
/// of the copy whose object is `object` when given (`store::of_copy`), and
/// the user's address on the calendar's account.
fn series_of(conn: &Connection, event: EventId, object: Option<&str>) -> Result<(Calendar, Vec<Row>, String)> {
    let (calendar_id, uid): (super::CalendarId, String) = conn
        .query_row("SELECT calendar_id, uid FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("event {event}")))?;
    let calendar = store::calendar(conn, calendar_id)?;
    let me = match calendar.account_id {
        Some(account) => store::account(conn, account)?.address,
        None => String::new(),
    };
    let mut stmt = conn.prepare("SELECT id, remote_id, etag, recurrence_id = '', data FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY id")?;
    let rows = stmt
        .query_map(params![calendar_id, uid], |r| {
            Ok((r.get::<_, EventId>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, bool>(3)?, r.get::<_, String>(4)?))
        })?
        .map(|row| {
            let (id, remote_id, etag, is_master, data) = row?;
            let data: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))?;
            Ok(Row { stored: Stored { id, remote_id, etag, raw: None, data }, is_master })
        })
        .filter(|row| match (row, object) {
            (Ok(row), Some(object)) => store::of_copy(object, &row.stored.remote_id, &row.stored.data),
            _ => true,
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((calendar, rows, me))
}

/// What an answer in `scope` to `event` goes to, from its series' rows.
fn target(rows: Vec<Row>, event: EventId, scope: &Scope, here: Tz, me: String) -> Result<Target> {
    let (masters, overrides): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(|r| r.is_master);
    let master = masters.into_iter().next().map(|r| r.stored);
    let find = |rid: &Moment| overrides.iter().find(|r| r.stored.data.recurrence_id.is_some_and(|o| o.names(rid, here))).map(|r| r.stored.clone());
    match (scope, master) {
        (Scope::Series, Some(master)) => Ok(Target { event: master, occurrence: None, me }),
        (Scope::Series, None) => {
            let alone = overrides.into_iter().find(|r| r.stored.id == event).map(|r| r.stored).ok_or_else(|| Error::NotFound(format!("event {event}")))?;
            Ok(Target { event: alone, occurrence: None, me })
        }
        (Scope::Occurrence(rid), Some(master)) => {
            if master.data.recurrence.is_none() {
                return Err(Error::Other("This event doesn't repeat, so it has no single occurrence to answer.".into()));
            }
            let stored = find(rid);
            Ok(Target { event: master, occurrence: Some(Occurrence { recurrence_id: *rid, stored }), me })
        }
        // Invited to occurrences only: each one stands alone.
        (Scope::Occurrence(rid), None) => {
            let alone = find(rid).ok_or_else(|| Error::NotFound("Jolta has no occurrence of this meeting at that time.".into()))?;
            Ok(Target { event: alone, occurrence: None, me })
        }
    }
}

/// Sets the answer on the stored events it covers: the whole series (each
/// override the user is on), the changed occurrence, or a new override for
/// an occurrence answered on its own, stored under the remote id the answer
/// `landed` on.
fn record(conn: &Connection, calendar: super::CalendarId, target: &Target, rsvp: &Rsvp, landed: &str, here: Tz) -> Result<()> {
    let uid = &target.event.data.uid;
    let mine = |data: &EventData| with_answer(data, rsvp, &target.me);
    match &target.occurrence {
        None if target.event.data.recurrence_id.is_none() => {
            let mut stmt = conn.prepare("SELECT id, data FROM event WHERE calendar_id = ?1 AND uid = ?2")?;
            let rows: Vec<(EventId, String)> =
                stmt.query_map(params![calendar, uid], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, data) in rows {
                let data: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))?;
                if data.status != EventStatus::Cancelled && (id == target.event.id || data.attendees.iter().any(|a| a.is_me)) {
                    store::set_event_data(conn, id, &mine(&data))?;
                }
            }
        }
        None => store::set_event_data(conn, target.event.id, &mine(&target.event.data))?,
        Some(Occurrence { stored: Some(s), .. }) => store::set_event_data(conn, s.id, &mine(&s.data))?,
        Some(Occurrence { recurrence_id, stored: None }) => {
            let master = &target.event.data;
            let span = series::occurrence_span(&master.span, recurrence_id, here)
                .ok_or_else(|| Error::Other("The occurrence and its series disagree on whether they are all-day.".into()))?;
            let single = EventData { recurrence_id: Some(*recurrence_id), recurrence: None, span, ..mine(master) };
            store::insert_event(conn, calendar, landed, None, &single, None)?;
        }
    }
    store::rebuild_series(conn, calendar, uid, &Horizon::now())
}

/// `data` with the user's own answer and note.
fn with_answer(data: &EventData, rsvp: &Rsvp, me: &str) -> EventData {
    let mut data = data.clone();
    let comment = rsvp.note.clone().unwrap_or_default();
    match data.attendees.iter_mut().find(|a| a.is_me) {
        Some(a) => {
            a.response = rsvp.answer.response();
            a.comment = comment;
        }
        None => data.attendees.push(Attendee {
            person: Person { address: me.to_string(), name: String::new() },
            role: Role::Required,
            response: rsvp.answer.response(),
            comment,
            is_me: true,
        }),
    }
    data
}

/// The recurrence id of the occurrence `spec` names in the series of
/// `event`: a day such as `2026-11-12`, on which the series has one
/// occurrence in the Mac's zone, or a recurrence id as iCalendar writes it
/// (`20261112T100000` on the series' clock, `20261112T090000Z`, `20261112`).
pub fn occurrence(jolta: &Jolta, event: EventId, spec: &str) -> Result<Moment> {
    let here = tz::here();
    let (master, overrides) = jolta.store.read(|c| series_parts(c, event))?;
    let spec = spec.trim();
    if let Ok(day) = NaiveDate::parse_from_str(spec, "%Y-%m-%d") {
        let midnight = |d: NaiveDate| tz::instant(d.and_time(chrono::NaiveTime::MIN), here);
        let window = super::recur::Window { start: midnight(day), end: midnight(day + chrono::Duration::days(1)) };
        let found: Vec<Moment> = series::place(master.as_ref(), &overrides, &Horizon { window, here })
            .into_iter()
            .filter(|o| match o.span {
                Span::AllDay { start, .. } => start == day,
                Span::Timed { start, .. } => tz::wall(start.instant(here), Zone::Tz(here), here).date() == day,
            })
            .filter_map(|o| match o.source {
                super::recur::Source::Master => Some(o.span.start()),
                super::recur::Source::Override(i) => overrides[i].recurrence_id,
            })
            .collect();
        return match found.as_slice() {
            [one] => Ok(*one),
            [] => Err(Error::NotFound(format!("the series has no occurrence on {day}"))),
            _ => Err(Error::Other(format!("the series has {} occurrences on {day}; name one by its recurrence id", found.len()))),
        };
    }
    let zone = match master.as_ref().or(overrides.first()).map(|e| e.span) {
        Some(Span::Timed { start, .. }) => start.zone,
        _ => Zone::Floating,
    };
    parse_moment(spec, None, zone).ok_or_else(|| Error::Other(format!("{spec} is not a day like 2026-11-12 or a recurrence id like 20261112T100000")))
}

/// The recurrence id of the occurrence of `event`'s series that the
/// calendar views place at `at`.
pub fn occurrence_at(conn: &Connection, event: EventId, at: Bounds, here: Tz) -> Result<Moment> {
    let (master, overrides) = series_parts(conn, event)?;
    let midnight = |n: i32| tz::instant((NaiveDate::default() + chrono::Duration::days(n.into())).and_time(chrono::NaiveTime::MIN), here);
    let (start, end) = match at {
        Bounds::Utc { start, end } => (start, end),
        Bounds::Days { start, end } => (midnight(start), midnight(end)),
    };
    let day = chrono::Duration::days(1);
    let window = super::recur::Window { start: start - day, end: end + day };
    series::place(master.as_ref(), &overrides, &Horizon { window, here })
        .into_iter()
        .find(|o| o.span.bounds(here) == at)
        .and_then(|o| match o.source {
            super::recur::Source::Master => Some(o.span.start()),
            super::recur::Source::Override(i) => overrides[i].recurrence_id,
        })
        .ok_or_else(|| Error::NotFound("Jolta has no occurrence of this meeting at that time.".into()))
}

/// The master and the overrides of `event`'s series.
fn series_parts(conn: &Connection, event: EventId) -> Result<(Option<EventData>, Vec<EventData>)> {
    let (_, rows, _) = series_of(conn, event, None)?;
    let (masters, overrides): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(|r| r.is_master);
    Ok((masters.into_iter().next().map(|r| r.stored.data), overrides.into_iter().map(|r| r.stored.data).collect()))
}

/// A DATE or DATE-TIME value: `20261112`, `20261112T090000Z`, or a wall
/// time in `zone`, the TZID's zone when there is one.
fn parse_moment(value: &str, tzid: Option<&str>, zone: Zone) -> Option<Moment> {
    let value = value.trim();
    if value.len() == 8 {
        return NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(Moment::Date);
    }
    let (wall, utc) = match value.strip_suffix('Z') {
        Some(v) => (v, true),
        None => (value, false),
    };
    let wall = NaiveDateTime::parse_from_str(wall, "%Y%m%dT%H%M%S").ok()?;
    let zone = match (utc, tzid) {
        (true, _) => Zone::Tz(chrono_tz::UTC),
        (false, Some(t)) => Zone::Tz(tz::zone(t)?),
        (false, None) => zone,
    };
    Some(Moment::Time(Local { wall, zone }))
}

/// The moment a DTSTART or RECURRENCE-ID line holds. None for a TZID Jolta
/// can't place, such as Outlook's "Customized Time Zone".
pub(super) fn prop_moment(p: &Prop) -> Option<Moment> {
    if p.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) {
        return NaiveDate::parse_from_str(p.value().trim(), "%Y%m%d").ok().map(Moment::Date);
    }
    parse_moment(p.value(), p.param("TZID"), Zone::Floating)
}

/// `moment` written the way `like` (a DTSTART) writes its value: a date, a
/// UTC time, or a wall time on its TZID's clock.
pub(super) fn value_like(like: &Prop, moment: &Moment, here: Tz) -> Option<String> {
    let date = like.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || like.value().trim().len() == 8;
    match (moment, date) {
        (Moment::Date(d), true) => Some(d.format("%Y%m%d").to_string()),
        (Moment::Time(at), false) if like.value().trim().ends_with('Z') => Some(format!("{}Z", at.instant(here).format("%Y%m%dT%H%M%S"))),
        (Moment::Time(at), false) => {
            let wall = match like.param("TZID").and_then(tz::zone) {
                Some(zone) => tz::wall(at.instant(here), Zone::Tz(zone), here),
                None => at.wall,
            };
            Some(wall.format("%Y%m%dT%H%M%S").to_string())
        }
        _ => None,
    }
}

fn is_me(p: &Prop, me: &[impl AsRef<str>]) -> bool {
    p.address().or(p.param("EMAIL")).is_some_and(|a| me.iter().any(|m| a.eq_ignore_ascii_case(m.as_ref())))
}

/// The answer written into a CalDAV resource: the user's ATTENDEE line, the
/// one with any of the addresses in `me`, in the VEVENT with `uid` and `recurrence_id` gets the PARTSTAT, and with a note
/// the X-RESPONSE-COMMENT Jolta reads and the comment Apple's servers pass
/// to the organizer. Answering a series (no `recurrence_id`) answers every
/// override the user is on too. An occurrence without an override gets one,
/// copied from the master. Not notifying sets SCHEDULE-AGENT=CLIENT on every
/// ORGANIZER line of the resource, which tells a scheduling server not to
/// send the reply; RFC 6638 section 7.1 asks for the same value on all of
/// them. Every other line stays byte for byte.
pub(crate) fn answer_in_ical(text: &str, uid: &str, recurrence_id: Option<&Moment>, me: &[impl AsRef<str>], rsvp: &Rsvp, here: Tz) -> Result<String> {
    let mut obj = Object::parse(text);
    let written = match recurrence_id {
        None => None,
        Some(rid) => {
            let stored = obj.calendar().into_iter().flat_map(Component::components).find_map(|c| {
                let p = c.prop("RECURRENCE-ID").filter(|_| c.name() == "VEVENT" && c.prop("UID").is_some_and(|u| u.text() == uid))?;
                prop_moment(p).filter(|m| m.names(rid, here)).map(|_| p.value().to_string())
            });
            match stored {
                Some(value) => Some(value),
                None => Some(add_override(&mut obj, uid, rid, here)?),
            }
        }
    };
    if obj.event(uid, written.as_deref()).is_none() {
        return Err(Error::Other(format!("the calendar object has no event {uid}")));
    }
    let partstat = rsvp.answer.partstat();
    let note = rsvp.note.as_deref().map(|n| n.replace(['\r', '\n'], " "));
    let mut answered = false;
    for ev in obj.events_mut(uid) {
        ev.update("ORGANIZER", |_| true, |p| {
            if !rsvp.notify {
                p.set_param("SCHEDULE-AGENT", "CLIENT");
            } else if p.param("SCHEDULE-AGENT").is_some_and(|a| a.eq_ignore_ascii_case("CLIENT")) {
                p.remove_param("SCHEDULE-AGENT");
            }
        });
        let target = ev.prop("RECURRENCE-ID").map(|p| p.value().trim()) == written.as_deref().map(str::trim);
        if !target && written.is_some() {
            continue;
        }
        let matched = ev.update("ATTENDEE", |p| is_me(p, me), |p| {
            p.set_param("PARTSTAT", partstat);
            match &note {
                Some(n) => p.set_param("X-RESPONSE-COMMENT", n),
                None => p.remove_param("X-RESPONSE-COMMENT"),
            }
        });
        if matched == 0 {
            continue;
        }
        answered |= target;
        match &rsvp.note {
            Some(n) => ev.set_text("X-CALENDARSERVER-PRIVATE-COMMENT", n),
            None => {
                ev.remove("X-CALENDARSERVER-PRIVATE-COMMENT");
            }
        }
    }
    if !answered {
        let me = me.first().map_or("", AsRef::as_ref);
        return Err(Error::Other(format!("{me} isn't on this meeting's guest list, so Jolta can't answer for it.")));
    }
    Ok(obj.to_string())
}

/// Adds the override of an unchanged occurrence, made by `occurrence_of`.
/// Returns the RECURRENCE-ID value as written.
fn add_override(obj: &mut Object, uid: &str, rid: &Moment, here: Tz) -> Result<String> {
    let master = obj.event(uid, None).ok_or_else(|| Error::Other(format!("the calendar object has no series {uid}")))?;
    let (over, value) = occurrence_of(master, rid, here)?;
    if !obj.add_after_master(over) {
        return Err(Error::Other(format!("the calendar object has no series {uid}")));
    }
    Ok(value)
}

/// One unchanged occurrence of `master` as a VEVENT of its own: the master
/// without its repeat rules, starting at `rid`, its end moved along, with a
/// RECURRENCE-ID. Returns it and the RECURRENCE-ID value as written.
fn occurrence_of(master: &Component, rid: &Moment, here: Tz) -> Result<(Component, String)> {
    let start = master.prop("DTSTART").ok_or_else(|| Error::Other("the series has no start".into()))?.clone();
    let value = value_like(&start, rid, here).ok_or_else(|| Error::Other("the occurrence and its series disagree on whether they are all-day".into()))?;
    // DTSTART, DTEND and the new start are all written on one clock, so the
    // end moves by the same amount of wall-clock time.
    let digits = |v: &str| parse_moment(v, None, Zone::Floating);
    let shift = match (digits(start.value()), digits(&value)) {
        (Some(Moment::Time(a)), Some(Moment::Time(b))) => b.wall - a.wall,
        (Some(Moment::Date(a)), Some(Moment::Date(b))) => b - a,
        _ => chrono::Duration::zero(),
    };
    let mut over = master.clone();
    for name in ["RRULE", "RDATE", "EXDATE", "EXRULE"] {
        over.remove(name);
    }
    over.set("DTSTART", &value);
    if let Some(end) = over.prop("DTEND").map(|p| p.value().trim().to_string()) {
        let moved = match digits(&end) {
            Some(Moment::Time(at)) => Some(format!("{}{}", (at.wall + shift).format("%Y%m%dT%H%M%S"), if end.ends_with('Z') { "Z" } else { "" })),
            Some(Moment::Date(d)) => Some((d + shift).format("%Y%m%d").to_string()),
            None => None,
        };
        if let Some(moved) = moved {
            over.set("DTEND", &moved);
        }
    }
    let mut id = Prop::new("RECURRENCE-ID", &value);
    for name in ["TZID", "VALUE"] {
        if let Some(v) = start.param(name) {
            id.set_param(name, v);
        }
    }
    over.add(id);
    Ok((over, value))
}

/// Who an iMIP mail to an invitation's organizer comes from, and what it
/// answers.
pub struct ImipReply<'a> {
    /// The invitation as the organizer sent it, or the calendar's copy.
    pub invitation: &'a str,
    /// The account that sends the reply: its From address.
    pub from: &'a str,
    /// The sending account's addresses: its own and the aliases Jolta knows
    /// for it, never another account's. The ATTENDEE line the invitation has
    /// for one of them is the one answered, so an invitation to an alias is
    /// answered as that alias.
    pub me: &'a [String],
    pub answer: Answer,
    pub note: Option<&'a str>,
    /// One occurrence of a series, by its recurrence id; else all of it.
    pub occurrence: Option<Moment>,
}

/// A new time for a meeting, or for one occurrence of a series, that an
/// attendee proposes with a Maybe or a Decline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Proposal {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

/// The event of an invitation an iMIP mail answers: the series or single
/// event, or one occurrence of it, with its organizer.
struct About {
    event: Component,
    master: Component,
    organizer: Prop,
    organizer_address: String,
    /// Every VTIMEZONE of the invitation.
    zones: Vec<Component>,
}

impl About {
    /// The VEVENT of `invitation` that `occurrence` names: its changed
    /// occurrence, else one made from the series, else the event itself.
    fn of(invitation: &str, occurrence: Option<&Moment>, here: Tz) -> Result<About> {
        let invite = Object::parse(invitation);
        let calendar = invite.calendar().ok_or_else(|| Error::Other("the invitation holds no calendar".into()))?;
        if let Some(method) = calendar.prop("METHOD").map(|m| m.value().trim().to_ascii_uppercase())
            && !matches!(method.as_str(), "REQUEST" | "ADD" | "PUBLISH")
        {
            return Err(Error::Other(format!("this calendar mail is a {method}, not an invitation")));
        }
        let events: Vec<&Component> = calendar.components().filter(|c| c.name() == "VEVENT").collect();
        let master = events
            .iter()
            .find(|e| e.prop("RECURRENCE-ID").is_none())
            .or(events.first())
            .copied()
            .ok_or_else(|| Error::Other("the invitation holds no event".into()))?;
        let event = match occurrence {
            None => master.clone(),
            Some(rid) => match events.iter().find(|e| e.prop("RECURRENCE-ID").and_then(prop_moment).is_some_and(|m| m.names(rid, here))) {
                Some(changed) => (*changed).clone(),
                None if master.prop("RRULE").is_some() || master.prop("RDATE").is_some() => occurrence_of(master, rid, here)?.0,
                None => return Err(Error::Other("This invitation doesn't repeat, so it has no single occurrence to answer.".into())),
            },
        };
        let organizer = event.prop("ORGANIZER").or(master.prop("ORGANIZER")).cloned().ok_or_else(|| Error::Other("the invitation names no organizer".into()))?;
        let organizer_address = organizer
            .address()
            .or(organizer.param("EMAIL"))
            .map(str::to_string)
            .ok_or_else(|| Error::Other("the invitation's organizer has no address".into()))?;
        let zones = calendar.components().filter(|c| c.name() == "VTIMEZONE").cloned().collect();
        Ok(About { event, master: master.clone(), organizer, organizer_address, zones })
    }

    /// The user's own ATTENDEE line with `answer`'s PARTSTAT: the one the
    /// event, else its series, has for an address in `me`, or one made for
    /// `from`.
    fn my_line(&self, me: &[String], from: &str, answer: Answer) -> Prop {
        let mine = |e: &Component| e.props().find(|p| p.is("ATTENDEE") && is_me(p, me)).cloned();
        let mut attendee = mine(&self.event).or_else(|| mine(&self.master)).unwrap_or_else(|| Prop::new("ATTENDEE", &format!("mailto:{from}")));
        attendee.set_param("PARTSTAT", answer.partstat());
        for stale in ["RSVP", "SCHEDULE-STATUS", "X-RESPONSE-COMMENT"] {
            attendee.remove_param(stale);
        }
        attendee
    }

    fn title(&self) -> String {
        self.event.prop("SUMMARY").map(Prop::text).unwrap_or_default()
    }
}

/// A VEVENT with the properties of `from` named in `names`, in that order.
fn echo(from: &Component, names: &[&str]) -> Component {
    let mut out = Component::new("VEVENT");
    for name in names {
        if let Some(p) = from.prop(name) {
            out.add(p.clone());
        }
    }
    if out.prop("SEQUENCE").is_none() {
        out.add(Prop::new("SEQUENCE", "0"));
    }
    out.add(Prop::new("DTSTAMP", &format!("{}Z", Utc::now().format("%Y%m%dT%H%M%S"))));
    out
}

/// One iTIP object: `event` under `method`, with the VTIMEZONEs its own
/// properties name.
fn itip(method: &str, event: Component, zones: &[Component]) -> String {
    let named: Vec<&str> = ["DTSTART", "DTEND", "RECURRENCE-ID"].iter().filter_map(|n| event.prop(n)).filter_map(|p| p.param("TZID")).collect();
    let mut cal = Component::new("VCALENDAR");
    cal.add(Prop::text_value("PRODID", concat!("-//Jolta//Jolta ", env!("CARGO_PKG_VERSION"), "//EN")));
    cal.add(Prop::new("VERSION", "2.0"));
    cal.add(Prop::new("CALSCALE", "GREGORIAN"));
    cal.add(Prop::new("METHOD", method));
    for vtimezone in zones {
        if vtimezone.prop("TZID").is_some_and(|t| named.contains(&t.value())) {
            cal.push(vtimezone.clone());
        }
    }
    cal.push(event);
    Object::new(cal).to_string()
}

/// An iMIP mail from `from` to the one person `to` names, as "Name" and
/// address: the text, and the iTIP object as its calendar part.
pub(super) struct Mail<'a> {
    pub(super) from: &'a str,
    pub(super) to: (Option<String>, String),
    pub(super) subject: String,
    pub(super) text: String,
    pub(super) method: &'a str,
    pub(super) ics: String,
    /// Made from what the mail says, so the same mail built twice has the
    /// same id; None for one of its own.
    pub(super) same_as: Option<String>,
}

pub(super) fn imip_mail(m: Mail) -> Result<Built> {
    let seed = m.same_as.unwrap_or_else(|| format!("{}{}", SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()), m.ics));
    let domain = m.from.rsplit('@').next().unwrap_or("jolta.invalid");
    let message_id = format!("{}@{domain}", &hex::encode(Sha256::digest(seed))[..24]);
    let calendar_part = ContentType::new("text/calendar").attribute("method", m.method).attribute("charset", "utf-8");
    let (name, address) = m.to;
    let raw = MessageBuilder::new()
        .from(BuilderAddress::new_address(None::<String>, m.from.to_string()))
        .to(BuilderAddress::new_address(name, address.clone()))
        .subject(m.subject)
        .message_id(message_id.clone())
        .body(MimePart::new("multipart/alternative", vec![MimePart::new("text/plain", m.text), MimePart::new(calendar_part, m.ics)]))
        .write_to_vec()?;
    Ok(Built { raw, message_id, from: m.from.to_string(), recipients: vec![address] })
}

fn cn(p: &Prop) -> Option<String> {
    p.param("CN").map(str::to_string).filter(|n| !n.is_empty())
}

/// The iMIP reply (RFC 6047) to an invitation: a METHOD:REPLY object (RFC
/// 5546 section 3.2.3) holding only the user's own ATTENDEE line with its
/// PARTSTAT, and the invitation's UID, SEQUENCE and RECURRENCE-ID, so the
/// organizer's calendar finds the meeting. A note goes in COMMENT. It goes
/// to the organizer only, through the account's outbox like any other mail.
pub fn imip_reply(r: &ImipReply, here: Tz) -> Result<Built> {
    let about = About::of(r.invitation, r.occurrence.as_ref(), here)?;
    let mut reply = echo(&about.event, &["UID", "RECURRENCE-ID", "SEQUENCE", "DTSTART", "DTEND", "DURATION", "SUMMARY"]);
    reply.add(about.organizer.clone());
    let attendee = about.my_line(r.me, r.from, r.answer);
    let who = attendee.address().unwrap_or(r.from).to_string();
    reply.add(attendee);
    let note = r.note.filter(|n| !n.trim().is_empty());
    if let Some(n) = note {
        reply.add(Prop::text_value("COMMENT", n));
    }
    let (prefix, verb) = match r.answer {
        Answer::Accept => ("Accepted", "accepted"),
        Answer::Maybe => ("Tentative", "tentatively accepted"),
        Answer::Decline => ("Declined", "declined"),
    };
    let mut text = format!("{who} has {verb} this invitation.");
    if let Some(n) = note {
        text.push_str(&format!("\n\n{n}"));
    }
    let title = about.title();
    imip_mail(Mail {
        from: r.from,
        to: (cn(&about.organizer), about.organizer_address.clone()),
        subject: if title.is_empty() { prefix.to_string() } else { format!("{prefix}: {title}") },
        text,
        method: "REPLY",
        ics: itip("REPLY", reply, &about.zones),
        same_as: None,
    })
}

/// The iMIP proposal of a new time (RFC 5546 section 3.2.7): a
/// METHOD:COUNTER object with the invitation's UID, SEQUENCE, RECURRENCE-ID
/// and title, the proposed start and end in UTC, and the user's own ATTENDEE
/// line with the answer that goes with it, Maybe or Decline. A note goes in
/// COMMENT. It goes to the organizer only. The same proposal built twice is
/// the same mail, with the same Message-ID, so the outbox can tell it is
/// already on its way.
pub fn imip_counter(r: &ImipReply, proposal: &Proposal, here: Tz) -> Result<Built> {
    if r.answer == Answer::Accept {
        return Err(Error::Other("A new time goes with Maybe or Decline, not with Accept.".into()));
    }
    if proposal.end <= proposal.start {
        return Err(Error::Other("The proposed meeting ends before it starts.".into()));
    }
    let about = About::of(r.invitation, r.occurrence.as_ref(), here)?;
    if about.event.prop("DTSTART").is_some_and(|p| p.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || p.value().trim().len() == 8) {
        return Err(Error::Other("This meeting lasts all day; Jolta proposes new times for meetings with a start and an end time.".into()));
    }
    let utc = |t: DateTime<Utc>| format!("{}Z", t.format("%Y%m%dT%H%M%S"));
    let mut counter = echo(&about.event, &["UID", "RECURRENCE-ID", "SEQUENCE", "SUMMARY", "LOCATION"]);
    counter.add(Prop::new("DTSTART", &utc(proposal.start)));
    counter.add(Prop::new("DTEND", &utc(proposal.end)));
    counter.add(about.organizer.clone());
    let attendee = about.my_line(r.me, r.from, r.answer);
    let who = cn(&attendee).unwrap_or_else(|| attendee.address().unwrap_or(r.from).to_string());
    counter.add(attendee);
    let note = r.note.filter(|n| !n.trim().is_empty());
    if let Some(n) = note {
        counter.add(Prop::text_value("COMMENT", n));
    }

    let mut text = format!("{who} proposes a new time: {}.", spoken(proposal, here));
    if let Some(n) = note {
        text.push_str(&format!("\n\n{n}"));
    }
    let title = about.title();
    let value = |name: &str| about.event.prop(name).map(|p| p.value().trim().to_string()).unwrap_or_default();
    let same_as = [r.from, &value("UID"), &value("RECURRENCE-ID"), &value("SEQUENCE"), &utc(proposal.start), &utc(proposal.end), r.answer.partstat(), note.unwrap_or("")].join("\n");
    imip_mail(Mail {
        from: r.from,
        to: (cn(&about.organizer), about.organizer_address.clone()),
        subject: if title.is_empty() { "New time proposed".into() } else { format!("New time proposed: {title}") },
        text,
        method: "COUNTER",
        ics: itip("COUNTER", counter, &about.zones),
        same_as: Some(format!("COUNTER\n{same_as}")),
    })
}

/// "Fri 13 Nov 2026, 10:00 to 11:00 (Europe/Brussels)" on the Mac's clock.
fn spoken(p: &Proposal, here: Tz) -> String {
    let (start, end) = (p.start.with_timezone(&here), p.end.with_timezone(&here));
    let end = if end.date_naive() == start.date_naive() { end.format("%H:%M").to_string() } else { end.format("%a %-d %b %Y %H:%M").to_string() };
    format!("{}, {} to {end} ({here})", start.format("%a %-d %b %Y"), start.format("%H:%M"))
}

/// The organizer's no to a proposal (RFC 5546 section 3.2.8): a
/// METHOD:DECLINECOUNTER object echoing the UID, SEQUENCE and
/// RECURRENCE-ID of the COUNTER's event `uid` at `rid`, with its
/// ORGANIZER, else `from` as Exchange's COUNTER names none, and the
/// proposer's ATTENDEE line, from the organizer's account `from` to the
/// proposer only. The meeting keeps its time; the
/// proposer's calendar keeps its copy. `title` is the meeting's, which
/// Exchange's COUNTER gives as "New Time Proposed: Planning".
pub fn imip_decline_counter(counter: &str, uid: &str, rid: Option<&Moment>, from: &str, proposer: &str, title: &str, here: Tz) -> Result<Built> {
    let obj = Object::parse(counter);
    let calendar = obj.calendar().ok_or_else(|| Error::Other("the proposal holds no calendar".into()))?;
    if !calendar.prop("METHOD").is_some_and(|m| m.value().trim().eq_ignore_ascii_case("COUNTER")) {
        return Err(Error::Other("this calendar mail isn't a proposal for a new time".into()));
    }
    // The calendar parser decodes escaped UIDs and resolves a custom
    // VTIMEZONE such as Outlook's "Customized Time Zone"; it reads the
    // VEVENTs in order, so its events line up with the components.
    let vevents: Vec<&Component> = calendar.components().filter(|c| c.name() == "VEVENT").collect();
    let parsed = super::ical::read(counter, &[]).ok().filter(|events| events.len() == vevents.len());
    let same_occurrence = |m: Option<Moment>| match (m, rid) {
        (None, None) => true,
        (Some(m), Some(rid)) => m.names(rid, here),
        _ => false,
    };
    let event = vevents
        .iter()
        .enumerate()
        .find(|(i, e)| match &parsed {
            Some(events) => events[*i].uid == uid && same_occurrence(events[*i].recurrence_id),
            None => e.prop("UID").is_some_and(|u| u.text().trim() == uid) && same_occurrence(e.prop("RECURRENCE-ID").and_then(prop_moment)),
        })
        .map(|(_, e)| *e)
        .ok_or_else(|| Error::Other("the proposal holds no event".into()))?;
    let organizer = event.prop("ORGANIZER").cloned().unwrap_or_else(|| Prop::new("ORGANIZER", &format!("mailto:{from}")));
    let mut attendee = event
        .props()
        .find(|p| p.is("ATTENDEE") && is_me(p, &[proposer]))
        .cloned()
        .ok_or_else(|| Error::Other(format!("the proposal doesn't name {proposer} as a guest")))?;
    for stale in ["PARTSTAT", "RSVP", "SCHEDULE-STATUS", "X-RESPONSE-COMMENT"] {
        attendee.remove_param(stale);
    }
    let proposed = |name: &str| event.prop(name).map(|p| p.value().trim().to_string()).unwrap_or_default();
    let mut decline = echo(event, &["UID", "RECURRENCE-ID", "SEQUENCE"]);
    decline.add(organizer);
    let name = cn(&attendee);
    decline.add(attendee);
    let zones: Vec<Component> = calendar.components().filter(|c| c.name() == "VTIMEZONE").cloned().collect();
    let title = title.trim();
    let same_as = ["DECLINECOUNTER", from, &proposed("UID"), &proposed("RECURRENCE-ID"), &proposed("SEQUENCE"), proposer, &proposed("DTSTART"), &proposed("DTEND")].join("\n");
    imip_mail(Mail {
        from,
        to: (name, proposer.to_string()),
        subject: if title.is_empty() { "New time declined".into() } else { format!("New time declined: {title}") },
        text: format!("{from} declined the new time you proposed. The meeting keeps its time."),
        method: "DECLINECOUNTER",
        ics: itip("DECLINECOUNTER", decline, &zones),
        same_as: Some(same_as),
    })
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe;

    use super::*;
    use crate::calendar::ical::tests::{ME, at, fixture, read_events};

    const OUTLOOK_UID: &str = "040000008200E00074C5B7101A82E00800000000A1B2C3D4E5F6D901000000000000000010000000F00DFACEB00C4B6A9E2F5B3D8C7A6E51";

    fn rsvp(answer: Answer, note: Option<&str>, notify: bool) -> Rsvp {
        Rsvp { event: EventId(1), answer, note: note.map(str::to_string), notify, scope: Scope::Series, proposal: None }
    }

    /// Content lines that differ between two texts, unfolded, as (removed, added).
    fn changed(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
        let lines = |t: &str| -> Vec<String> { t.replace("\r\n ", "").split("\r\n").map(str::to_string).collect() };
        let minus = |from: Vec<String>, take: Vec<String>| {
            let mut left = from;
            for l in take {
                if let Some(i) = left.iter().position(|x| *x == l) {
                    left.remove(i);
                }
            }
            left
        };
        (minus(lines(before), lines(after)), minus(lines(after), lines(before)))
    }

    fn mine(events: &[EventData], rid: Option<Moment>) -> (Response, String) {
        let e = events.iter().find(|e| e.recurrence_id == rid).expect("the event");
        let a = e.attendees.iter().find(|a| a.is_me).expect("me on the guest list");
        (a.response, a.comment.clone())
    }

    #[test]
    fn answering_a_series_answers_every_occurrence_and_nothing_else() {
        let text = fixture!("outlook-series.ics");
        let out = answer_in_ical(text, OUTLOOK_UID, None, &[ME], &rsvp(Answer::Accept, None, true), Europe::Brussels).unwrap();
        let (removed, added) = changed(text, &out);
        let mine_before = "ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE;CN=Koen Test:mailto:koen@example.org";
        let mine_after = "ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;RSVP=TRUE;CN=Koen Test:mailto:koen@example.org";
        assert_eq!(removed, [mine_before, mine_before], "the master's line and the moved occurrence's");
        assert_eq!(added, [mine_after, mine_after]);
        let events = read_events(&out);
        let answers: Vec<Option<Response>> = events.iter().map(EventData::my_response).collect();
        assert_eq!(answers, [Some(Response::Accepted), Some(Response::Accepted)], "the master and the moved occurrence");
    }

    #[test]
    fn a_series_answer_is_stored_as_the_server_holds_it() {
        let fixture = fixture!("outlook-series.ics");
        let cut = fixture.rfind("mailto:koen@example.org").unwrap();
        let text = format!("{}mailto:someone@example.org{}", &fixture[..cut], &fixture[cut + "mailto:koen@example.org".len()..]);
        let here = Europe::Brussels;
        let rsvp = rsvp(Answer::Accept, None, true);
        let server = answer_in_ical(&text, OUTLOOK_UID, None, &[ME], &rsvp, here).unwrap();
        let on_server: Vec<Option<Response>> = read_events(&server).iter().map(EventData::my_response).collect();
        assert_eq!(on_server, [Some(Response::Accepted), None], "the moved occurrence doesn't invite the user");

        let dir = std::env::temp_dir().join(format!("jolta-rsvp-series-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = store::Store::open(&dir.join("jolta.sqlite")).unwrap();
        let stored = store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO account (id, kind, address, username, secret_ref, created, calendar)
                     VALUES (1, 'imap', 'koen@example.org', 'koen', 'a1', 0, 1)",
                )?;
                let remote = store::RemoteCalendar {
                    remote_id: "/cal/work/".into(),
                    name: "Work".into(),
                    color: None,
                    zone: None,
                    access: super::super::Access::Owner,
                    primary: true,
                    refresh: None,
                    video: None,
                };
                let calendar = store::sync_calendar_list(tx, crate::AccountId(1), CalendarSource::CalDav, &[remote])?[0].id;
                store::apply_item(tx, calendar, "/cal/work/plan.ics", Some("\"1\""), &read_events(&text), None, &Horizon::around(Utc::now(), here))?;
                let master: EventId = tx.query_row("SELECT id FROM event WHERE recurrence_id = ''", [], |r| r.get(0))?;
                let (_, rows, me) = series_of(tx, master, None)?;
                let target = target(rows, master, &Scope::Series, here, me)?;
                record(tx, calendar, &target, &rsvp, &target.event.remote_id, here)?;
                let mut stmt = tx.prepare("SELECT data FROM event ORDER BY recurrence_id")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.iter().map(|d| serde_json::from_str::<EventData>(d).unwrap().my_response()).collect::<Vec<_>>())
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(stored, on_server);
    }

    /// A sync that couldn't learn the user's aliases stored an invitation to
    /// one as someone else's. The answer, sent once the aliases are known,
    /// stores the written resource read with them, as the next sync would.
    #[test]
    fn an_answer_to_an_alias_is_stored_with_the_aliases_the_answer_knew() {
        let alias = "koen@icloud.example";
        let text = fixture!("outlook-series.ics").replace("mailto:koen@example.org", &format!("mailto:{alias}"));
        let here = Europe::Brussels;
        let stale = super::super::ical::read(&text, &[ME.to_string()]).unwrap();
        assert!(stale.iter().all(|e| e.my_response().is_none()), "the account address alone doesn't find the alias");

        let me = [ME, alias];
        let written = answer_in_ical(&text, OUTLOOK_UID, None, &me, &rsvp(Answer::Accept, None, true), here).unwrap();
        let dir = std::env::temp_dir().join(format!("jolta-rsvp-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = store::Store::open(&dir.join("jolta.sqlite")).unwrap();
        let stored = store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO account (id, kind, address, username, secret_ref, created, calendar)
                     VALUES (1, 'imap', 'koen@example.org', 'koen', 'a1', 0, 1)",
                )?;
                let remote = store::RemoteCalendar {
                    remote_id: "/cal/work/".into(),
                    name: "Work".into(),
                    color: None,
                    zone: None,
                    access: super::super::Access::Owner,
                    primary: true,
                    refresh: None,
                    video: None,
                };
                let calendar = store::sync_calendar_list(tx, crate::AccountId(1), CalendarSource::CalDav, &[remote])?[0].id;
                let horizon = Horizon::around(Utc::now(), here);
                store::apply_item(tx, calendar, "/cal/work/plan.ics", Some("\"1\""), &stale, None, &horizon)?;
                let master: EventId = tx.query_row("SELECT id FROM event WHERE recurrence_id = ''", [], |r| r.get(0))?;
                let (_, rows, account) = series_of(tx, master, None)?;
                let target = target(rows, master, &Scope::Series, here, account)?;
                let events = super::super::ical::read(&written, &me.map(String::from))?;
                // Answering as already answered leaves the server's ETag as it was.
                let written = Written { etag: Some("\"1\"".into()), events, blob: None };
                store_answer(tx, calendar, &target, &rsvp(Answer::Accept, None, true), &target.event.remote_id, Some(written), here)?;
                let mut stmt = tx.prepare("SELECT data FROM event ORDER BY recurrence_id")?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows.iter().map(|d| serde_json::from_str::<EventData>(d).unwrap()).collect::<Vec<_>>())
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let answers: Vec<Option<Response>> = stored.iter().map(EventData::my_response).collect();
        assert_eq!(answers, [Some(Response::Accepted), Some(Response::Accepted)], "the master and the moved occurrence");
        assert!(
            stored.iter().flat_map(|e| &e.attendees).all(|a| !a.person.address.eq_ignore_ascii_case(ME)),
            "no made-up attendee for the account address"
        );
    }

    #[test]
    fn a_note_and_no_reply() {
        let text = fixture!("outlook-series.ics");
        let out = answer_in_ical(text, OUTLOOK_UID, None, &[ME], &rsvp(Answer::Maybe, Some("Wat later,\nsorry"), false), Europe::Brussels).unwrap();
        let (removed, added) = changed(text, &out);
        assert_eq!(removed.len(), 4, "both of my lines and both ORGANIZER lines: {removed:?}");
        assert!(added.contains(&"ORGANIZER;CN=Sofie Janssens;SCHEDULE-AGENT=CLIENT:mailto:sofie.janssens@example.com".to_string()), "{added:?}");
        assert!(added.contains(&"X-CALENDARSERVER-PRIVATE-COMMENT:Wat later\\,\\nsorry".to_string()), "{added:?}");
        assert_eq!(mine(&read_events(&out), None), (Response::Tentative, "Wat later, sorry".into()));

        let again = answer_in_ical(&out, OUTLOOK_UID, None, &[ME], &rsvp(Answer::Accept, None, true), Europe::Brussels).unwrap();
        assert!(!again.contains("SCHEDULE-AGENT") && !again.contains("COMMENT"), "answering again with a reply undoes both");
        assert_eq!(mine(&read_events(&again), None), (Response::Accepted, String::new()));
    }

    #[test]
    fn one_occurrence_gets_its_own_override() {
        let text = fixture!("outlook-series.ics");
        let paris = Zone::Tz(Tz::Europe__Paris);
        let rid = Moment::Time(at("2026-12-01 10:00", paris));
        let out = answer_in_ical(text, OUTLOOK_UID, Some(&rid), &[ME], &rsvp(Answer::Decline, None, true), Europe::Brussels).unwrap();
        let (removed, added) = changed(text, &out);
        assert_eq!(removed, Vec::<String>::new(), "the master and the moved occurrence stay as they were");
        assert!(added.contains(&"RECURRENCE-ID;TZID=Romance Standard Time:20261201T100000".to_string()), "{added:?}");
        assert!(added.contains(&"DTSTART;TZID=Romance Standard Time:20261201T100000".to_string()));
        assert!(added.contains(&"DTEND;TZID=Romance Standard Time:20261201T103000".to_string()));
        assert!(!added.iter().any(|l| l.starts_with("RRULE") || l.starts_with("EXDATE")));

        let events = read_events(&out);
        assert_eq!(mine(&events, None).0, Response::NeedsAction);
        assert_eq!(mine(&events, Some(rid)).0, Response::Declined);
        let over = events.iter().find(|e| e.recurrence_id == Some(rid)).unwrap();
        assert_eq!(over.span, Span::Timed { start: at("2026-12-01 10:00", paris), end: at("2026-12-01 10:30", paris) });

        let rid = Moment::Time(at("2026-11-17 10:00", paris));
        let moved = answer_in_ical(text, OUTLOOK_UID, Some(&rid), &[ME], &rsvp(Answer::Accept, None, true), Europe::Brussels).unwrap();
        let (removed, added) = changed(text, &moved);
        assert_eq!((removed.len(), added.len()), (1, 1), "the stored override is answered, not copied again");
        assert_eq!(mine(&read_events(&moved), Some(rid)).0, Response::Accepted);
    }

    #[test]
    fn no_reply_for_one_occurrence_holds_for_every_organizer_line() {
        let text = fixture!("outlook-series.ics");
        let rid = Moment::Time(at("2026-12-01 10:00", Zone::Tz(Tz::Europe__Paris)));
        let organizers = |t: &str| -> Vec<Option<String>> {
            let obj = Object::parse(t);
            let events: Vec<&Component> = obj.calendar().unwrap().components().filter(|c| c.name() == "VEVENT").collect();
            events.iter().map(|e| e.prop("ORGANIZER").unwrap().param("SCHEDULE-AGENT").map(str::to_string)).collect()
        };
        let quiet = answer_in_ical(text, OUTLOOK_UID, Some(&rid), &[ME], &rsvp(Answer::Decline, None, false), Europe::Brussels).unwrap();
        assert_eq!(organizers(&quiet), [Some("CLIENT".to_string()), Some("CLIENT".to_string()), Some("CLIENT".to_string())], "master, new override, moved one");
        let answers: Vec<Option<Response>> = read_events(&quiet).iter().map(EventData::my_response).collect();
        assert_eq!(answers, [Some(Response::NeedsAction), Some(Response::Declined), Some(Response::NeedsAction)], "only the occurrence is answered");

        let loud = answer_in_ical(&quiet, OUTLOOK_UID, Some(&rid), &[ME], &rsvp(Answer::Accept, None, true), Europe::Brussels).unwrap();
        assert_eq!(organizers(&loud), [None, None, None]);
    }

    #[test]
    fn an_invitation_to_another_of_my_addresses_is_answered() {
        let text = fixture!("outlook-series.ics");
        let me = ["koen@icloud.example", "KOEN@example.org"];
        let out = answer_in_ical(text, OUTLOOK_UID, None, &me, &rsvp(Answer::Accept, None, true), Europe::Brussels).unwrap();
        assert_eq!(mine(&read_events(&out), None).0, Response::Accepted);
    }

    #[test]
    fn someone_not_invited_cannot_answer() {
        let text = fixture!("outlook-series.ics");
        let err = answer_in_ical(text, OUTLOOK_UID, None, &["stranger@example.org"], &rsvp(Answer::Accept, None, true), Europe::Brussels).unwrap_err();
        assert!(err.to_string().contains("isn't on this meeting's guest list"), "{err}");
    }

    #[test]
    fn occurrences_keep_their_length_on_the_series_clock() {
        let brussels = Zone::Tz(Tz::Europe__Brussels);
        let span = Span::Timed { start: at("2026-10-20 09:00", brussels), end: at("2026-10-20 09:30", brussels) };
        let after_the_clock_change = Moment::Time(at("2026-11-03 09:00", brussels));
        assert_eq!(
            series::occurrence_span(&span, &after_the_clock_change, Europe::Brussels),
            Some(Span::Timed { start: at("2026-11-03 09:00", brussels), end: at("2026-11-03 09:30", brussels) })
        );
        let utc = Moment::Time(at("2026-11-03 08:00", Zone::Tz(Tz::UTC)));
        assert_eq!(series::occurrence_span(&span, &utc, Europe::Brussels).map(|s| s.start()), Some(after_the_clock_change), "read on the series' clock");
        let day = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let all_day = Span::AllDay { start: day("2026-11-02"), end: day("2026-11-04") };
        assert_eq!(series::occurrence_span(&all_day, &Moment::Date(day("2026-11-09")), Europe::Brussels), Some(Span::AllDay { start: day("2026-11-09"), end: day("2026-11-11") }));
        assert_eq!(series::occurrence_span(&all_day, &after_the_clock_change, Europe::Brussels), None);
    }

    fn reply_to(invitation: &str, me: &str, answer: Answer, note: Option<&str>, occurrence: Option<Moment>) -> Result<Built> {
        let me = [me.to_string()];
        imip_reply(&ImipReply { invitation, from: &me[0], me: &me, answer, note, occurrence }, Europe::Brussels)
    }

    /// The text/calendar part of a built reply, and the message.
    fn reply_parts(built: &Built) -> (mail_parser::Message<'_>, String) {
        let msg = mail_parser::MessageParser::default().parse(&built.raw).expect("a message");
        let ics = msg
            .parts()
            .find(|p| p.content_type().is_some_and(|c| c.subtype() == Some("calendar")))
            .map(|part| {
                assert_eq!(part.content_type().unwrap().attribute("method"), Some("REPLY"));
                let mut text = String::new();
                assert!(part.text_into(&mut text), "a text part");
                text
            })
            .expect("a text/calendar part");
        (msg, ics)
    }

    #[test]
    fn imip_reply_to_a_google_invitation() {
        let built = reply_to(fixture!("google.ics"), ME, Answer::Maybe, Some("Ik ben 10 minuten later; sorry"), None).unwrap();
        assert_eq!(built.recipients, ["lotte.maes@example.com"]);
        assert_eq!(built.from, ME);
        let (msg, ics) = reply_parts(&built);
        assert_eq!(msg.subject(), Some("Tentative: Design review"));
        assert_eq!(msg.to().and_then(|t| t.first()).and_then(|a| a.address()), Some("lotte.maes@example.com"));
        assert_eq!(msg.message_id(), Some(built.message_id.as_str()));
        assert!(msg.body_text(0).unwrap().contains("koen@example.org has tentatively accepted this invitation."));

        let reply = Object::parse(&ics);
        let cal = reply.calendar().unwrap();
        assert_eq!(cal.prop("METHOD").unwrap().value(), "REPLY");
        let events: Vec<&Component> = cal.components().filter(|c| c.name() == "VEVENT").collect();
        assert_eq!(events.len(), 1);
        let ev = events[0];
        assert_eq!(ev.prop("UID").unwrap().text(), "5f2k9q8v1n3m7c0b4x6z2a8d1e@google.com");
        assert_eq!(ev.prop("SEQUENCE").unwrap().value(), "0");
        assert_eq!(ev.prop("ORGANIZER").unwrap().address(), Some("lotte.maes@example.com"));
        let attendees: Vec<&Prop> = ev.props().filter(|p| p.is("ATTENDEE")).collect();
        assert_eq!(attendees.len(), 1, "only the user's own line");
        assert_eq!(attendees[0].address(), Some(ME));
        assert_eq!(attendees[0].param("PARTSTAT"), Some("TENTATIVE"));
        assert_eq!(attendees[0].param("RSVP"), None);
        assert_eq!(ev.prop("COMMENT").unwrap().text(), "Ik ben 10 minuten later; sorry");
        assert!(ev.prop("DESCRIPTION").is_none() && ev.prop("X-GOOGLE-CONFERENCE").is_none());

        let read = read_events(&ics);
        assert_eq!(read[0].my_response(), Some(Response::Tentative));
        assert_eq!(read[0].span, read_events(fixture!("google.ics"))[0].span);
    }

    #[test]
    fn imip_reply_keeps_the_zone_and_the_series() {
        let built = reply_to(fixture!("outlook-series.ics"), "KOEN@example.org", Answer::Decline, None, None).unwrap();
        let (msg, ics) = reply_parts(&built);
        assert_eq!(msg.subject(), Some("Declined: Weekplanning"));
        let cal = Object::parse(&ics);
        let cal = cal.calendar().unwrap();
        assert_eq!(cal.components().filter(|c| c.name() == "VTIMEZONE").count(), 1, "the TZID DTSTART uses comes along");
        let events: Vec<&Component> = cal.components().filter(|c| c.name() == "VEVENT").collect();
        assert_eq!(events.len(), 1, "the reply is for the series, not the moved occurrence");
        assert!(events[0].prop("RECURRENCE-ID").is_none());
        assert!(events[0].prop("COMMENT").is_none());
        let paris = Zone::Tz(Tz::Europe__Paris);
        let read = read_events(&ics);
        assert_eq!(read[0].span, Span::Timed { start: at("2026-11-10 10:00", paris), end: at("2026-11-10 10:30", paris) });
        assert_eq!(read[0].my_response(), Some(Response::Declined));
    }

    #[test]
    fn imip_reply_for_one_occurrence() {
        let paris = Zone::Tz(Tz::Europe__Paris);
        let unchanged = Moment::Time(at("2026-12-01 10:00", paris));
        let (_, ics) = reply_parts(&reply_to(fixture!("outlook-series.ics"), ME, Answer::Decline, None, Some(unchanged)).unwrap());
        let read = read_events(&ics);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].recurrence_id, Some(unchanged));
        assert_eq!(read[0].span, Span::Timed { start: at("2026-12-01 10:00", paris), end: at("2026-12-01 10:30", paris) });
        assert!(read[0].recurrence.is_none(), "no repeat rule in a reply for one occurrence");
        assert!(ics.contains("RECURRENCE-ID;TZID=Romance Standard Time:20261201T100000"), "{ics}");

        let moved = Moment::Time(at("2026-11-17 10:00", paris));
        let (_, ics) = reply_parts(&reply_to(fixture!("outlook-series.ics"), ME, Answer::Accept, None, Some(moved)).unwrap());
        assert_eq!(read_events(&ics)[0].recurrence_id, Some(moved), "the changed occurrence's own event");

        let single = reply_to(fixture!("google.ics"), ME, Answer::Accept, None, Some(unchanged)).err().unwrap();
        assert!(single.to_string().contains("doesn't repeat"), "{single}");
    }

    #[test]
    fn imip_reply_from_the_account_for_an_alias() {
        let me = ["account@example.net".to_string(), ME.to_string()];
        let r = ImipReply { invitation: fixture!("google.ics"), from: &me[0], me: &me, answer: Answer::Accept, note: None, occurrence: None };
        let built = imip_reply(&r, Europe::Brussels).unwrap();
        let (msg, ics) = reply_parts(&built);
        assert_eq!(msg.from().and_then(|f| f.first()).and_then(|a| a.address()), Some("account@example.net"));
        assert_eq!(built.recipients, ["lotte.maes@example.com"], "the organizer only");
        assert_eq!(read_events(&ics)[0].attendees.iter().map(|a| a.person.address.as_str()).collect::<Vec<_>>(), [ME], "the address invited");
    }

    #[test]
    fn imip_reply_refuses_what_is_not_an_invitation() {
        let cancel = fixture!("google.ics").replace("METHOD:REQUEST", "METHOD:CANCEL");
        assert!(reply_to(&cancel, ME, Answer::Accept, None, None).err().unwrap().to_string().contains("CANCEL"));
        let built = reply_to(fixture!("google.ics"), "list-member@example.org", Answer::Accept, None, None).unwrap();
        let (_, ics) = reply_parts(&built);
        assert!(ics.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:list-member@example.org"), "invited through a list: {ics}");
    }

    /// The calendar part of an iMIP mail under `method`, and the message.
    fn itip_parts<'a>(built: &'a Built, method: &str) -> (mail_parser::Message<'a>, String) {
        let msg = mail_parser::MessageParser::default().parse(&built.raw).expect("a message");
        let part = msg.parts().find(|p| p.content_type().is_some_and(|c| c.subtype() == Some("calendar"))).expect("a text/calendar part");
        assert_eq!(part.content_type().unwrap().attribute("method"), Some(method));
        let ics = String::from_utf8_lossy(&part.decoded()).into_owned();
        (msg, ics)
    }

    fn propose(invitation: &str, answer: Answer, note: Option<&str>, occurrence: Option<Moment>, proposal: &Proposal) -> Result<Built> {
        let me = [ME.to_string()];
        imip_counter(&ImipReply { invitation, from: ME, me: &me, answer, note, occurrence }, proposal, Europe::Brussels)
    }

    fn utc(s: &str) -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc()
    }

    #[test]
    fn a_counter_proposes_the_new_time_to_the_organizer_only() {
        let proposal = Proposal { start: utc("2026-11-06 13:00"), end: utc("2026-11-06 13:45") };
        let built = propose(fixture!("google.ics"), Answer::Maybe, Some("Thursday is full; Friday?"), None, &proposal).unwrap();
        assert_eq!((built.from.as_str(), built.recipients.as_slice()), (ME, ["lotte.maes@example.com".to_string()].as_slice()));
        let (msg, ics) = itip_parts(&built, "COUNTER");
        assert_eq!(msg.subject(), Some("New time proposed: Design review"));
        assert!(msg.body_text(0).unwrap().contains("proposes a new time: Fri 6 Nov 2026, 14:00 to 14:45 (Europe/Brussels)."), "{:?}", msg.body_text(0));

        let obj = Object::parse(&ics);
        let cal = obj.calendar().unwrap();
        assert_eq!(cal.prop("METHOD").unwrap().value(), "COUNTER");
        let ev = cal.components().find(|c| c.name() == "VEVENT").unwrap();
        assert_eq!(ev.prop("UID").unwrap().text(), "5f2k9q8v1n3m7c0b4x6z2a8d1e@google.com");
        assert_eq!(ev.prop("SEQUENCE").unwrap().value(), "0", "the version it counters");
        assert_eq!((ev.prop("DTSTART").unwrap().value(), ev.prop("DTEND").unwrap().value()), ("20261106T130000Z", "20261106T134500Z"));
        assert_eq!(ev.prop("ORGANIZER").unwrap().address(), Some("lotte.maes@example.com"));
        let attendees: Vec<&Prop> = ev.props().filter(|p| p.is("ATTENDEE")).collect();
        assert_eq!(attendees.len(), 1, "only the user, who proposes: {ics}");
        assert_eq!((attendees[0].address(), attendees[0].param("PARTSTAT"), attendees[0].param("RSVP")), (Some(ME), Some("TENTATIVE"), None));
        assert_eq!(ev.prop("COMMENT").unwrap().text(), "Thursday is full; Friday?");
        assert!(ev.prop("RRULE").is_none() && ev.prop("RECURRENCE-ID").is_none() && ev.prop("DESCRIPTION").is_none());

        let read = super::super::ical::read_invitation(&ics, &[ME.to_string()]).unwrap();
        assert_eq!(read[0].event.span.bounds(Europe::Brussels), Bounds::Utc { start: proposal.start, end: proposal.end }, "the card reads the proposed time");
    }

    #[test]
    fn the_same_proposal_is_the_same_mail() {
        let proposal = Proposal { start: utc("2026-11-06 13:00"), end: utc("2026-11-06 13:45") };
        let id = |answer, note, p: &Proposal| propose(fixture!("google.ics"), answer, note, None, p).unwrap().message_id;
        let first = id(Answer::Maybe, None, &proposal);
        assert_eq!(first, id(Answer::Maybe, None, &proposal), "built twice, one Message-ID");
        let later = Proposal { start: utc("2026-11-06 14:00"), end: utc("2026-11-06 14:45") };
        for other in [id(Answer::Decline, None, &proposal), id(Answer::Maybe, Some("Or later"), &proposal), id(Answer::Maybe, None, &later)] {
            assert_ne!(first, other, "a changed answer, note or time is a new proposal");
        }
    }

    #[test]
    fn a_counter_for_one_occurrence_names_it() {
        let paris = Zone::Tz(Tz::Europe__Paris);
        let unchanged = Moment::Time(at("2026-12-01 10:00", paris));
        let proposal = Proposal { start: utc("2026-12-01 14:00"), end: utc("2026-12-01 14:30") };
        let built = propose(fixture!("outlook-series.ics"), Answer::Decline, None, Some(unchanged), &proposal).unwrap();
        let (_, ics) = itip_parts(&built, "COUNTER");
        assert!(ics.contains("RECURRENCE-ID;TZID=Romance Standard Time:20261201T100000"), "{ics}");
        assert!(ics.contains("BEGIN:VTIMEZONE"), "the recurrence id's zone comes along");
        let event = &ics[ics.find("BEGIN:VEVENT").unwrap()..];
        assert!(!event.contains("RRULE") && !event.contains("EXDATE"), "{ics}");
        assert!(ics.contains("PARTSTAT=DECLINED"));
        assert!(ics.contains("DTSTART:20261201T140000Z"));

        let moved = Moment::Time(at("2026-11-17 10:00", paris));
        let (_, ics) = itip_parts(&propose(fixture!("outlook-series.ics"), Answer::Maybe, None, Some(moved), &proposal).unwrap(), "COUNTER");
        assert!(ics.contains("RECURRENCE-ID;TZID=Romance Standard Time:20261117T100000"), "the changed occurrence's own: {ics}");
    }

    #[test]
    fn a_counter_needs_maybe_or_decline_and_a_timed_meeting() {
        let proposal = Proposal { start: utc("2026-11-06 13:00"), end: utc("2026-11-06 13:45") };
        let err = |r: Result<Built>| r.err().expect("refused").to_string();
        assert!(err(propose(fixture!("google.ics"), Answer::Accept, None, None, &proposal)).contains("not with Accept"));
        let backwards = Proposal { start: proposal.end, end: proposal.start };
        assert!(err(propose(fixture!("google.ics"), Answer::Maybe, None, None, &backwards)).contains("ends before it starts"));
        let all_day = fixture!("google.ics").replace("DTSTART:20261105T090000Z", "DTSTART;VALUE=DATE:20261105").replace("DTEND:20261105T094500Z", "DTEND;VALUE=DATE:20261106");
        assert!(err(propose(&all_day, Answer::Maybe, None, None, &proposal)).contains("all day"));
        let cancel = fixture!("google.ics").replace("METHOD:REQUEST", "METHOD:CANCEL");
        assert!(err(propose(&cancel, Answer::Maybe, None, None, &proposal)).contains("CANCEL"));
    }

    /// Outlook writes a custom VTIMEZONE as "Customized Time Zone", and a
    /// UID may carry an escaped comma; the no still finds the proposal.
    #[test]
    fn a_declinecounter_finds_a_proposal_in_a_custom_zone_with_an_escaped_uid() {
        let counter = format!(
            "BEGIN:VCALENDAR\r\nMETHOD:COUNTER\r\nBEGIN:VTIMEZONE\r\nTZID:Customized Time Zone\r\n\
             BEGIN:STANDARD\r\nDTSTART:16010101T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\n\
             BEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=3\r\nEND:DAYLIGHT\r\n\
             END:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:plan\\,slot\r\nRECURRENCE-ID;TZID=Customized Time Zone:20261201T100000\r\n\
             DTSTART;TZID=Customized Time Zone:20261201T150000\r\nDTEND;TZID=Customized Time Zone:20261201T153000\r\n\
             DTSTAMP:20261105T090000Z\r\nSEQUENCE:0\r\nORGANIZER:mailto:sofie.janssens@example.com\r\n\
             ATTENDEE;PARTSTAT=TENTATIVE:mailto:{ME}\r\nSUMMARY:Weekplanning\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let rid = Moment::Time(at("2026-12-01 10:00", Zone::Tz(Tz::Europe__Brussels)));
        let built = imip_decline_counter(&counter, "plan,slot", Some(&rid), "sofie.janssens@example.com", ME, "Weekplanning", Tz::Europe__Brussels).unwrap();
        assert_eq!(built.recipients, [ME.to_string()]);
        let (_, ics) = itip_parts(&built, "DECLINECOUNTER");
        assert!(ics.contains("UID:plan\\,slot") && ics.contains("RECURRENCE-ID;TZID=Customized Time Zone:20261201T100000"), "{ics}");
    }

    #[test]
    fn a_declinecounter_goes_back_to_the_proposer() {
        let paris = Zone::Tz(Tz::Europe__Paris);
        let proposal = Proposal { start: utc("2026-12-01 14:00"), end: utc("2026-12-01 14:30") };
        let counter = propose(fixture!("outlook-series.ics"), Answer::Maybe, None, Some(Moment::Time(at("2026-12-01 10:00", paris))), &proposal).unwrap();
        let (_, counter) = itip_parts(&counter, "COUNTER");
        let organizer = "sofie.janssens@example.com";
        let rid = Moment::Time(at("2026-12-01 10:00", paris));
        let decline = |counter: &str, proposer: &str| imip_decline_counter(counter, OUTLOOK_UID, Some(&rid), organizer, proposer, "Weekplanning", Tz::Europe__Brussels);

        let built = decline(&counter, ME).unwrap();
        assert_eq!((built.from.as_str(), built.recipients.as_slice()), (organizer, [ME.to_string()].as_slice()), "from the organizer to the proposer only");
        assert_eq!(built.message_id, decline(&counter, ME).unwrap().message_id, "the same no twice is one mail");
        let (msg, ics) = itip_parts(&built, "DECLINECOUNTER");
        assert_eq!(msg.subject(), Some("New time declined: Weekplanning"));
        let obj = Object::parse(&ics);
        let ev = obj.calendar().unwrap().components().find(|c| c.name() == "VEVENT").unwrap();
        assert_eq!(ev.prop("UID").unwrap().text(), OUTLOOK_UID);
        assert!(ics.contains("RECURRENCE-ID;TZID=Romance Standard Time:20261201T100000") && ics.contains("BEGIN:VTIMEZONE"), "{ics}");
        assert_eq!(ev.prop("ORGANIZER").unwrap().address(), Some(organizer));
        let attendees: Vec<&Prop> = ev.props().filter(|p| p.is("ATTENDEE")).collect();
        assert_eq!((attendees.len(), attendees[0].address(), attendees[0].param("PARTSTAT")), (1, Some(ME), None), "{ics}");
        assert!(ev.prop("DTSTART").is_none(), "the meeting keeps its time, which the proposer already has");

        let exchange: String = counter.lines().filter(|l| !l.starts_with("ORGANIZER")).map(|l| format!("{l}\r\n")).collect();
        let (_, ics) = itip_parts(&decline(&exchange, ME).unwrap(), "DECLINECOUNTER");
        assert!(ics.contains("ORGANIZER:mailto:sofie.janssens@example.com"), "Exchange's COUNTER names no organizer: {ics}");

        let err = |r: Result<Built>| r.err().expect("refused").to_string();
        assert!(err(decline(fixture!("google.ics"), ME)).contains("isn't a proposal"));
        assert!(err(decline(&counter, "someone@example.org")).contains("doesn't name"));
    }
}
