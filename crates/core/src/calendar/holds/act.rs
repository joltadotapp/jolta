//! Holds (C4 U10): a pending set while the mail is a draft, its events made
//! when the mail leaves the outbox, released as `decide` says, and a
//! release taken back.
//!
//! Inserting times with Hold these times makes a pending set tied to the
//! draft and nothing else. The outbox's send transaction calls `sent`,
//! which keeps the slots whose line is still in the sent body and queues
//! one tentative, private event per slot without guests or a reminder,
//! carrying Jolta's marker. `check` runs after every calendar round and
//! new mail: it applies what `decide` says, deleting released holds
//! through the op queue after the undo window, and tells the user when a
//! booking released the others. `undo` takes such a release back: a delete
//! still waiting is dropped, and a hold already deleted is made again.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::{Booking, Decision, HeldSlot, HoldSet, HoldSetId, Mark, Notice as Said, Outcome, Release, SlotState, decide, seen};
use crate::blobs::Blobs;
use crate::calendar::draft::{EventPrefill, default_calendar};
use crate::calendar::ops::{self, CalendarOp, CalendarOpId, Change, Edit, Extra, OpState, UNDO_SEND_SECONDS, Write};
use crate::calendar::series::Horizon;
use crate::calendar::{Calendar, CalendarSource, EventData, EventStatus, Local, Person, ShowAs, Span, Visibility, Zone, availability, invite};
use crate::model::ThreadId;
use crate::store::OutboxItem;
use crate::{AccountId, Error, Result, compose, store};

/// The most times one mail holds (C4 spec, decision 3).
pub const MAX_SLOTS: usize = 5;

/// Hold events are titled "Hold: <subject>".
pub const TITLE: &str = "Hold: ";

/// The calendar a set's events go to: the sending account's own calendar
/// for new events when its calendar is on, else the calendar new events go
/// to. Err says why there is none.
pub fn calendar_for(conn: &Connection, account: AccountId) -> Result<std::result::Result<Calendar, String>> {
    if let Some(c) = invite::account_calendar(conn, account)? {
        return Ok(Ok(c));
    }
    Ok(default_calendar(conn)?.ok_or_else(|| "No calendar can take holds. Turn on calendar for this account, or make a calendar on this Mac.".to_string()))
}

/// What Insert my availability puts in the message for picked times: the
/// heading and one line per time, on `theirs`' clock, and each time's line.
pub fn text(picked: &[(DateTime<Utc>, DateTime<Utc>)], theirs: Tz, mine: Tz) -> (String, Vec<String>) {
    let heading = availability::availability_text(picked, theirs, mine).lines().next().unwrap_or_default().to_string();
    let lines: Vec<String> = picked
        .iter()
        .map(|slot| availability::availability_text(std::slice::from_ref(slot), theirs, mine).lines().nth(1).unwrap_or_default().to_string())
        .collect();
    (std::iter::once(heading).chain(lines.iter().cloned()).collect::<Vec<_>>().join("\n"), lines)
}

/// Makes the draft's pending set for these times, each with the line of
/// the message that offers it, in place of one the draft had. Nothing
/// reaches a calendar until the mail leaves.
pub fn pending(conn: &Connection, draft: i64, account: AccountId, picked: &[(DateTime<Utc>, DateTime<Utc>, String)], now: DateTime<Utc>) -> Result<HoldSetId> {
    if picked.is_empty() {
        return Err(Error::Other("Pick the times to hold first.".into()));
    }
    if picked.len() > MAX_SLOTS {
        return Err(Error::Other(format!("Jolta holds up to {MAX_SLOTS} times for one message.")));
    }
    let calendar = calendar_for(conn, account)?.map_err(Error::Other)?;
    discarded(conn, draft)?;
    let mut slots: Vec<HeldSlot> = Vec::new();
    for (start, end, line) in picked {
        if end > start && !slots.iter().any(|s| s.start == *start) {
            slots.push(HeldSlot { start: *start, end: *end, line: line.clone(), state: SlotState::Pending, remote_id: None });
        }
    }
    slots.sort_by_key(|s| s.start);
    let set = HoldSet {
        id: HoldSetId::new()?,
        account,
        message_id: None,
        draft: Some(draft),
        recipients: Vec::new(),
        subject: String::new(),
        calendar: calendar.id,
        sent: None,
        kept: false,
        slots,
    };
    store::insert_hold_set(conn, &set, now)?;
    Ok(set.id)
}

/// The draft was discarded: its pending set goes.
pub fn discarded(conn: &Connection, draft: i64) -> Result<()> {
    if let Some(set) = store::pending_hold_set(conn, draft)? {
        store::delete_hold_set(conn, &set.id)?;
    }
    Ok(())
}

/// How many times a mail that just left holds now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    pub set: HoldSetId,
    pub held: usize,
}

