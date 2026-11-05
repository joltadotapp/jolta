//! Add to calendar: puts an invitation into a calendar of the user's
//! choice when the account it came to has no copy, because that account's
//! calendar is off in Jolta, as for iCloud. Nothing is ever added without
//! the user asking: calendar spam relies on automatic adding.
//!
//! The copy keeps the invitation's UID on every provider, so later mail
//! finds it: Google's `events.import`, a CalDAV PUT of the invitation's own
//! VEVENTs, and on Microsoft 365 a Global Object ID that carries the UID.
//! Nobody is told: the import notifies no one, CalDAV's ORGANIZER gets
//! SCHEDULE-AGENT=CLIENT, and Graph's copy has no guests. The answer still
//! goes from the account the invitation came to by mail (`respond`); the
//! copy only follows it (`follow`). A newer version from the organizer is
//! written over the copy when the user asks (`update`), and a cancellation's
//! Remove from calendar takes it out (`remove`).

use chrono::Utc;
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension};

use super::card::{self, Card, EventCard};
use super::invitation::{self, Invitation, Method};
use super::rsvp::{self, Answer, Rsvp, Scope, Target};
use super::series::Horizon;
use super::sources::{Change, NewCopy, Replaced};
use super::update::Version;
use super::{Access, Calendar, CalendarId, CalendarSource, EventData, EventId, Moment, Response, sync, tz};
use crate::model::{AccountId, MessageId};
use crate::store::{self, AddedCopy, WrittenVersion};
use crate::{Error, Jolta, Result, imap};

/// One calendar the Add to calendar menu offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub calendar: CalendarId,
    pub name: String,
    /// The address of the calendar's account.
    pub account: String,
    pub color: Option<String>,
    /// The menu's default: the calendar the user last added to, else the
    /// default calendar.
    pub default: bool,
}

/// What Add to calendar needs on the card.
#[derive(Clone, Debug, PartialEq)]
pub struct Adding {
    pub message: MessageId,
    pub uid: String,
}

/// What Update in your calendar needs on the card.
#[derive(Clone, Debug, PartialEq)]
pub struct Updating {
    pub message: MessageId,
    pub uid: String,
    /// The name of the calendar holding the copy.
    pub calendar: String,
}

/// The calendars an invitation from `organizer` can go to: every shown
/// calendar of an account that Jolta can write an event to, except one that
/// already holds an event with its UID, which a second copy would collide
/// with, and those of the organizer's own account, where the meeting is.
pub fn choices(conn: &Connection, uid: &str, organizer: Option<&str>) -> Result<Vec<Choice>> {
    let accounts = store::accounts(conn)?;
    let last = store::last_added_calendar(conn)?;
    let mut holding = conn.prepare_cached("SELECT 1 FROM event WHERE calendar_id = ?1 AND uid = ?2 LIMIT 1")?;
    let mut out = Vec::new();
    let mut fallback = None;
    for c in store::calendars(conn)? {
        let writable = matches!(c.access, Access::Owner | Access::Write) && matches!(c.source, CalendarSource::Google | CalendarSource::Graph | CalendarSource::CalDav);
        let Some(account) = c.account_id.and_then(|id| accounts.iter().find(|a| a.id == id)) else { continue };
        let organizes = organizer.is_some_and(|o| o.trim().eq_ignore_ascii_case(&account.address));
        if organizes || !c.shown || !writable || holding.query_row(rusqlite::params![c.id, uid], |_| Ok(())).optional()?.is_some() {
            continue;
        }
        if c.is_default {
            fallback = Some(c.id);
        }
        out.push(Choice { calendar: c.id, name: c.name, account: account.address.clone(), color: c.color, default: false });
    }
    let chosen = last.filter(|l| out.iter().any(|c| c.calendar == *l)).or(fallback).or(out.first().map(|c| c.calendar));
    for c in &mut out {
        c.default = Some(c.calendar) == chosen;
    }
    Ok(out)
}

/// Add to calendar and Update in your calendar for a request from the
/// meeting's organizer that the card offers answers on. `sent` is the
/// card's invitation. Add while the account it came to has no copy of
/// its own or added; Update while the copy added holds an older version.
pub(crate) fn offers(conn: &Connection, sent: &Invitation, cx: &card::Context) -> Result<(Option<Adding>, Option<Updating>)> {
    if sent.method != Method::Request || !sent.dated {
        return Ok((None, None));
    }
    let uid = &sent.event.uid;
    let rid = sent.event.recurrence_id.as_ref();
    if let Some(copy) = store::added_copy(conn, cx.account, uid)?
        && let Some(event) = copied(conn, cx.account, uid, rid, cx.here)?
    {
        let updating = match newer(&copy.written, rid, Version::of(sent), cx.here) {
            true => Some(Updating { message: cx.message, uid: uid.clone(), calendar: store::calendar(conn, event.calendar_id)?.name }),
            false => None,
        };
        return Ok((None, updating));
    }
    let own = store::stored_event(conn, uid, None, Some(cx.account), cx.here)?.is_some()
        || rid.map(|rid| store::stored_event(conn, uid, Some(rid), Some(cx.account), cx.here)).transpose()?.flatten().is_some();
    let organizer = sent.event.organizer.as_ref().map(|o| o.address.as_str());
    let adding = (!own && !choices(conn, uid, organizer)?.is_empty()).then(|| Adding { message: cx.message, uid: uid.clone() });
    Ok((adding, None))
}

/// The added copy's event for `rid`, its series else; an invitation to one
/// occurrence may have become a single event.
fn copied(conn: &Connection, account: AccountId, uid: &str, rid: Option<&Moment>, here: Tz) -> Result<Option<store::StoredEvent>> {
    match rid.map(|rid| store::added_event(conn, account, uid, Some(rid), here)).transpose()?.flatten() {
        Some(e) => Ok(Some(e)),
        None => store::added_event(conn, account, uid, None, here),
    }
}

