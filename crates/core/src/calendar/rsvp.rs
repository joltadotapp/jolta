//! Answering invitations: accept, maybe or decline.
//!
//! An event in a synced calendar is answered through that calendar's
//! provider, which sends the organizer one reply: Graph's `accept`,
//! `tentativelyAccept` and `decline`; on Google a patch of the user's own
//! attendee entry; on CalDAV the user's PARTSTAT in the stored object, after
//! which the server's scheduling sends the reply. Answers go out at once and
//! fail when offline, until the calendar op queue exists.
//!
//! `imip_reply` writes the reply mail (RFC 6047) Jolta sends itself: for an
//! invitation no synced calendar holds, and on iCloud, which doesn't tell an
//! outside organizer. `respond` decides which an answer needs.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{NaiveDate, NaiveDateTime, Utc};
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
use super::{Attendee, Bounds, Calendar, CalendarSource, EventData, EventId, EventStatus, Local, Moment, Person, Response, Role, Span, Zone, sync, tz};
use crate::compose::Built;
use crate::{Error, Jolta, Result, store};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    let (calendar, target) = target_of(jolta, rsvp.event, &rsvp.scope).await?;
    match calendar.source {
        CalendarSource::Local => return Err(Error::Other("This event is in a calendar on this Mac only. Answering it by mail isn't built yet.".into())),
        CalendarSource::EventKit => {
            return Err(Error::Other("Answer this invitation in the Calendar app: macOS doesn't let other apps answer for its calendars.".into()));
        }
        _ => {}
    }
    let organizer = target.event.data.organizer.as_ref().map(|o| o.address.as_str());
    if organizer.is_some_and(|o| o.eq_ignore_ascii_case(&target.me)) {
        return Err(Error::Other("You organize this meeting, so there is no invitation to answer.".into()));
    }
    if organizer.is_none() && target.event.data.attendees.is_empty() {
        return Err(Error::Other("This event has no organizer or guests, so there is no invitation to answer.".into()));
    }

    let source = sync::calendar_source(jolta, &calendar).await?;
    let (landed, written) = source.answer(&calendar, &target, rsvp).await?;

    let (rsvp, calendar_id) = (rsvp.clone(), calendar.id);
    let written = match written {
        Some(Change::Upsert { etag, events, raw, .. }) => Some(Written { etag, events, blob: raw.map(|r| jolta.blobs.put(&r)).transpose()? }),
        _ => None,
    };
    jolta.store.write_async(move |tx| store_answer(tx, calendar_id, &target, &rsvp, &landed, written, here)).await
}

/// The calendar of `event`, and what a change in `scope` goes to, with the
/// objects as the server last gave them.
pub(crate) async fn target_of(jolta: &Jolta, event: EventId, scope: &Scope) -> Result<(Calendar, Target)> {
    let here = tz::here();
    let (calendar, rows, me) = jolta.store.read_async(move |c| series_of(c, event)).await?;
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
            store::apply_item(conn, calendar, landed, w.etag.as_deref(), &w.events, w.blob.as_deref(), &Horizon::around(Utc::now(), here)).map(|_| ())
        }
        None => record(conn, calendar, target, rsvp, landed, here),
    }
}

/// One stored row of a series.
struct Row {
    stored: Stored,
    is_master: bool,
}