/// The mail `item` left the outbox: called in the transaction that records
/// it, so its holds are queued once. Its draft's pending set keeps the
/// slots whose line is still in the sent body and that haven't started,
/// takes the recipients and subject from the mail, and queues one hold
/// event per slot. A set with nothing left to hold, or no calendar to hold
/// in, goes.
pub fn sent(conn: &Connection, item: &OutboxItem, raw: &[u8], now: DateTime<Utc>, horizon: &Horizon) -> Result<Option<Made>> {
    let Some(draft) = item.draft else { return Ok(None) };
    let Some(mut set) = store::pending_hold_set(conn, draft)? else { return Ok(None) };
    let calendar = match calendar_for(conn, item.account)? {
        Ok(c) => c,
        Err(_) => return store::delete_hold_set(conn, &set.id).map(|_| None),
    };
    let message = mail_parser::MessageParser::new().parse(raw);
    let body = squeeze(&message.as_ref().and_then(|m| m.body_text(0)).unwrap_or_default());
    set.slots.retain(|s| !squeeze(&s.line).is_empty() && body.contains(&squeeze(&s.line)) && s.start > now);
    if set.slots.is_empty() {
        store::delete_hold_set(conn, &set.id)?;
        return Ok(None);
    }
    let me = store::account(conn, item.account)?.address;
    let mut recipients: Vec<String> = Vec::new();
    for r in item.recipients.iter().map(|r| r.trim()) {
        if !r.is_empty() && !r.eq_ignore_ascii_case(&me) && !recipients.iter().any(|x| x.eq_ignore_ascii_case(r)) {
            recipients.push(r.to_string());
        }
    }
    set.account = item.account;
    set.message_id = Some(item.message_id.clone());
    set.draft = None;
    set.recipients = recipients;
    set.subject = message.as_ref().and_then(|m| m.subject().map(str::to_string)).unwrap_or_default();
    set.calendar = calendar.id;
    set.sent = Some(now);
    let mark = Mark::Hold { set: set.id.clone(), message: set.message_id.clone() };
    for i in 0..set.slots.len() {
        let key = hold(conn, &calendar, &set, &mark, &set.slots[i], now, horizon)?;
        set.slots[i].remote_id = Some(key);
        set.slots[i].state = SlotState::Held;
    }
    store::save_sent_hold_set(conn, &set)?;
    Ok(Some(Made { held: set.slots.len(), set: set.id }))
}

/// Whitespace runs as one space, so a line the editor wrapped or the
/// encoder folded still matches.
fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Queues the hold event for `slot`. Returns the key it is filed under
/// until its create lands (`landed`).
fn hold(conn: &Connection, calendar: &Calendar, set: &HoldSet, mark: &Mark, slot: &HeldSlot, now: DateTime<Utc>, horizon: &Horizon) -> Result<String> {
    let (event, extra) = event(calendar.source, set, mark, slot, ops::new_uid()?, horizon.here);
    let edit = Edit { extra, ..Edit::quiet(calendar.id, Change::Create(vec![event])) };
    let op = ops::queue(conn, &edit, now.timestamp(), horizon)?.ok_or_else(|| Error::Other("Jolta couldn't add the hold.".into()))?;
    Ok(store::calendar_op(conn, op)?.remote_key)
}

/// A hold as each source takes it: tentative, private, without guests or
/// a reminder, with the marker. Microsoft 365 says tentative through
/// `showAs` and refuses a tentative status; Google and CalDAV keep the
/// status, and iCloud replaces a STATUS line passed as an extra line with
/// the event's own, so the status is the event's.
fn event(source: CalendarSource, set: &HoldSet, mark: &Mark, slot: &HeldSlot, uid: String, here: Tz) -> (EventData, Option<Extra>) {
    let at = |t: DateTime<Utc>| Local { wall: t.with_timezone(&here).naive_local(), zone: Zone::Tz(here) };
    let mut e = EventData::new(uid, Span::Timed { start: at(slot.start), end: at(slot.end) });
    let subject = store::without_reply_prefixes(set.subject.trim());
    e.title = format!("{TITLE}{}", if subject.is_empty() { "(no subject)" } else { subject });
    e.visibility = Visibility::Private;
    e.mark = Some(mark.clone());
    let extra = match source {
        CalendarSource::Graph => {
            e.show_as = ShowAs::Tentative;
            Some(json!({ "singleValueExtendedProperties": mark.graph_properties(), "isReminderOn": false }))
        }
        CalendarSource::Google => {
            e.status = EventStatus::Tentative;
            Some(json!({ "extendedProperties": { "private": mark.google_private() }, "reminders": { "useDefault": false } }))
        }
        _ => {
            e.status = EventStatus::Tentative;
            e.show_as = ShowAs::Tentative;
            return (e, (source == CalendarSource::CalDav).then(|| Extra::Lines(mark.caldav_lines())));
        }
    };
    (e, extra.map(object))
}

fn object(v: Value) -> Extra {
    match v {
        Value::Object(map) => Extra::Json(map),
        _ => unreachable!("built as an object"),
    }
}

/// A hold's create landed under `remote_id`: its slot follows it there.
/// Called when an op settles.
pub(crate) fn landed(conn: &Connection, op: &CalendarOp, remote_id: &str) -> Result<()> {
    let Write::Create(events) = &op.write else { return Ok(()) };
    let Some(set) = events.first().and_then(|e| e.mark.as_ref()).and_then(Mark::hold_set) else { return Ok(()) };
    store::refile_hold(conn, set, &op.remote_key, remote_id)
}

