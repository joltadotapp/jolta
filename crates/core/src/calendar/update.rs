//! Updates to an invitation: the order of its versions, the version the
//! user had before the one in a message, and whether an answer given to an
//! earlier version still holds.
//!
//! An organizer's update arrives as a new invitation mail with the same UID
//! and a higher SEQUENCE, or the same SEQUENCE and a later DTSTAMP (RFC 5546
//! section 2.1.5). The user has earlier versions as the mails before it and
//! as the copy in the calendars of the account the invitation came to.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;

use super::invitation::{self, Invitation, Method};
use super::{CalendarSource, EventData, EventStatus, Moment, recur, series};
use crate::blobs::Blobs;
use crate::model::{AccountId, MessageId};
use crate::{Result, store};

/// Where one version stands: SEQUENCE first, then DTSTAMP. A version
/// without a DTSTAMP, such as a calendar's copy, sorts before one with it
/// at the same SEQUENCE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub sequence: i64,
    pub stamp: Option<DateTime<Utc>>,
}

impl Version {
    pub fn of(i: &Invitation) -> Version {
        Version { sequence: i.event.sequence, stamp: i.stamp }
    }
}

/// Whether an answer given to `answered` still holds for `now`, a version
/// of the same event or occurrence. It holds for the same version or a
/// later one, and across an update that kept the time, the date and the
/// repeat rule: a new place, title or description doesn't ask again, as
/// Google and Microsoft 365 don't. Graph keeps no SEQUENCE (every copy reads
/// 0), so its copies hold by content alone.
pub fn answer_holds(answered: &EventData, source: Option<CalendarSource>, now: &EventData, here: Tz) -> bool {
    (answered.sequence >= now.sequence && source != Some(CalendarSource::Graph)) || same_time(answered, now, here)
}

/// The version of an event a reply mail answered. The reply carries its
/// SEQUENCE, start and end; for a series the repeat rule comes from the
/// account's request it answered, the latest at that SEQUENCE sent before
/// the reply. Without that request the rule counts as kept.
pub fn answered_by(seen: &Seen, reply: &Invitation, now: &EventData) -> Result<EventData> {
    let r = &reply.event;
    let recurrence = match r.recurrence_id {
        Some(_) => None,
        None => {
            let at = Version { sequence: r.sequence, stamp: None };
            let before_reply = |v: &Version| v.sequence == r.sequence && reply.stamp.is_none_or(|sent| v.stamp.is_none_or(|s| s <= sent));
            let request = mails(seen, organizer(seen, &r.uid)?.as_deref(), &r.uid, None, at)?
                .into_iter()
                .filter(|(method, _, v)| *method == Method::Request && before_reply(v))
                .max_by_key(|(_, _, v)| *v)
                .and_then(|(_, events, _)| events.into_iter().find(|e| e.recurrence_id.is_none()));
            match request {
                Some(request) => request.recurrence,
                None => now.recurrence.clone(),
            }
        }
    };
    Ok(EventData { recurrence, ..r.clone() })
}

/// The same start, end and repeat rule. EXDATEs don't count: an
/// occurrence cancelled doesn't move the others.
pub fn same_time(a: &EventData, b: &EventData, here: Tz) -> bool {
    a.span.bounds(here) == b.span.bounds(here) && recur::same_repeat(a.recurrence.as_ref(), &a.span, b.recurrence.as_ref(), &b.span, here)
}

/// What the user has of an invitation's other versions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    /// The method of the newest version of the same event or occurrence
    /// when it replaces this one: another mail, a cancellation of the whole
    /// series, or the account's calendar copy at a higher SEQUENCE or
    /// cancelled.
    pub newer: Option<Method>,
    /// The version before this one: the events of the latest earlier
    /// request, or the account's calendar copy while it is still older than
    /// this one. For an invitation to one occurrence without an earlier one
    /// for it, that occurrence as its series had it. Empty for a first
    /// invitation.
    pub before: Vec<EventData>,
    /// The message names another organizer than the meeting has, or comes
    /// from someone else than the organizer it names. It is no version of
    /// the meeting, and the other mails are none of its versions.
    pub not_from_organizer: bool,
}

/// Where the card's message stands among the versions of its event.
pub struct Seen<'a> {
    pub conn: &'a Connection,
    pub blobs: &'a Blobs,
    pub account: AccountId,
    pub message: MessageId,
    /// The message's From address.
    pub sender: &'a str,
    /// The user's addresses, for reading other messages.
    pub me: &'a [String],
    pub here: Tz,
}