/// The calendar of `event`, every stored event of its series, and the
/// user's address on the calendar's account.
fn series_of(conn: &Connection, event: EventId) -> Result<(Calendar, Vec<Row>, String)> {
    let (calendar_id, uid): (super::CalendarId, String) = conn
        .query_row("SELECT calendar_id, uid FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("event {event}")))?;
    let calendar = store::calendar(conn, calendar_id)?;
    let me = match calendar.account_id {
        Some(account) => store::account(conn, account)?.address,
        None => String::new(),
    };
    let mut stmt = conn.prepare(
        "SELECT id, remote_id, etag, recurrence_id = '', data FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![calendar_id, uid], |r| {
            Ok((r.get::<_, EventId>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, bool>(3)?, r.get::<_, String>(4)?))
        })?
        .map(|row| {
            let (id, remote_id, etag, is_master, data) = row?;
            let data: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}")))?;
            Ok(Row { stored: Stored { id, remote_id, etag, raw: None, data }, is_master })
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
    store::rebuild_series(conn, calendar, uid, &Horizon::around(Utc::now(), here))
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
    let (_, rows, _) = series_of(conn, event)?;
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

/// Who an iMIP reply comes from, and what it answers.
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

/// The iMIP reply (RFC 6047) to an invitation: a METHOD:REPLY object (RFC
/// 5546 section 3.2.3) holding only the user's own ATTENDEE line with its
/// PARTSTAT, and the invitation's UID, SEQUENCE and RECURRENCE-ID, so the
/// organizer's calendar finds the meeting. A note goes in COMMENT. It goes
/// to the organizer only, through the account's outbox like any other mail.
pub fn imip_reply(r: &ImipReply, here: Tz) -> Result<Built> {
    let invite = Object::parse(r.invitation);
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
    let made;
    let event: &Component = match &r.occurrence {
        None => master,
        Some(rid) => match events.iter().find(|e| e.prop("RECURRENCE-ID").and_then(prop_moment).is_some_and(|m| m.names(rid, here))) {
            Some(changed) => changed,
            None if master.prop("RRULE").is_some() || master.prop("RDATE").is_some() => {
                made = occurrence_of(master, rid, here)?.0;
                &made
            }
            None => return Err(Error::Other("This invitation doesn't repeat, so it has no single occurrence to answer.".into())),
        },
    };
    let organizer = event.prop("ORGANIZER").or(master.prop("ORGANIZER")).ok_or_else(|| Error::Other("the invitation names no organizer".into()))?;
    let organizer_address = organizer.address().or(organizer.param("EMAIL")).map(str::to_string)
        .ok_or_else(|| Error::Other("the invitation's organizer has no address".into()))?;
    let me = r.from;

    let mut reply = Component::new("VEVENT");
    for name in ["UID", "RECURRENCE-ID", "SEQUENCE", "DTSTART", "DTEND", "DURATION", "SUMMARY"] {
        if let Some(p) = event.prop(name) {
            reply.add(p.clone());
        }
    }
    if reply.prop("SEQUENCE").is_none() {
        reply.add(Prop::new("SEQUENCE", "0"));
    }
    reply.add(Prop::new("DTSTAMP", &format!("{}Z", Utc::now().format("%Y%m%dT%H%M%S"))));
    reply.add(organizer.clone());
    let mine = |e: &Component| e.props().find(|p| p.is("ATTENDEE") && is_me(p, r.me)).cloned();
    let mut attendee = mine(event).or_else(|| mine(master)).unwrap_or_else(|| Prop::new("ATTENDEE", &format!("mailto:{me}")));
    attendee.set_param("PARTSTAT", r.answer.partstat());
    for stale in ["RSVP", "SCHEDULE-STATUS", "X-RESPONSE-COMMENT"] {
        attendee.remove_param(stale);
    }
    let who = attendee.address().unwrap_or(me).to_string();
    reply.add(attendee);
    let note = r.note.filter(|n| !n.trim().is_empty());
    if let Some(n) = note {
        reply.add(Prop::text_value("COMMENT", n));
    }

    let zones: Vec<&str> = ["DTSTART", "DTEND", "RECURRENCE-ID"].iter().filter_map(|n| event.prop(n)).filter_map(|p| p.param("TZID")).collect();
    let mut cal = Component::new("VCALENDAR");
    cal.add(Prop::text_value("PRODID", concat!("-//Jolta//Jolta ", env!("CARGO_PKG_VERSION"), "//EN")));
    cal.add(Prop::new("VERSION", "2.0"));
    cal.add(Prop::new("CALSCALE", "GREGORIAN"));
    cal.add(Prop::new("METHOD", "REPLY"));
    for vtimezone in calendar.components().filter(|c| c.name() == "VTIMEZONE") {
        if vtimezone.prop("TZID").is_some_and(|t| zones.contains(&t.value())) {
            cal.push(vtimezone.clone());
        }
    }
    cal.push(reply);
    let ics = Object::new(cal).to_string();

    let title = event.prop("SUMMARY").map(Prop::text).unwrap_or_default();
    let (prefix, verb) = match r.answer {
        Answer::Accept => ("Accepted", "accepted"),
        Answer::Maybe => ("Tentative", "tentatively accepted"),
        Answer::Decline => ("Declined", "declined"),
    };
    let mut text = format!("{who} has {verb} this invitation.");
    if let Some(n) = note {
        text.push_str(&format!("\n\n{n}"));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let domain = me.rsplit('@').next().unwrap_or("jolta.invalid");
    let message_id = format!("{}@{domain}", &hex::encode(Sha256::digest(format!("{now}{ics}")))[..24]);
    let organizer_name = organizer.param("CN").map(str::to_string).filter(|n| !n.is_empty());
    let calendar_part = ContentType::new("text/calendar").attribute("method", "REPLY").attribute("charset", "utf-8");
    let raw = MessageBuilder::new()
        .from(BuilderAddress::new_address(None::<String>, me.to_string()))
        .to(BuilderAddress::new_address(organizer_name, organizer_address.clone()))
        .subject(if title.is_empty() { prefix.to_string() } else { format!("{prefix}: {title}") })
        .message_id(message_id.clone())
        .body(MimePart::new("multipart/alternative", vec![MimePart::new("text/plain", text), MimePart::new(calendar_part, ics)]))
        .write_to_vec()?;
    Ok(Built { raw, message_id, from: me.to_string(), recipients: vec![organizer_address] })
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe;

    use super::*;
    use crate::calendar::ical::tests::{ME, at, fixture, read_events};

    const OUTLOOK_UID: &str = "040000008200E00074C5B7101A82E00800000000A1B2C3D4E5F6D901000000000000000010000000F00DFACEB00C4B6A9E2F5B3D8C7A6E51";

    fn rsvp(answer: Answer, note: Option<&str>, notify: bool) -> Rsvp {
        Rsvp { event: EventId(1), answer, note: note.map(str::to_string), notify, scope: Scope::Series }
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
                };
                let calendar = store::sync_calendar_list(tx, crate::AccountId(1), CalendarSource::CalDav, &[remote])?[0].id;
                store::apply_item(tx, calendar, "/cal/work/plan.ics", Some("\"1\""), &read_events(&text), None, &Horizon::around(Utc::now(), here))?;
                let master: EventId = tx.query_row("SELECT id FROM event WHERE recurrence_id = ''", [], |r| r.get(0))?;
                let (_, rows, me) = series_of(tx, master)?;
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
                };
                let calendar = store::sync_calendar_list(tx, crate::AccountId(1), CalendarSource::CalDav, &[remote])?[0].id;
                let horizon = Horizon::around(Utc::now(), here);
                store::apply_item(tx, calendar, "/cal/work/plan.ics", Some("\"1\""), &stale, None, &horizon)?;
                let master: EventId = tx.query_row("SELECT id FROM event WHERE recurrence_id = ''", [], |r| r.get(0))?;
                let (_, rows, account) = series_of(tx, master)?;
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
}