/// What the user hears about a set: a toast with Undo for the ops that
/// released holds, or with Release for holds just made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub set: HoldSetId,
    pub text: String,
    /// The releases Undo takes back.
    pub undo: Vec<CalendarOpId>,
    /// The toast offers Release.
    pub release: bool,
}

impl Notice {
    /// "Holding 3 times until someone books." with Release.
    pub fn made(made: &Made) -> Notice {
        let times = if made.held == 1 { "1 time".to_string() } else { format!("{} times", made.held) };
        Notice { set: made.set.clone(), text: format!("Holding {times} until someone books."), undo: Vec::new(), release: true }
    }
}

/// What a check changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// The conversations whose line changed.
    pub threads: Vec<ThreadId>,
    pub notices: Vec<Notice>,
    /// Some hold's release was queued, so the op worker has work.
    pub queued: bool,
}

/// Runs `decide` on every open set and applies what it says.
pub fn check(conn: &Connection, blobs: &Blobs, now: DateTime<Utc>, horizon: &Horizon) -> Result<Checked> {
    let mut checked = Checked::default();
    for id in super::adopt(conn, now, horizon.here)? {
        checked.threads.extend(thread(conn, &store::hold_set(conn, &id)?)?);
    }
    for set in store::hold_sets(conn)?.into_iter().filter(|s| s.sent.is_some() && s.is_open()) {
        let decisions = decide(&set, &seen(conn, blobs, &set, horizon.here)?, now);
        if !decisions.iter().any(|d| !matches!(d, Decision::Notice(_))) {
            continue;
        }
        let applied = apply(conn, &set, &decisions, now, horizon)?;
        checked.queued |= !applied.deletes.is_empty();
        checked.threads.extend(thread(conn, &set)?);
        if let Some(notice) = applied.notice {
            checked.notices.push(notice);
        }
    }
    Ok(checked)
}

struct Applied {
    deletes: Vec<CalendarOpId>,
    notice: Option<Notice>,
}

/// Applies `decisions` to `set` in the store and queues the deletes of the
/// holds they release. A booking says who booked and which holds went,
/// with those deletes for Undo.
fn apply(conn: &Connection, set: &HoldSet, decisions: &[Decision], now: DateTime<Utc>, horizon: &Horizon) -> Result<Applied> {
    let mut deletes = Vec::new();
    let mut released = Vec::new();
    let mut booked = None;
    for d in decisions {
        let Decision::Slot { slot, outcome } = d else { continue };
        let Some(held) = set.slots.iter().find(|s| s.start == *slot) else { continue };
        store::set_hold_state(conn, &set.id, *slot, outcome.state())?;
        let op = if outcome.deletes_hold() { unhold(conn, set, held, now, horizon)? } else { None };
        deletes.extend(op);
        match outcome {
            Outcome::Booked(how) => booked = Some((held.start, how.clone())),
            Outcome::Released(Release::OtherBooked) => released.extend(op),
            Outcome::Taken | Outcome::Released(Release::Passed) | Outcome::Gone => {}
        }
    }
    // After the outcomes, which name slots by their stored start.
    for d in decisions {
        if let Decision::Moved { slot, start, end } = d {
            store::move_hold(conn, &set.id, *slot, (*start, *end))?;
        }
    }
    let notice = booked.map(|(at, how)| {
        let who = match &how {
            Booking::Event { by } | Booking::Invitation { by } => name(conn, set, by),
            Booking::Meeting => "You".to_string(),
        };
        let mut text = format!("{who} booked {}.", when(at, horizon.here));
        match released.len() {
            0 => {}
            1 => text.push_str(" Jolta released the other hold."),
            n => text.push_str(&format!(" Jolta released the other {n} holds.")),
        }
        Notice { set: set.id.clone(), text, undo: released, release: false }
    });
    Ok(Applied { deletes, notice })
}

/// Queues the delete of `slot`'s hold, sent after the undo window so Undo
/// can drop it. None when its event isn't stored.
fn unhold(conn: &Connection, set: &HoldSet, slot: &HeldSlot, now: DateTime<Utc>, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    let Some(remote) = &slot.remote_id else { return Ok(None) };
    let Some(event) = store::hold_events(conn, Some(&set.id))?.into_iter().find(|m| m.calendar == set.calendar && &m.remote_id == remote) else {
        return Ok(None);
    };
    match ops::queue(conn, &Edit::quiet(set.calendar, Change::Delete { uid: event.event.uid }), now.timestamp(), horizon) {
        Ok(Some(op)) => {
            store::hold_calendar_op(conn, op, now.timestamp() + UNDO_SEND_SECONDS)?;
            Ok(Some(op))
        }
        Ok(None) => Ok(None),
        // A calendar that turned read-only keeps the hold; the slot still ends.
        Err(e) => {
            tracing::warn!("a hold of set {} stays in its calendar: {e}", set.id);
            Ok(None)
        }
    }
}