/// Addresses that send invitations on an organizer's behalf. Google sends
/// from the organizer's own address with its own as Sender, and from its
/// own when it can't. iCloud sends from its own, naming the organizer by an
/// `imip.me.com` relay address (seen on the iCloud test account's mail,
/// 2026-11-04).
const SENDS_FOR_ORGANIZERS: &[&str] = &["calendar-notification@google.com", "noreply@email.apple.com"];

/// The meeting's organizer as the account knows it: on its calendar's
/// copy, else on the earliest request for the UID that its own organizer
/// sent. None for a meeting the account has no request or copy of.
fn organizer(seen: &Seen, uid: &str) -> Result<Option<String>> {
    let copy = store::stored_series(seen.conn, uid, Some(seen.account))?;
    if let Some(o) = copy.iter().find_map(|e| e.organizer.as_ref()).filter(|o| !o.address.trim().is_empty()) {
        return Ok(Some(o.address.clone()));
    }
    let first = store::request_senders(seen.conn, seen.account, uid)?.into_iter().find(|(o, from)| sent_by(seen.me, None, o, from));
    Ok(first.map(|(o, _)| o))
}

/// Whether a message naming `organizer`, From `from`, is the organizer's:
/// it names the meeting's organizer, when there is one, and comes from that
/// address, from a provider sending for it, or for the user's own meeting
/// from another of the user's addresses.
fn sent_by(me: &[String], meeting: Option<&str>, organizer: &str, from: &str) -> bool {
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    let mine = |a: &str| me.iter().any(|m| same(m, a));
    if organizer.trim().is_empty() || meeting.is_some_and(|m| !same(m, organizer)) {
        return false;
    }
    same(organizer, from) || SENDS_FOR_ORGANIZERS.iter().any(|s| same(s, from)) || (mine(organizer) && mine(from))
}

/// The other versions of `sent`, the event a message names, and which of
/// them came before it. Other mails count only on the account the message
/// came to, and only from the meeting's organizer.
pub fn history(seen: &Seen, sent: &Invitation) -> Result<History> {
    let this = Version::of(sent);
    let uid = &sent.event.uid;
    let rid = sent.event.recurrence_id;
    let organizer = organizer(seen, uid)?;
    let organizers = matches!(sent.method, Method::Request | Method::Add | Method::Cancel | Method::DeclineCounter);
    // Without an ORGANIZER nobody vouches for the message.
    if organizers && !sent.event.organizer.as_ref().is_some_and(|named| sent_by(seen.me, organizer.as_deref(), &named.address, seen.sender)) {
        return Ok(History { not_from_organizer: true, ..History::default() });
    }

    let mut versions = mails(seen, organizer.as_deref(), uid, rid.as_ref(), this)?;
    if rid.is_some() {
        let series = mails(seen, organizer.as_deref(), uid, None, this)?;
        versions.extend(series.into_iter().filter(|(method, ..)| *method == Method::Cancel));
    }
    let mut newest: Option<(Version, Method)> = None;
    let mut before: Option<(Version, Vec<EventData>)> = None;
    for (method, events, version) in versions {
        if version > this && newest.is_none_or(|(v, _)| version > v) {
            newest = Some((version, method));
        } else if version < this && method != Method::Cancel && before.as_ref().is_none_or(|(v, _)| version > *v) {
            before = Some((version, events));
        }
    }

    let copy = store::stored_event(seen.conn, uid, rid.as_ref(), Some(seen.account), seen.here)?;
    if let Some(copy) = &copy {
        let source = store::calendar(seen.conn, copy.calendar_id)?.source;
        let cancelled = copy.event.status == EventStatus::Cancelled;
        // A cancelled copy took a cancellation, this one or a later one.
        // Graph keeps no SEQUENCE: its copy is newer only once cancelled.
        let newer_copy = match source {
            _ if cancelled && sent.method == Method::Cancel => false,
            CalendarSource::Graph => cancelled,
            _ => copy.event.sequence > this.sequence || (cancelled && copy.event.sequence >= this.sequence),
        };
        if newer_copy {
            let version = Version { sequence: copy.event.sequence, stamp: None };
            newest = match newest {
                _ if cancelled => Some((version, Method::Cancel)),
                Some(mail) => Some(mail),
                None => Some((version, Method::Request)),
            };
        // Behind a newer mail the copy may be that one: Graph keeps no
        // SEQUENCE to tell.
        } else if newest.is_none() && older_copy(&copy.event, source, &sent.event, seen.here) && before.as_ref().is_none_or(|(v, _)| v.sequence < copy.event.sequence) {
            let version = Version { sequence: copy.event.sequence, stamp: None };
            let events = match rid {
                Some(_) => vec![copy.event.clone()],
                None => store::stored_series(seen.conn, uid, Some(seen.account))?,
            };
            before = Some((version, events));
        }
    }
    // A series the calendar holds cancelled cancels each of its occurrences.
    if rid.is_some()
        && sent.method != Method::Cancel
        && let Some(master) = store::stored_event(seen.conn, uid, None, Some(seen.account), seen.here)?
        && master.event.status == EventStatus::Cancelled
    {
        newest = Some((Version { sequence: master.event.sequence, stamp: None }, Method::Cancel));
    }

    // One occurrence moved for the first time: the series said where it was.
    if let (None, Some(rid)) = (&before, rid) {
        before = occurrence_before(seen, organizer.as_deref(), uid, &rid)?.map(|e| (Version { sequence: e.sequence, stamp: None }, vec![e]));
    }
    Ok(History { newer: newest.map(|(_, method)| method), before: before.map(|(_, events)| events).unwrap_or_default(), not_from_organizer: false })
}