/// The added copy's event that an answer or an update for `rid` goes to,
/// with its scope: that occurrence of a series, also one stored changed or
/// without its master, else the event itself. Also the remote id of the
/// object Jolta created.
fn copy_target(conn: &Connection, account: AccountId, uid: &str, rid: Option<&Moment>, here: Tz) -> Result<Option<(EventId, Scope, String)>> {
    let Some(copy) = store::added_copy(conn, account, uid)? else { return Ok(None) };
    let Some(event) = copied(conn, account, uid, rid, here)? else { return Ok(None) };
    let scope = match rid {
        Some(rid) if event.event.recurrence.is_some() || event.event.recurrence_id.is_some() => Scope::Occurrence(*rid),
        _ => Scope::Series,
    };
    Ok(Some((event.event_id, scope, copy.remote_id)))
}

/// What an answer or an update for `rid` goes to in the copy the user
/// added of the invitation `uid` to `account`, with its event and scope:
/// the object Jolta created, never another event with its UID.
async fn added_target(jolta: &Jolta, account: AccountId, uid: &str, rid: Option<Moment>) -> Result<Option<(Calendar, Target, EventId, Scope)>> {
    let here = tz::here();
    let uid = uid.to_string();
    let Some((event, scope, object)) = jolta.store.read_async(move |c| copy_target(c, account, &uid, rid.as_ref(), here)).await? else { return Ok(None) };
    let (calendar, target) = rsvp::target_of(jolta, event, &scope, Some(object)).await?;
    Ok(Some((calendar, target, event, scope)))
}

/// Whether `version` of the event or occurrence `rid` is newer than what
/// was written into the copy for it, or for its series.
fn newer(written: &[WrittenVersion], rid: Option<&Moment>, version: Version, here: Tz) -> bool {
    let same = |w: &&WrittenVersion| match (&w.recurrence_id, rid) {
        (None, None) => true,
        (Some(a), Some(b)) => a.names(b, here),
        _ => false,
    };
    let entry = written.iter().find(same).or_else(|| written.iter().find(|w| w.recurrence_id.is_none()));
    entry.is_none_or(|w| version > Version { sequence: w.sequence, stamp: w.stamp })
}

/// The versions the copy holds after `sent` was written into it: a series
/// or single event replaces them all, an occurrence its own.
fn written_after(before: &[WrittenVersion], sent: &Invitation, here: Tz) -> Vec<WrittenVersion> {
    let new = WrittenVersion { recurrence_id: sent.event.recurrence_id, sequence: sent.event.sequence, stamp: sent.stamp };
    let Some(rid) = &sent.event.recurrence_id else { return vec![new] };
    let mut out: Vec<WrittenVersion> = before.iter().filter(|w| !w.recurrence_id.is_some_and(|r| r.names(rid, here))).cloned().collect();
    out.push(new);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Added {
    /// For the toast: "Added to Work."
    pub text: String,
    pub calendar: CalendarId,
}

/// The card's invitation on a message, with what Add and Update write.
struct Offered {
    account: AccountId,
    receiver: String,
    card: EventCard,
    sent: Invitation,
    events: Vec<EventData>,
    text: String,
}

impl Offered {
    fn copy(&self) -> NewCopy<'_> {
        let answer = match self.card.answer {
            Some(Response::Accepted) => Some(Answer::Accept),
            Some(Response::Tentative) => Some(Answer::Maybe),
            Some(Response::Declined) => Some(Answer::Decline),
            _ => None,
        };
        NewCopy { uid: &self.sent.event.uid, text: &self.text, events: &self.events, me: &self.receiver, answer }
    }
}

/// The Add to calendar menu of the card on `message`.
pub async fn menu(jolta: &Jolta, message: MessageId) -> Result<Vec<Choice>> {
    let o = offered(jolta, message).await?;
    if o.card.adding.is_none() {
        return Ok(Vec::new());
    }
    let (uid, organizer) = (o.sent.event.uid.clone(), o.sent.event.organizer.map(|p| p.address));
    jolta.store.read_async(move |c| choices(c, &uid, organizer.as_deref())).await
}

/// Puts the invitation on `message` into `calendar`, or the menu's
/// default, when its card offers Add to calendar.
pub async fn add(jolta: &Jolta, message: MessageId, calendar: Option<CalendarId>) -> Result<Added> {
    let o = offered(jolta, message).await?;
    if o.card.adding.is_none() {
        return Err(Error::Other("This meeting is in your calendar already, or Jolta has no calendar to add it to.".into()));
    }
    let uid = o.sent.event.uid.clone();
    let organizer = o.sent.event.organizer.as_ref().map(|p| p.address.clone());
    let established = organizer.clone().ok_or_else(|| Error::Other("Jolta adds an invitation only from its organizer.".into()))?;
    let calendar = jolta
        .store
        .read_async({
            let uid = uid.clone();
            move |c| {
                let list = choices(c, &uid, organizer.as_deref())?;
                let id = calendar.or(list.iter().find(|c| c.default).map(|c| c.calendar)).ok_or_else(|| Error::Other("Jolta has no calendar it can add this meeting to.".into()))?;
                if !list.iter().any(|c| c.calendar == id) {
                    return Err(Error::Other("Jolta can't add this meeting to that calendar: it's read-only, hidden, or holds the meeting already.".into()));
                }
                store::calendar(c, id)
            }
        })
        .await?;
    let source = sync::calendar_source(jolta, &calendar).await?;
    let (remote_id, change, complete) = source.create(&calendar, &o.copy()).await?;
    let id = calendar.id;
    let blob = match &change {
        Some(Change::Upsert { raw: Some(raw), .. }) => Some(jolta.blobs.put(raw)?),
        _ => None,
    };
    let (account, written) = (o.account, written_on_add(complete, &o.sent, tz::here()));
    jolta
        .store
        .write_async(move |tx| {
            if let Some(Change::Upsert { remote_id, etag, events, .. }) = &change {
                store::apply_item(tx, id, remote_id, etag.as_deref(), events, blob.as_deref(), &Horizon::around(Utc::now(), tz::here()))?;
            }
            store::remember_added(tx, account, &uid, &AddedCopy { calendar_id: id, remote_id, organizer: established, written })?;
            store::set_last_added_calendar(tx, id)
        })
        .await?;
    if calendar.source != CalendarSource::CalDav {
        sync::sync_calendar(jolta, id).await?;
    }
    let text = match complete {
        true => format!("Added to {}.", calendar.name),
        false => format!("Added to {}, but the meeting changed there while Jolta added it. Use Update in your calendar.", calendar.name),
    };
    Ok(Added { text, calendar: id })
}