/// Release all: every held slot of the set is released, its hold deleted
/// after the undo window.
pub fn release(conn: &Connection, id: &HoldSetId, now: DateTime<Utc>, horizon: &Horizon) -> Result<Notice> {
    let set = store::hold_set(conn, id)?;
    let mut undo = Vec::new();
    for slot in set.slots.iter().filter(|s| s.state == SlotState::Held) {
        store::set_hold_state(conn, &set.id, slot.start, SlotState::Released)?;
        undo.extend(unhold(conn, &set, slot, now, horizon)?);
    }
    let text = match undo.len() {
        0 => "No holds to release.".to_string(),
        1 => "Released 1 hold.".to_string(),
        n => format!("Released {n} holds."),
    };
    Ok(Notice { set: set.id, text, undo, release: false })
}

/// Takes back the releases of `ops`: a delete still waiting is dropped, a
/// hold already deleted is made again, and each slot is held again. A slot
/// booked before stays booked, and no longer releases the others.
pub fn undo(conn: &Connection, id: &HoldSetId, releases: &[CalendarOpId], now: DateTime<Utc>, horizon: &Horizon) -> Result<String> {
    let set = store::hold_set(conn, id)?;
    let calendar = store::calendar(conn, set.calendar)?;
    let mark = Mark::Hold { set: set.id.clone(), message: set.message_id.clone() };
    let mut back = 0;
    for &op_id in releases {
        let op = store::calendar_op(conn, op_id)?;
        let filed = op.held.as_ref().map_or_else(|| op.remote_key.clone(), |h| h.remote_id.clone());
        let Some(slot) = set.slots.iter().find(|s| s.remote_id.as_deref() == Some(filed.as_str()) && s.state == SlotState::Released) else { continue };
        if slot.end <= now {
            continue;
        }
        if op.state == OpState::Pending && !op.tried {
            ops::undo(conn, op_id, now.timestamp(), horizon)?;
        } else if matches!(op.state, OpState::Failed | OpState::Conflict) {
            // The delete never happened, and its event is back in the store.
        } else {
            let key = hold(conn, &calendar, &set, &mark, slot, now, horizon)?;
            store::refile_hold(conn, &set.id, &filed, &key)?;
        }
        store::set_hold_state(conn, &set.id, slot.start, SlotState::Held)?;
        back += 1;
    }
    store::set_hold_kept(conn, &set.id, now)?;
    Ok(match back {
        0 => "Nothing to bring back.".to_string(),
        1 => "The hold is back.".to_string(),
        n => format!("The {n} holds are back."),
    })
}

/// The conversation of the set's sent mail, once it is stored.
fn thread(conn: &Connection, set: &HoldSet) -> Result<Option<ThreadId>> {
    Ok(set.message_id.as_deref().map(|m| store::sent_mail(conn, m)).transpose()?.flatten().map(|m| m.thread))
}

/// The recipients with the names the sent mail gives them.
fn people(conn: &Connection, set: &HoldSet) -> Result<Vec<Person>> {
    let named: HashMap<String, String> = match set.message_id.as_deref().map(|m| store::sent_mail(conn, m)).transpose()?.flatten() {
        Some(mail) => compose::parse_addresses(&mail.to).into_iter().chain(compose::parse_addresses(&mail.cc)).map(|(n, a)| (a.to_ascii_lowercase(), n)).collect(),
        None => HashMap::new(),
    };
    Ok(set.recipients.iter().map(|a| Person { address: a.clone(), name: named.get(&a.to_ascii_lowercase()).cloned().unwrap_or_default() }).collect())
}

/// "Ann" for "Ann Berg <ann@…>", else the address.
fn name(conn: &Connection, set: &HoldSet, address: &str) -> String {
    let person = people(conn, set).ok().and_then(|p| p.into_iter().find(|p| p.address.eq_ignore_ascii_case(address)));
    person.and_then(|p| p.name.split_whitespace().next().map(str::to_string)).unwrap_or_else(|| address.to_string())
}

/// "Tue 10 Nov 16:00".
fn when(at: DateTime<Utc>, here: Tz) -> String {
    at.with_timezone(&here).format("%a %-d %b %H:%M").to_string()
}

/// The line above a conversation's newest message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub set: HoldSetId,
    /// "Holding 3 times for this conversation · Ann replied".
    pub title: String,
    /// "Ann booked Wed 11 Nov 15:00. Release the holds?"
    pub ask: Option<String>,
    pub slots: Vec<LineSlot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineSlot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// "Tue 10 Nov 16:00".
    pub label: String,
    /// Held, booked, or taken: no longer free.
    pub state: SlotState,
}