/// A copy still at an earlier version than the mail: a lower SEQUENCE, or
/// on Graph, which keeps none, another time.
fn older_copy(copy: &EventData, source: CalendarSource, sent: &EventData, here: Tz) -> bool {
    match source {
        CalendarSource::Graph => !same_time(copy, sent, here),
        _ => copy.sequence < sent.sequence,
    }
}

/// The occurrence `rid` of the series as an earlier version had it: from
/// the latest request for the whole series, else the account's copy of the
/// series, its changed occurrence or its master moved there.
fn occurrence_before(seen: &Seen, organizer: Option<&str>, uid: &str, rid: &Moment) -> Result<Option<EventData>> {
    let mut series_mails = mails(seen, organizer, uid, None, Version { sequence: i64::MAX, stamp: None })?;
    series_mails.retain(|(method, ..)| *method != Method::Cancel);
    let latest = series_mails.into_iter().max_by_key(|(_, _, v)| *v).map(|(_, events, _)| events);
    let events = match latest {
        Some(events) => events,
        None => store::stored_series(seen.conn, uid, Some(seen.account))?,
    };
    if let Some(changed) = events.iter().find(|e| e.recurrence_id.is_some_and(|r| r.names(rid, seen.here))) {
        return Ok(Some(changed.clone()));
    }
    let master = events.iter().find(|e| e.recurrence_id.is_none() && e.recurrence.is_some());
    Ok(master.and_then(|m| {
        let span = series::occurrence_span(&m.span, rid, seen.here)?;
        Some(EventData { recurrence_id: Some(*rid), recurrence: None, span, ..m.clone() })
    }))
}

/// The requests, additions and cancellations for `uid` and `rid` in the
/// account's other messages from the meeting's `organizer`, each with the
/// events of that UID it carries and its version, the 20 highest SEQUENCEs.
/// A message with a higher SEQUENCE than `this` is newer without reading it.
fn mails(seen: &Seen, organizer: Option<&str>, uid: &str, rid: Option<&Moment>, this: Version) -> Result<Vec<(Method, Vec<EventData>, Version)>> {
    let mut out = Vec::new();
    let rows = store::invitation_versions(seen.conn, seen.account, uid, rid, seen.message, seen.here)?;
    for row in rows.into_iter().filter(|r| sent_by(seen.me, organizer, &r.organizer, &r.from)).take(20) {
        if row.sequence > this.sequence {
            out.push((row.method, Vec::new(), Version { sequence: row.sequence, stamp: None }));
            continue;
        }
        let Some(raw) = row.blob.and_then(|hash| seen.blobs.get(&hash).ok()) else { continue };
        let found = invitation::find(&raw, seen.me);
        let names = |i: &&Invitation| {
            i.event.uid == uid
                && match (&i.event.recurrence_id, rid) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.names(b, seen.here),
                    _ => false,
                }
        };
        let Some(named) = found.iter().find(names) else { continue };
        let version = Version::of(named);
        let events = found.iter().filter(|i| i.event.uid == uid).map(|i| i.event.clone()).collect();
        out.push((row.method, events, version));
    }
    Ok(out)
}