/// The versions a new copy holds: none when part of the version couldn't
/// be written, so the card keeps offering Update in your calendar.
fn written_on_add(complete: bool, sent: &Invitation, here: Tz) -> Vec<WrittenVersion> {
    if complete { written_after(&[], sent, here) } else { Vec::new() }
}

/// Writes the newer version on `message` over the copy the user added,
/// telling nobody, while that copy is the version Jolta last read.
pub async fn update(jolta: &Jolta, message: MessageId) -> Result<Added> {
    let o = offered(jolta, message).await?;
    if o.card.updating.is_none() {
        return Err(Error::Other("Your calendar's copy already has this version of the meeting.".into()));
    }
    let (account, uid, here) = (o.account, o.sent.event.uid.clone(), tz::here());
    let (calendar, mut target, _, _) =
        added_target(jolta, account, &uid, o.sent.event.recurrence_id).await?.ok_or_else(|| Error::NotFound("Your calendar no longer has this meeting.".into()))?;
    let (before, held) = jolta
        .store
        .read_async({
            let (uid, event, object) = (uid.clone(), target.event.id, target.event.remote_id.clone());
            move |c| Ok((store::added_copy(c, account, &uid)?.map(|a| a.written).unwrap_or_default(), rsvp::changed_occurrences(c, event, &object)?))
        })
        .await?;
    target.me = o.receiver.clone();
    let source = sync::calendar_source(jolta, &calendar).await?;
    let id = calendar.id;
    match source.replace(&calendar, &target, &held, &o.copy()).await? {
        Replaced::Changed => {
            sync::sync_calendar(jolta, id).await?;
            return Err(Error::Other("The meeting changed in your calendar since Jolta last read it; check it again.".into()));
        }
        Replaced::Written(Some(Change::Upsert { remote_id, etag, events, raw })) => {
            let blob = raw.as_ref().map(|r| jolta.blobs.put(r)).transpose()?;
            jolta.store.write_async(move |tx| store::apply_item(tx, id, &remote_id, etag.as_deref(), &events, blob.as_deref(), &Horizon::around(Utc::now(), here)).map(|_| ())).await?;
        }
        Replaced::Written(_) => {
            sync::sync_calendar(jolta, id).await?;
        }
    }
    let written = written_after(&before, &o.sent, here);
    jolta
        .store
        .write_async(move |tx| {
            let Some(mut copy) = store::added_copy(tx, account, &uid)? else { return Ok(()) };
            copy.written = written;
            store::remember_added(tx, account, &uid, &copy)
        })
        .await?;
    Ok(Added { text: format!("Updated in {}.", calendar.name), calendar: id })
}

/// Gives the copy the user added of the invitation `uid` to `account` the
/// answer just sent by mail, telling nobody: the whole event, or the
/// occurrence `rid`. Returns the calendar that changed, None without a copy.
pub(crate) async fn follow(jolta: &Jolta, account: AccountId, uid: &str, rid: Option<Moment>, answer: Answer) -> Result<Option<CalendarId>> {
    let here = tz::here();
    let Some((calendar, mut target, event, scope)) = added_target(jolta, account, uid, rid).await? else { return Ok(None) };
    target.me = jolta.store.read_async(move |c| store::account(c, account)).await?.address;
    let source = sync::calendar_source(jolta, &calendar).await?;
    let rsvp = Rsvp { event, answer, note: None, notify: false, scope };
    let id = calendar.id;
    match source.follow(&calendar, &target, &rsvp).await? {
        Some(Change::Upsert { remote_id, etag, events, raw }) => {
            let blob = raw.as_ref().map(|r| jolta.blobs.put(r)).transpose()?;
            jolta.store.write_async(move |tx| store::apply_item(tx, id, &remote_id, etag.as_deref(), &events, blob.as_deref(), &Horizon::around(Utc::now(), here)).map(|_| ())).await?;
        }
        _ => {
            sync::sync_calendar(jolta, id).await?;
        }
    }
    Ok(Some(id))
}