/// The line for the newest set of `thread` that still holds a time.
pub fn line(conn: &Connection, blobs: &Blobs, thread_id: ThreadId, now: DateTime<Utc>, here: Tz) -> Result<Option<Line>> {
    let mut sets = store::hold_sets(conn)?;
    sets.retain(|s| s.sent.is_some() && s.is_open());
    let Some(set) = sets.into_iter().rev().find(|s| thread(conn, s).ok().flatten() == Some(thread_id)) else { return Ok(None) };
    let held = set.slots.iter().filter(|s| s.state == SlotState::Held).count();
    let mut title = format!("Holding {} for this conversation", if held == 1 { "1 time".to_string() } else { format!("{held} times") });
    let mut ask = None;
    for d in decide(&set, &seen(conn, blobs, &set, here)?, now) {
        match d {
            Decision::Notice(Said::Replied { by }) if !title.contains(" replied") => title.push_str(&format!(" · {} replied", name(conn, &set, &by))),
            Decision::Notice(Said::BookedElsewhere { by, start, .. }) if ask.is_none() => {
                ask = Some(format!("{} booked {}. Release the holds?", name(conn, &set, &by), when(start, here)));
            }
            _ => {}
        }
    }
    let slots = set
        .slots
        .iter()
        .filter(|s| matches!(s.state, SlotState::Held | SlotState::Booked | SlotState::Taken))
        .map(|s| LineSlot { start: s.start, end: s.end, label: when(s.start, here), state: s.state })
        .collect();
    Ok(Some(Line { set: set.id, title, ask, slots }))
}

/// Book: the editor's prefill for a meeting at the held time that starts at
/// `start`, with the recipients as guests and the conversation's subject as
/// title. Saving it books the slot through `check`, which releases the rest.
pub fn book(conn: &Connection, id: &HoldSetId, start: DateTime<Utc>, here: Tz) -> Result<EventPrefill> {
    let set = store::hold_set(conn, id)?;
    let slot = set.slots.iter().find(|s| s.start == start).ok_or_else(|| Error::NotFound("that held time".into()))?;
    let people = people(conn, &set)?;
    let picked = invite::Picked {
        account: set.account,
        thread: thread(conn, &set)?,
        people: &people,
        rooms: &[],
        span: crate::calendar::draft::timed_span(slot.start.timestamp(), slot.end.timestamp(), Zone::Tz(here), here),
    };
    let mut prefill = invite::prefill(conn, &picked)?;
    if prefill.title.is_empty() {
        prefill.title = store::without_reply_prefixes(set.subject.trim()).to_string();
    }
    Ok(prefill)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, DurationRound, TimeZone};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::invitation::tests::{import, jolta};
    use crate::calendar::{CalendarId, ical};
    use crate::compose::Compose;

    const ANN: &str = "ann@example.com";

    struct Fixture {
        j: crate::Jolta,
        work: CalendarId,
        now: DateTime<Utc>,
        /// Three times on the next days at 10:00 UTC, as (start, end, line).
        picked: Vec<(DateTime<Utc>, DateTime<Utc>, String)>,
        draft: i64,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.j.data_dir);
        }
    }

    fn horizon(now: DateTime<Utc>) -> Horizon {
        Horizon::around(now, Brussels)
    }

    fn fixture(name: &str) -> Fixture {
        let (j, work) = jolta(name);
        // A CalDAV calendar's events are filed under URLs below its own.
        j.store.write(|tx| Ok(tx.execute("UPDATE calendar SET remote_id = 'https://dav.example.com/cal/work/' WHERE id = ?1", [work])?)).unwrap();
        let now = Utc::now().duration_trunc(Duration::minutes(1)).unwrap();
        let day = now.duration_trunc(Duration::days(1)).unwrap();
        let times: Vec<(DateTime<Utc>, DateTime<Utc>)> = (2..5).map(|d| (day + Duration::days(d) + Duration::hours(10), day + Duration::days(d) + Duration::hours(10) + Duration::minutes(30))).collect();
        let (_, lines) = text(&times, Brussels, Brussels);
        let picked = times.iter().zip(lines).map(|((s, e), l)| (*s, *e, l)).collect();
        let draft = j.store.write(|tx| store::save_draft(tx, &compose(None, ""))).unwrap();
        Fixture { j, work, now, picked, draft }
    }

    fn compose(draft: Option<i64>, body: &str) -> Compose {
        Compose {
            account: Some(AccountId(1)),
            to: format!("Ann Berg <{ANN}>"),
            cc: "me@example.com".into(),
            subject: "Re: Planning".into(),
            body: body.into(),
            draft,
            ..Compose::default()
        }
    }

    /// The mail with `body` leaves the outbox now, as the engine records it.
    fn send(f: &Fixture, body: &str) -> Option<Made> {
        let account = f.j.store.read(|c| store::account(c, AccountId(1))).unwrap();
        let built = compose::build(&account, &compose(Some(f.draft), body), true).unwrap();
        f.j.store
            .write(|tx| {
                let id = store::queue_outbox(tx, AccountId(1), Some(f.draft), "blob", &built.message_id, &built.from, &built.recipients, None, 0)?;
                let item = store::due_outbox(tx, AccountId(1))?.into_iter().find(|i| i.id == id).unwrap();
                store::outbox_sent(tx, &item)?;
                sent(tx, &item, &built.raw, f.now, &horizon(f.now))
            })
            .unwrap()
    }

    fn ops(f: &Fixture) -> Vec<CalendarOp> {
        f.j.store.read(|c| store::calendar_ops(c, 100)).unwrap()
    }

    fn set(f: &Fixture) -> HoldSet {
        f.j.store.read(store::hold_sets).unwrap().remove(0)
    }

    fn states(f: &Fixture) -> Vec<SlotState> {
        set(f).slots.iter().map(|s| s.state).collect()
    }

    fn pend(f: &Fixture) -> HoldSetId {
        f.j.store.write(|tx| pending(tx, f.draft, AccountId(1), &f.picked, f.now)).unwrap()
    }

    fn body(f: &Fixture, lines: &[usize]) -> String {
        let kept: Vec<&str> = lines.iter().map(|&i| f.picked[i].2.as_str()).collect();
        format!("Hi Ann,\n\nI'm free at these times:\n{}\n\nMe", kept.join("\n"))
    }

    #[test]
    fn a_pending_set_goes_with_its_draft_and_undo_send_holds_nothing() {
        let f = fixture("holds-pending");
        pend(&f);
        let again = pend(&f);
        let sets = f.j.store.read(store::hold_sets).unwrap();
        assert_eq!(sets.iter().map(|s| &s.id).collect::<Vec<_>>(), [&again], "inserting again replaces the draft's set");
        assert_eq!(sets[0].slots.iter().map(|s| s.state).collect::<Vec<_>>(), [SlotState::Pending; 3]);

        let account = f.j.store.read(|c| store::account(c, AccountId(1))).unwrap();
        let built = compose::build(&account, &compose(Some(f.draft), &body(&f, &[0, 1, 2])), true).unwrap();
        f.j.store
            .write(|tx| {
                let id = store::queue_outbox(tx, AccountId(1), Some(f.draft), "blob", &built.message_id, &built.from, &built.recipients, None, 10)?;
                store::cancel_send(tx, id)
            })
            .unwrap();
        assert!(ops(&f).is_empty(), "Undo send holds nothing");
        assert_eq!(set(&f).sent, None, "the set waits for the draft");

        f.j.store.write(|tx| discarded(tx, f.draft)).unwrap();
        assert!(f.j.store.read(store::hold_sets).unwrap().is_empty(), "a discarded draft's set goes");
        let too_many: Vec<_> = (0..6).map(|i| (f.now + Duration::hours(i + 1), f.now + Duration::hours(i + 2), format!("line {i}"))).collect();
        assert!(f.j.store.write(|tx| pending(tx, f.draft, AccountId(1), &too_many, f.now)).is_err(), "at most 5");
    }

    #[test]
    fn the_mail_leaving_holds_the_lines_still_in_it() {
        let f = fixture("holds-sent");
        let id = pend(&f);
        let made = send(&f, &body(&f, &[0, 2])).unwrap();
        assert_eq!(made, Made { set: id.clone(), held: 2 }, "a line edited out before sending isn't held");
        assert_eq!(Notice::made(&made).text, "Holding 2 times until someone books.");

        let set = set(&f);
        assert_eq!((set.draft, set.recipients.clone(), set.subject.as_str(), set.calendar), (None, vec![ANN.to_string()], "Re: Planning", f.work));
        assert!(set.message_id.is_some() && set.sent == Some(f.now));
        assert_eq!(set.slots.iter().map(|s| s.start).collect::<Vec<_>>(), [f.picked[0].0, f.picked[2].0]);
        let mut ops = ops(&f);
        ops.sort_by_key(|o| o.id);
        assert_eq!(ops.len(), 2, "one create per slot");
        for (op, slot) in ops.iter().zip(&set.slots) {
            assert_eq!((slot.state, slot.remote_id.as_deref()), (SlotState::Held, Some(op.remote_key.as_str())));
            assert_eq!(op.send_after, f.now.timestamp(), "the mail already left");
            let Write::Create(events) = &op.write else { panic!("a create") };
            let e = &events[0];
            assert_eq!((e.title.as_str(), e.status, e.show_as, e.visibility), ("Hold: Planning", EventStatus::Tentative, ShowAs::Tentative, Visibility::Private));
            assert!(e.attendees.is_empty() && e.alarms.is_empty() && e.organizer.is_none());
            assert_eq!(e.mark, Some(Mark::Hold { set: id.clone(), message: set.message_id.clone() }));
            assert_eq!(op.extra, Some(Extra::Lines(e.mark.as_ref().unwrap().caldav_lines())));
        }
        let stored = f.j.store.read(|c| store::hold_events(c, Some(&id))).unwrap();
        assert_eq!(stored.iter().map(|m| Some(m.remote_id.clone())).collect::<Vec<_>>(), set.slots.iter().map(|s| s.remote_id.clone()).collect::<Vec<_>>());
        assert_eq!(send(&f, &body(&f, &[0, 1, 2])), None, "a set is held once");
    }

    #[test]
    fn each_provider_gets_a_tentative_private_hold_without_a_reminder() {
        let mark = Mark::Hold { set: HoldSetId::parse("5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10").unwrap(), message: Some("<m1@example.com>".into()) };
        let now = Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap();
        let slot = HeldSlot { start: now + Duration::days(1), end: now + Duration::days(1) + Duration::minutes(30), line: String::new(), state: SlotState::Pending, remote_id: None };
        let set = HoldSet { id: HoldSetId::new().unwrap(), account: AccountId(1), message_id: None, draft: None, recipients: vec![], subject: "AW: Plan".into(), calendar: CalendarId(1), sent: None, kept: false, slots: vec![] };
        let (graph, extra) = event(CalendarSource::Graph, &set, &mark, &slot, "u1".into(), Brussels);
        assert_eq!((graph.title.as_str(), graph.show_as, graph.status, graph.visibility), ("Hold: Plan", ShowAs::Tentative, EventStatus::Confirmed, Visibility::Private), "Microsoft 365 refuses a tentative status");
        let Some(Extra::Json(extra)) = extra else { panic!() };
        assert_eq!((extra["singleValueExtendedProperties"].clone(), extra["isReminderOn"].clone()), (mark.graph_properties(), json!(false)));
        let body = crate::calendar::sources::graph::write::create_body(&[graph], "tx1", Some(&Extra::Json(extra)), Brussels).unwrap();
        assert_eq!((body["showAs"].clone(), body["sensitivity"].clone(), body["isReminderOn"].clone()), (json!("tentative"), json!("private"), json!(false)));
        assert_eq!(body["singleValueExtendedProperties"].as_array().unwrap().len(), 4, "the UID's two and the marker's two");

        let (google, extra) = event(CalendarSource::Google, &set, &mark, &slot, "u2".into(), Brussels);
        assert_eq!((google.status, google.show_as), (EventStatus::Tentative, ShowAs::Busy));
        let body = crate::calendar::sources::google::write::insert_body(&[google], extra.as_ref(), "r", Brussels).unwrap();
        assert_eq!((body["status"].clone(), body["visibility"].clone(), body["reminders"].clone()), (json!("tentative"), json!("private"), json!({ "useDefault": false })));
        assert_eq!(body["extendedProperties"]["private"], mark.google_private());

        let (local, extra) = event(CalendarSource::Local, &set, &mark, &slot, "u3".into(), Brussels);
        assert_eq!((local.status, extra), (EventStatus::Tentative, None));
    }

    /// A meeting `who` organizes over the set's first slot, as sync stores it.
    fn meeting(f: &Fixture, remote_id: &str, who: &str) {
        let (s, e) = (f.picked[0].0, f.picked[0].1);
        let ics = |t: DateTime<Utc>| t.format("%Y%m%dT%H%M%SZ").to_string();
        let text = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{remote_id}@example.com\r\nDTSTAMP:20261101T080000Z\r\nCREATED:{}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:Planning\r\n\
             ORGANIZER:mailto:{who}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            ics(f.now + Duration::minutes(1)),
            ics(s),
            ics(e)
        );
        let events = ical::read(&text, &["me@example.com".into()]).unwrap();
        f.j.store.write(|tx| store::apply_item(tx, f.work, remote_id, None, &events, None, &horizon(f.now))).unwrap();
    }

    fn check_at(f: &Fixture, at: DateTime<Utc>) -> Checked {
        f.j.store.write(|tx| check(tx, &f.j.blobs, at, &horizon(at))).unwrap()
    }

    #[test]
    fn a_booking_releases_the_others_once_and_undo_brings_them_back() {
        let f = fixture("holds-book");
        let id = pend(&f);
        send(&f, &body(&f, &[0, 1, 2]));
        f.j.store
            .write(|tx| {
                for op in store::claim_calendar_ops(tx, f.now.timestamp(), 10)? {
                    ops::settle(tx, &op, Ok(ops::Sent::Done { version: None, merged: false }), f.now.timestamp(), &horizon(f.now))?;
                }
                Ok(())
            })
            .unwrap();
        import(&f.j, b"From: Me <me@example.com>\r\nTo: Ann Berg <ann@example.com>\r\nSubject: Re: Planning\r\nDate: Mon, 09 Nov 2026 09:00:00 +0000\r\nMessage-ID: <unrelated@example.com>\r\n\r\nHi\r\n");
        assert_eq!(check_at(&f, f.now), Checked::default(), "nothing happened yet");
        meeting(&f, "booked", ANN);
        let checked = check_at(&f, f.now + Duration::minutes(2));
        let [notice] = checked.notices.as_slice() else { panic!("{checked:?}") };
        let when = f.picked[0].0.with_timezone(&Brussels).format("%a %-d %b %H:%M");
        assert_eq!(notice.text, format!("{ANN} booked {when}. Jolta released the other 2 holds."), "named by address: the sent mail isn't stored");
        assert!(checked.queued && !notice.release);
        assert_eq!(states(&f), [SlotState::Booked, SlotState::Released, SlotState::Released]);
        let deletes: Vec<CalendarOp> = ops(&f).into_iter().filter(|o| o.write.kind() == "delete").collect();
        assert_eq!(deletes.len(), 3, "the booked slot's hold goes too: the meeting holds the time");
        assert!(deletes.iter().all(|o| o.send_after == f.now.timestamp() + 120 + UNDO_SEND_SECONDS), "after the undo window");
        assert_eq!(notice.undo.len(), 2, "Undo brings back only the released holds");
        assert_eq!(check_at(&f, f.now + Duration::minutes(3)), Checked::default(), "releasing twice deletes once");

        let at = f.now + Duration::minutes(2) + Duration::seconds(5);
        let (first, second) = (notice.undo[0], notice.undo[1]);
        // The booked slot's delete and the first release reached the server.
        f.j.store.write(|tx| store::claim_calendar_ops(tx, at.timestamp() + UNDO_SEND_SECONDS, 2).map(|_| ())).unwrap();
        let said = f.j.store.write(|tx| undo(tx, &id, &notice.undo, at, &horizon(at))).unwrap();
        assert_eq!(said, "The 2 holds are back.");
        assert_eq!(states(&f), [SlotState::Booked, SlotState::Held, SlotState::Held]);
        let all = ops(&f);
        assert!(all.iter().all(|o| o.id != second), "a delete still waiting is dropped");
        assert!(all.iter().any(|o| o.id == first), "one already claimed stays");
        let remade: Vec<&CalendarOp> = all.iter().filter(|o| o.write.kind() == "create" && o.created == at.timestamp()).collect();
        assert_eq!(remade.len(), 1, "and its hold is made again");
        assert_eq!(set(&f).slots[1].remote_id.as_deref(), Some(remade[0].remote_key.as_str()));
        assert!(set(&f).kept);
        assert_eq!(check_at(&f, at + Duration::minutes(1)), Checked::default(), "the booking doesn't release them again");
    }

    #[test]
    fn a_failed_create_keeps_the_other_holds_and_is_tried_again() {
        let f = fixture("holds-retry");
        pend(&f);
        send(&f, &body(&f, &[0, 1, 2]));
        let claimed = f.j.store.write(|tx| store::claim_calendar_ops(tx, f.now.timestamp(), 1)).unwrap();
        f.j.store.write(|tx| ops::settle(tx, &claimed[0], Err("offline".into()), f.now.timestamp(), &horizon(f.now)).map(|_| ())).unwrap();
        let first = f.j.store.read(|c| store::calendar_op(c, claimed[0].id)).unwrap();
        assert_eq!((first.state, first.attempts), (OpState::Pending, 1));
        assert!(first.send_after > f.now.timestamp(), "tried again later");
        assert_eq!(check_at(&f, f.now + Duration::minutes(1)), Checked::default(), "its hold is still there meanwhile");
        assert_eq!(states(&f), [SlotState::Held; 3]);
    }

    #[test]
    fn undo_after_a_delete_that_failed_for_good_makes_no_second_hold() {
        let f = fixture("holds-undo-failed");
        let id = pend(&f);
        send(&f, &body(&f, &[0]));
        let at = f.now + Duration::minutes(1);
        f.j.store
            .write(|tx| {
                for op in store::claim_calendar_ops(tx, at.timestamp(), 10)? {
                    ops::settle(tx, &op, Ok(ops::Sent::Done { version: None, merged: false }), at.timestamp(), &horizon(at))?;
                }
                Ok(())
            })
            .unwrap();
        let notice = f.j.store.write(|tx| release(tx, &id, at, &horizon(at))).unwrap();
        let later = at + Duration::minutes(1);
        f.j.store
            .write(|tx| {
                let mut claimed = store::claim_calendar_ops(tx, later.timestamp(), 10)?;
                claimed[0].attempts = ops::MAX_ATTEMPTS - 1;
                ops::settle(tx, &claimed[0], Err("refused".into()), later.timestamp(), &horizon(later)).map(|_| ())
            })
            .unwrap();
        assert_eq!(f.j.store.read(|c| store::hold_events(c, Some(&id))).unwrap().len(), 1, "the failed delete put the hold back");
        f.j.store.write(|tx| undo(tx, &id, &notice.undo, later, &horizon(later))).unwrap();
        assert_eq!(f.j.store.read(|c| store::hold_events(c, Some(&id))).unwrap().len(), 1, "and Undo makes no second one");
        assert_eq!(states(&f), [SlotState::Held]);
    }

    #[test]
    fn release_all_then_book_reads_the_set() {
        let f = fixture("holds-release");
        let id = pend(&f);
        send(&f, &body(&f, &[0, 1]));
        let prefill = f.j.store.read(|c| book(c, &id, f.picked[1].0, Brussels)).unwrap();
        assert_eq!(prefill.title, "Planning");
        assert_eq!(prefill.required.iter().map(|p| p.address.as_str()).collect::<Vec<_>>(), [ANN]);
        assert_eq!(prefill.span, Some(crate::calendar::draft::timed_span(f.picked[1].0.timestamp(), f.picked[1].1.timestamp(), Zone::Tz(Brussels), Brussels)));
        let notice = f.j.store.write(|tx| release(tx, &id, f.now, &horizon(f.now))).unwrap();
        assert_eq!((notice.text.as_str(), notice.undo.len()), ("Released 2 holds.", 2));
        assert_eq!(f.j.store.write(|tx| release(tx, &id, f.now, &horizon(f.now))).unwrap().undo, [], "releasing twice deletes once");
        assert_eq!(states(&f), [SlotState::Released; 2]);
    }
}