/// The card on `message` that offers Add or Update, with its invitation
/// and the calendar part it came in.
async fn offered(jolta: &Jolta, message: MessageId) -> Result<Offered> {
    let (account, sender): (AccountId, String) = jolta
        .store
        .read_async(move |c| {
            c.query_row("SELECT account_id, from_addr FROM message WHERE id = ?1", [message], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("message {message}")))
        })
        .await?;
    let raw = imap::message_source(jolta, message).await?;
    let blobs = jolta.blobs.clone();
    jolta
        .store
        .read_async(move |c| {
            let every: Vec<String> = store::accounts(c)?.into_iter().map(|a| a.address).collect();
            let receiver = store::account(c, account)?.address;
            let invitations = invitation::find(&raw, &every);
            let cx = card::Context { message, account, me: &every, sender: &sender, now: Utc::now(), here: tz::here(), blobs: &blobs };
            let cards: Vec<EventCard> = card::cards(c, &invitations, &cx)?
                .into_iter()
                .filter_map(|card| match card {
                    Card::Event(e) => Some(e),
                    Card::Reply(_) => None,
                })
                .collect();
            let first = cards.iter().position(|e| e.adding.is_some() || e.updating.is_some()).unwrap_or(0);
            let card = cards.into_iter().nth(first).ok_or_else(|| Error::Other("This message has no invitation to add.".into()))?;
            let uid = match (&card.adding, &card.updating, &card.answering) {
                (Some(a), _, _) => a.uid.clone(),
                (_, Some(u), _) => u.uid.clone(),
                (_, _, Some(a)) => a.uid.clone(),
                _ => return Err(Error::Other("Jolta adds and updates an invitation only from its organizer, while it is the newest version.".into())),
            };
            let mut mine: Vec<&Invitation> = invitations.iter().filter(|i| i.event.uid == uid).collect();
            mine.sort_by_key(|i| i.event.recurrence_id.is_some());
            let sent = (*mine.first().expect("the card's invitation")).clone();
            let text = invitation::calendar_text(&raw, &uid).ok_or_else(|| Error::Other("Jolta can't find this invitation's calendar part.".into()))?;
            Ok(Offered { account, receiver, card, events: mine.iter().map(|i| i.event.clone()).collect(), sent, text })
        })
        .await
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, TimeZone};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::card::tests::{V1, version};
    use crate::calendar::invitation::tests::{ME, import, jolta};
    use crate::calendar::{EventStatus, Local, Person, Span, Zone};

    const OTHER: &str = "me@gmail.example";

    /// The invitation fixture's folder with account 1's calendar off, as
    /// for iCloud, and account 2 with a calendar the user owns, one shared
    /// read-only and one hidden.
    fn folder(name: &str) -> (crate::Jolta, CalendarId, CalendarId, CalendarId) {
        let (j, _) = jolta(name);
        let ids = j
            .store
            .write(|tx| {
                tx.execute_batch(&format!(
                    "DELETE FROM calendar WHERE account_id = 1; UPDATE account SET calendar = 0 WHERE id = 1;
                     INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (2, 'gmail', '{OTHER}', '{OTHER}', 'a2', 0, 1);"
                ))?;
                let cal = |id: &str, name: &str, access, primary| store::RemoteCalendar { remote_id: id.into(), name: name.into(), color: None, zone: None, access, primary, refresh: None };
                let list = [cal("personal", "Personal", Access::Owner, true), cal("team", "Team", Access::Read, false), cal("side", "Side", Access::Owner, false)];
                let made = store::sync_calendar_list(tx, AccountId(2), CalendarSource::Google, &list)?;
                Ok(made.into_iter().map(|c| (c.name, c.id)).collect::<Vec<_>>())
            })
            .unwrap();
        let find = |name: &str| ids.iter().find(|(n, _)| n == name).expect("made").1;
        j.store.write(|tx| store::set_calendar_shown(tx, find("Team"), true).and_then(|_| store::set_calendar_shown(tx, find("Side"), false))).unwrap();
        (j, find("Personal"), find("Team"), find("Side"))
    }

    fn card_of(j: &crate::Jolta, message: MessageId, raw: &str) -> EventCard {
        let me = [ME.to_string(), OTHER.to_string()];
        let found = invitation::find(raw.as_bytes(), &me);
        let sender = raw.lines().find_map(|l| l.strip_prefix("From: ")).unwrap_or_default().to_string();
        let now = Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap();
        let cx = card::Context { message, account: AccountId(1), me: &me, sender: &sender, now, here: Brussels, blobs: &j.blobs };
        match j.store.read(|c| card::cards(c, &found, &cx)).unwrap().remove(0) {
            Card::Event(e) => e,
            other => panic!("{other:?}"),
        }
    }

    /// The events a request sends, as a calendar would hold its copy.
    fn events_of(raw: &str) -> Vec<EventData> {
        invitation::find(raw.as_bytes(), &[ME.into()]).into_iter().map(|i| i.event).collect()
    }

    fn hold(j: &crate::Jolta, calendar: CalendarId, remote: &str, events: &[EventData]) {
        j.store.write(|tx| store::apply_item(tx, calendar, remote, None, events, None, &Horizon::around(Utc::now(), Brussels)).map(|_| ())).unwrap();
    }

    fn added(j: &crate::Jolta, calendar: CalendarId, uid: &str, remote: &str, raw: &str) {
        let sent = invitation::find(raw.as_bytes(), &[ME.into()]).remove(0);
        let organizer = sent.event.organizer.as_ref().expect("an organizer").address.clone();
        let copy = AddedCopy { calendar_id: calendar, remote_id: remote.into(), organizer, written: written_after(&[], &sent, Brussels) };
        j.store.write(|tx| store::remember_added(tx, AccountId(1), uid, &copy)).unwrap();
    }

    #[test]
    fn add_is_offered_on_the_organizers_request_until_a_copy_exists() {
        let (j, personal, _team, side) = folder("add-offered");
        let v1 = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        let id = import(&j, v1.as_bytes());
        let c = card_of(&j, id, &v1);
        assert_eq!((c.adding, c.updating), (Some(Adding { message: id, uid: "sync".into() }), None), "iCloud's calendar is off: nothing holds it");

        let fake = version("f", "sync", "REQUEST", 1, "20261104T083000Z", V1).replace("From: ann@example.com", "From: mallory@example.com");
        let fake_id = import(&j, fake.as_bytes());
        assert_eq!(card_of(&j, fake_id, &fake).adding, None, "a request that isn't from the organizer is never added");

        // Another account's own copy isn't the receiving account's: Add stays,
        // but not into the calendar that holds it, and goes once no calendar
        // is left to add to.
        hold(&j, personal, "theirs", &events_of(&v1));
        assert_eq!(card_of(&j, id, &v1).adding, None, "Personal holds it, Team is read-only and Side hidden");
        j.store.write(|tx| store::set_calendar_shown(tx, side, true)).unwrap();
        assert!(card_of(&j, id, &v1).adding.is_some());
        assert!(j.store.read(|c| choices(c, "sync", Some("ann@example.com"))).unwrap().iter().all(|c| c.calendar != personal));

        added(&j, personal, "sync", "theirs", &v1);
        let c = card_of(&j, id, &v1);
        assert_eq!((c.adding, c.updating), (None, None), "added: no second copy, and nothing newer to write");

        let v2 = version("v2", "sync", "REQUEST", 1, "20261104T090000Z", &V1.replace("T150000Z", "T130000Z").replace("T160000Z", "T140000Z"));
        let v2_id = import(&j, v2.as_bytes());
        let c = card_of(&j, v2_id, &v2);
        assert_eq!(c.updating, Some(Updating { message: v2_id, uid: "sync".into(), calendar: "Personal".into() }));
        assert_eq!(card_of(&j, id, &v1).updating, None, "the older request points to the newer one");

        added(&j, personal, "sync", "theirs", &v2);
        assert_eq!(card_of(&j, v2_id, &v2).updating, None, "written: a second Update has nothing to do");

        j.store.write(|tx| store::remove_item(tx, personal, "theirs", &Horizon::around(Utc::now(), Brussels)).map(|_| ())).unwrap();
        assert!(card_of(&j, v2_id, &v2).adding.is_some(), "the copy was deleted in the calendar: Add again");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn the_menu_offers_shown_calendars_jolta_can_write_without_the_meeting() {
        let (j, personal, team, side) = folder("add-menu");
        let names = |j: &crate::Jolta| j.store.read(|c| choices(c, "sync", Some("ann@example.com"))).unwrap().into_iter().map(|c| (c.name, c.default)).collect::<Vec<_>>();
        assert_eq!(names(&j), [("Personal".to_string(), true)], "Team is read-only, Side hidden");
        j.store.write(|tx| store::set_calendar_shown(tx, side, true).map(|_| ())).unwrap();
        assert_eq!(names(&j), [("Personal".to_string(), true), ("Side".to_string(), false)], "the default calendar first chosen");
        j.store.write(|tx| store::set_last_added_calendar(tx, side)).unwrap();
        assert_eq!(names(&j), [("Personal".to_string(), false), ("Side".to_string(), true)], "then the one last added to");
        j.store.write(|tx| store::set_last_added_calendar(tx, team)).unwrap();
        assert_eq!(names(&j)[0], ("Personal".to_string(), true), "a calendar no longer offered falls back to the default");
        let own = j.store.read(|c| choices(c, "sync", Some(&OTHER.to_uppercase()))).unwrap();
        assert!(own.is_empty(), "a meeting this account organizes is in its calendar already: {own:?}");
        let _ = personal;
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn remove_from_calendar_takes_the_added_copy_whoever_it_names_as_organizer() {
        let (j, personal, team, _side) = folder("add-remove");
        let v1 = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        import(&j, v1.as_bytes());
        // Microsoft 365's copy is the user's own event without guests.
        let mut own = events_of(&v1);
        own[0].organizer = Some(Person { address: OTHER.into(), name: String::new() });
        own[0].attendees.clear();
        hold(&j, personal, "copy", &own);
        let cancel = version("x", "sync", "CANCEL", 1, "20261104T090000Z", V1);
        let id = import(&j, cancel.as_bytes());
        assert_eq!(card_of(&j, id, &cancel).removing, None, "another account's event, not one the user added");

        added(&j, personal, "sync", "copy", &v1);
        assert!(card_of(&j, id, &cancel).removing.is_some(), "the copy the user added is theirs to remove");

        let stranger = version("y", "sync", "CANCEL", 2, "20261104T091000Z", V1).replace("From: ann@example.com", "From: mallory@example.com");
        let stranger_id = import(&j, stranger.as_bytes());
        assert_eq!(card_of(&j, stranger_id, &stranger).removing, None, "a stranger's cancel removes nothing");

        own[0].status = EventStatus::Cancelled;
        hold(&j, personal, "copy", &own);
        assert_eq!(card_of(&j, id, &cancel).removing, None, "already cancelled there");

        hold(&j, team, "copy", &events_of(&v1));
        added(&j, team, "sync", "copy", &v1);
        assert_eq!(card_of(&j, id, &cancel).removing, None, "a calendar that went read-only");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn one_cancelled_occurrence_of_an_added_series_is_removable() {
        let (j, personal, _team, _side) = folder("add-occurrence");
        let series = "DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n";
        let request = version("s", "standup", "REQUEST", 0, "20261104T080000Z", series);
        import(&j, request.as_bytes());
        hold(&j, personal, "standup", &events_of(&request));
        added(&j, personal, "standup", "standup", &request);
        let one = version("o", "standup", "CANCEL", 1, "20261104T090000Z", &format!("RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\n{series}").replace("RRULE:FREQ=WEEKLY\r\n", ""));
        let id = import(&j, one.as_bytes());
        let (rid, day) = card_of(&j, id, &one).removing.expect("the added series holds 17 November").occurrence.unwrap();
        assert_eq!((rid, day.as_str()), (at(17), "Tue 17 Nov"));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The copy is the object Jolta created. Once it is gone, another event
    /// with the same UID in that calendar is no copy the user added.
    #[test]
    fn another_event_with_the_uid_is_not_the_added_copy() {
        let (j, personal, _team, side) = folder("add-replaced");
        let v1 = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        import(&j, v1.as_bytes());
        let mut other = events_of(&v1);
        other[0].status = EventStatus::Cancelled;
        hold(&j, personal, "other", &other);
        hold(&j, personal, "theirs", &events_of(&v1));
        added(&j, personal, "sync", "theirs", &v1);
        let cancel = version("x", "sync", "CANCEL", 1, "20261104T090000Z", V1);
        let cancel_id = import(&j, cancel.as_bytes());
        assert!(card_of(&j, cancel_id, &cancel).removing.is_some(), "the copy Jolta made holds it, whatever the other event says");

        j.store.write(|tx| store::remove_item(tx, personal, "theirs", &Horizon::around(Utc::now(), Brussels)).map(|_| ())).unwrap();
        hold(&j, personal, "other", &events_of(&v1));
        assert_eq!(card_of(&j, cancel_id, &cancel).removing, None, "nothing the user added is left to remove");
        let v2 = version("v2", "sync", "REQUEST", 1, "20261104T091000Z", &V1.replace("T150000Z", "T130000Z"));
        let v2_id = import(&j, v2.as_bytes());
        let c = card_of(&j, v2_id, &v2);
        assert_eq!((c.updating, c.adding), (None, None), "Personal holds the UID, Team is read-only and Side hidden");
        j.store.write(|tx| store::set_calendar_shown(tx, side, true)).unwrap();
        assert!(card_of(&j, v2_id, &v2).adding.is_some(), "the user can add it again");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A copy added to a calendar of the account the invitation came to, as
    /// Microsoft 365 holds one: the user's own event without guests. It is
    /// no copy of the account's own: Ann stays the organizer, the answer goes
    /// by mail, and the card and the list show the answer the mail gave.
    #[test]
    fn a_copy_added_to_the_receiving_accounts_own_calendar_is_only_the_added_copy() {
        let (j, own) = jolta("add-own-calendar");
        let v1 = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        let id = import(&j, v1.as_bytes());
        let mut copy = events_of(&v1);
        copy[0].organizer = Some(Person { address: ME.into(), name: String::new() });
        copy[0].attendees.clear();
        hold(&j, own, "copy", &copy);
        added(&j, own, "sync", "copy", &v1);
        let given = store::GivenAnswer { answer: Response::Accepted, note: String::new(), notify: true };
        j.store.write(|tx| store::remember_answer(tx, AccountId(1), "sync", None, &given, &events_of(&v1), None)).unwrap();

        let routed = j.store.read(|c| store::stored_event(c, "sync", None, Some(AccountId(1)), Brussels)).unwrap();
        assert_eq!(routed, None, "an answer finds no copy of the account's own to answer through, so it goes by mail");
        let c = card_of(&j, id, &v1);
        assert!(c.answering.is_some(), "Ann still organizes it: {:?}", c.notes);
        assert_eq!(c.answer, Some(Response::Accepted), "the answer the mail gave");
        let thread = j.store.read(|c| Ok(c.query_row("SELECT thread_id FROM message WHERE id = ?1", [id], |r| r.get::<_, crate::model::ThreadId>(0))?)).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap();
        let listed = j.store.read(|c| store::thread_invitations(c, &[thread], now, Brussels)).unwrap();
        assert_eq!(listed.get(&thread), Some(&store::InvitationState::Answered(Response::Accepted)));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The organizer the copy was added from stays the meeting's, also once
    /// the mail it came in is gone: a stranger reusing the UID at a higher
    /// SEQUENCE changes and removes nothing.
    #[test]
    fn the_added_copy_keeps_the_organizer_it_was_added_from() {
        let (j, personal, _team, _side) = folder("add-organizer");
        let v1 = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        let first = import(&j, v1.as_bytes());
        hold(&j, personal, "theirs", &events_of(&v1));
        added(&j, personal, "sync", "theirs", &v1);
        j.store.write(|tx| store::delete_messages(tx, &[first])).unwrap();

        let mallory = |raw: String| raw.replace("ann@example.com", "mallory@example.com").replace("Ann Dupont", "Mallory");
        let moved = V1.replace("T150000Z", "T130000Z").replace("T160000Z", "T140000Z");
        let fake = mallory(version("f", "sync", "REQUEST", 5, "20261104T090000Z", &moved));
        let fake_id = import(&j, fake.as_bytes());
        let c = card_of(&j, fake_id, &fake);
        assert_eq!((c.answering.is_some(), c.updating), (false, None), "not the organizer the copy was added from");
        let cancel = mallory(version("x", "sync", "CANCEL", 6, "20261104T091000Z", V1));
        let cancel_id = import(&j, cancel.as_bytes());
        assert_eq!(card_of(&j, cancel_id, &cancel).removing, None);

        let v2 = version("v2", "sync", "REQUEST", 1, "20261104T092000Z", &moved);
        let v2_id = import(&j, v2.as_bytes());
        assert!(card_of(&j, v2_id, &v2).updating.is_some(), "the organizer's own update still is one");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// An answer by mail follows into the copy as the invitation scoped it.
    #[test]
    fn an_answer_follows_into_the_added_occurrence_or_series() {
        let (j, personal, _team, _side) = folder("add-follow");
        let series = "DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n";
        let target = |uid: &str, rid: Option<Moment>| j.store.read(|c| copy_target(c, AccountId(1), uid, rid.as_ref(), Brussels)).unwrap().map(|(_, scope, _)| scope);

        // CalDAV keeps an invitation to one occurrence as that occurrence
        // alone, without a master.
        let lines = format!("RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\n{series}").replace("RRULE:FREQ=WEEKLY\r\n", "");
        let one = version("o", "one", "REQUEST", 0, "20261104T080000Z", &lines);
        hold(&j, personal, "one.ics", &events_of(&one));
        added(&j, personal, "one", "one.ics", &one);
        assert_eq!(target("one", Some(at(17))), Some(Scope::Occurrence(at(17))), "the occurrence the copy holds alone");

        let request = version("s", "standup", "REQUEST", 0, "20261104T080000Z", series);
        let mut events = events_of(&request);
        let mut moved = events[0].clone();
        (moved.recurrence, moved.recurrence_id) = (None, Some(at(17)));
        events.push(moved);
        hold(&j, personal, "standup", &events);
        added(&j, personal, "standup", "standup", &request);
        assert_eq!(target("standup", Some(at(17))), Some(Scope::Occurrence(at(17))), "a changed occurrence answered alone stays alone");
        assert_eq!(target("standup", Some(at(24))), Some(Scope::Occurrence(at(24))));
        assert_eq!(target("standup", None), Some(Scope::Series));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Update, an answer that follows and Remove from calendar go to the
    /// object Jolta created, also while another event in that calendar has
    /// its UID and was stored first.
    #[tokio::test]
    async fn the_copy_is_written_only_through_the_object_jolta_created() {
        use crate::calendar::sources::Source;
        use crate::calendar::sources::caldav::tests::caldav_server;
        let (j, personal, _team, _side) = folder("add-object");
        let v1 = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        import(&j, v1.as_bytes());
        let text = invitation::calendar_text(v1.as_bytes(), "sync").unwrap();
        let blob = j.blobs.put(text.as_bytes()).unwrap();
        let events = events_of(&v1);
        for remote in ["other", "theirs"] {
            j.store.write(|tx| store::apply_item(tx, personal, remote, Some("\"1\""), &events, Some(&blob), &Horizon::around(Utc::now(), Brussels)).map(|_| ())).unwrap();
        }
        added(&j, personal, "sync", "theirs", &v1);
        let copy = NewCopy { uid: "sync", text: &text, events: &events, me: ME, answer: None };

        let (_, mut target, event, scope) = added_target(&j, AccountId(1), "sync", None).await.unwrap().expect("the copy");
        target.me = ME.into();
        let (dav, calendar, server) = caldav_server(vec![("204 No Content", Some("\"2\""), None)]).await;
        assert!(matches!(Source::CalDav(dav).replace(&calendar, &target, &[], &copy).await.unwrap(), Replaced::Written(_)));
        assert_eq!(server.await.unwrap(), ["PUT /cal/theirs If-Match \"1\""], "Update");

        let (dav, calendar, server) = caldav_server(vec![("204 No Content", Some("\"2\""), None)]).await;
        let rsvp = Rsvp { event, answer: Answer::Accept, note: None, notify: false, scope };
        Source::CalDav(dav).follow(&calendar, &target, &rsvp).await.unwrap();
        assert_eq!(server.await.unwrap(), ["PUT /cal/theirs If-Match \"1\""], "an answer that follows");

        let (_, target, added) = super::super::remove::target(&j, AccountId(1), "sync", None).await.unwrap();
        assert!(added);
        let (dav, calendar, server) = caldav_server(vec![("412 Precondition Failed", None, None)]).await;
        Source::CalDav(dav).remove(&calendar, &target).await.unwrap();
        assert_eq!(server.await.unwrap(), ["PUT /cal/theirs If-Match \"1\""], "Remove from calendar");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The object and the changed occurrence an answer or an update for
    /// the 17th goes to in the copy added of `uid`, and the changed
    /// occurrences an update holds Google to.
    async fn on_the_17th(j: &crate::Jolta, uid: &str) -> (String, Option<String>, Vec<String>) {
        let (_, target, _, _) = added_target(j, AccountId(1), uid, Some(at(17))).await.unwrap().expect("the copy");
        let (event, object) = (target.event.id, target.event.remote_id.clone());
        let held = j.store.read(|c| rsvp::changed_occurrences(c, event, &object)).unwrap();
        (object, target.occurrence.and_then(|o| o.stored).map(|s| s.remote_id), held.into_iter().map(|s| s.remote_id).collect())
    }

    /// A changed occurrence is part of the added copy only while it belongs
    /// to the object Jolta created: in its CalDAV resource, or filed under it
    /// by Google or Microsoft 365. Another event in that calendar with the
    /// UID, stored first, lends the copy none of its occurrences.
    #[tokio::test]
    async fn only_the_objects_own_occurrences_are_part_of_the_added_copy() {
        let (j, personal, _team, _side) = folder("add-own-occurrences");
        let series = "DTSTART;TZID=Europe/Brussels:20261110T093000\r\nDTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n";
        let store_as = |remote: &str, events: &[EventData]| {
            j.store.write(|tx| store::apply_item(tx, personal, remote, None, events, None, &Horizon::around(Utc::now(), Brussels)).map(|_| ())).unwrap()
        };
        // CalDAV, Google and Microsoft 365: the other event's series and its
        // changed 17th, the copy's series, then the copy's own changed 17th.
        let shapes = [
            ("dav", ["other.ics", "other.ics", "theirs.ics", "theirs.ics"], [None, None]),
            ("google", ["other", "other_20261117T083000Z", "theirs", "theirs_20261117T083000Z"], [Some("other"), Some("theirs")]),
            ("graph", ["M-other", "X-other", "M-theirs", "X-theirs"], [Some("M-other"), Some("M-theirs")]),
        ];
        for (uid, [other, other_moved, theirs, theirs_moved], [other_master, theirs_master]) in shapes {
            let request = version(uid, uid, "REQUEST", 0, "20261104T080000Z", series);
            let master = events_of(&request).remove(0);
            let moved = |master_id: Option<&str>| EventData { recurrence: None, recurrence_id: Some(at(17)), master_id: master_id.map(String::from), ..master.clone() };
            let both = |master_id| if other == other_moved { vec![master.clone(), moved(master_id)] } else { vec![moved(master_id)] };
            if other == other_moved {
                store_as(other, &both(other_master));
                store_as(theirs, &[master.clone()]);
            } else {
                store_as(other, &[master.clone()]);
                store_as(other_moved, &both(other_master));
                store_as(theirs, &[master.clone()]);
            }
            added(&j, personal, uid, theirs, &request);
            assert_eq!(on_the_17th(&j, uid).await, (theirs.to_string(), None, vec![]), "{uid}: the copy's 17th is unchanged");

            store_as(theirs_moved, &both(theirs_master));
            assert_eq!(on_the_17th(&j, uid).await, (theirs.to_string(), Some(theirs_moved.to_string()), vec![theirs_moved.to_string()]), "{uid}: its own changed 17th");
        }
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// An occurrence the user added alone is the object Jolta created, also
    /// while another event in that calendar holds that occurrence alone and
    /// was stored first.
    #[tokio::test]
    async fn an_occurrence_added_alone_is_written_only_as_itself() {
        use crate::calendar::sources::Source;
        use crate::calendar::sources::caldav::tests::caldav_server;
        let (j, personal, _team, _side) = folder("add-alone");
        let lines = "RECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\nDTSTART;TZID=Europe/Brussels:20261117T093000\r\nDTEND;TZID=Europe/Brussels:20261117T094500\r\nSUMMARY:Standup\r\n";
        let one = version("o", "one", "REQUEST", 0, "20261104T080000Z", lines);
        import(&j, one.as_bytes());
        let text = invitation::calendar_text(one.as_bytes(), "one").unwrap();
        let blob = j.blobs.put(text.as_bytes()).unwrap();
        for remote in ["other.ics", "theirs.ics"] {
            j.store.write(|tx| store::apply_item(tx, personal, remote, Some("\"1\""), &events_of(&one), Some(&blob), &Horizon::around(Utc::now(), Brussels)).map(|_| ())).unwrap();
        }
        added(&j, personal, "one", "theirs.ics", &one);

        let (_, mut target, event, scope) = added_target(&j, AccountId(1), "one", Some(at(17))).await.unwrap().expect("the copy");
        assert_eq!(target.event.remote_id, "theirs.ics");
        target.me = ME.into();
        let (dav, calendar, server) = caldav_server(vec![("204 No Content", Some("\"2\""), None)]).await;
        let rsvp = Rsvp { event, answer: Answer::Accept, note: None, notify: false, scope };
        Source::CalDav(dav).follow(&calendar, &target, &rsvp).await.unwrap();
        assert_eq!(server.await.unwrap(), ["PUT /cal/theirs.ics If-Match \"1\""], "an answer that follows");

        let (_, target, added) = super::super::remove::target(&j, AccountId(1), "one", Some(at(17))).await.unwrap();
        assert!(added);
        assert_eq!(target.event.remote_id, "theirs.ics", "Remove from calendar");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    fn at(day: u32) -> Moment {
        Moment::Time(Local { wall: NaiveDate::from_ymd_opt(2026, 11, day).unwrap().and_hms_opt(9, 30, 0).unwrap(), zone: Zone::Tz(Brussels) })
    }

    fn written(rid: Option<Moment>, sequence: i64, minute: u32) -> WrittenVersion {
        WrittenVersion { recurrence_id: rid, sequence, stamp: Some(Utc.with_ymd_and_hms(2026, 11, 4, 9, minute, 0).unwrap()) }
    }

    fn v(sequence: i64, minute: u32) -> Version {
        Version { sequence, stamp: Some(Utc.with_ymd_and_hms(2026, 11, 4, 9, minute, 0).unwrap()) }
    }

    #[test]
    fn only_a_newer_version_than_the_one_written_asks_for_an_update() {
        let written = vec![written(None, 1, 0)];
        assert!(!newer(&written, None, v(1, 0), Brussels), "the version added");
        assert!(!newer(&written, None, v(0, 30), Brussels), "an older one");
        assert!(newer(&written, None, v(1, 5), Brussels), "the same SEQUENCE stamped later");
        assert!(newer(&written, None, v(2, 0), Brussels));
        assert!(!newer(&written, Some(&at(17)), v(1, 0), Brussels), "an occurrence sent with the series' version is in the copy");
        assert!(newer(&written, Some(&at(17)), v(2, 0), Brussels), "a newer occurrence");
        assert!(newer(&[], None, v(0, 0), Brussels), "nothing written yet");
    }

    /// Graph refused a changed occurrence while the copy was created: the
    /// copy records no version, so the card keeps offering Update.
    #[test]
    fn a_copy_added_only_in_part_keeps_offering_update() {
        let sent = Invitation {
            method: Method::Request,
            event: EventData { sequence: 2, ..EventData::new("u", Span::AllDay { start: NaiveDate::MIN, end: NaiveDate::MIN }) },
            dated: true,
            stamp: Some(Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()),
        };
        let complete = written_on_add(true, &sent, Brussels);
        assert!(!newer(&complete, None, Version::of(&sent), Brussels), "a whole copy holds the version it was added from");
        let partial = written_on_add(false, &sent, Brussels);
        assert!(newer(&partial, None, Version::of(&sent), Brussels), "a partial copy still offers that version as an update");
    }

    #[test]
    fn an_occurrence_update_keeps_what_the_series_and_other_occurrences_had() {
        let invitation = |rid: Option<Moment>, sequence: i64| Invitation {
            method: Method::Request,
            event: EventData { recurrence_id: rid, sequence, ..EventData::new("u", Span::AllDay { start: NaiveDate::MIN, end: NaiveDate::MIN }) },
            dated: true,
            stamp: Some(Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()),
        };
        let utc = Moment::Time(Local { wall: NaiveDate::from_ymd_opt(2026, 11, 17).unwrap().and_hms_opt(8, 30, 0).unwrap(), zone: Zone::Tz(chrono_tz::UTC) });
        let before = vec![written(None, 1, 0), written(Some(at(17)), 2, 0), written(Some(at(24)), 2, 0)];
        let after = written_after(&before, &invitation(Some(utc), 3), Brussels);
        assert_eq!(after.len(), 3, "the moment written in UTC replaces 17 November: {after:?}");
        assert!(after.contains(&written(None, 1, 0)) && after.contains(&written(Some(at(24)), 2, 0)));
        assert_eq!(written_after(&after, &invitation(None, 4), Brussels).len(), 1, "a series update replaces them all");
    }
}
