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
use super::{CalendarSource, EventData, Moment, series};
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

/// Whether an answer still holds for `now` when the reply mail that sent it
/// is all Jolta has of the version answered: the reply carries that
/// version's SEQUENCE, start and end, not its repeat rule.
pub fn reply_holds(reply: &EventData, now: &EventData, here: Tz) -> bool {
    reply.sequence >= now.sequence || reply.span.bounds(here) == now.span.bounds(here)
}

/// The same start, end and repeat rule. EXDATEs don't count: an
/// occurrence cancelled doesn't move the others.
pub fn same_time(a: &EventData, b: &EventData, here: Tz) -> bool {
    let rule = |e: &EventData| e.recurrence.as_ref().map(|r| (r.rules.clone(), r.rdates.clone()));
    a.span.bounds(here) == b.span.bounds(here) && rule(a) == rule(b)
}

/// What the user has of an invitation's other versions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    /// The method of a newer version of the same event or occurrence, which
    /// replaces this one: another mail, or the account's calendar copy at a
    /// higher SEQUENCE and another time.
    pub newer: Option<Method>,
    /// The version before this one: the events of the latest earlier
    /// request, or the account's calendar copy while it is still older than
    /// this one. For an invitation to one occurrence without an earlier one
    /// for it, that occurrence as its series had it. Empty for a first
    /// invitation.
    pub before: Vec<EventData>,
}

/// Where the card's message stands among the versions of its event.
pub struct Seen<'a> {
    pub conn: &'a Connection,
    pub blobs: &'a Blobs,
    pub account: AccountId,
    pub message: MessageId,
    /// The user's addresses, for reading other messages.
    pub me: &'a [String],
    pub here: Tz,
}

/// The other versions of `sent`, the event a message names, and which of
/// them came before it. Other mails count only on the account the message
/// came to.
pub fn history(seen: &Seen, sent: &Invitation) -> Result<History> {
    let this = Version::of(sent);
    let uid = &sent.event.uid;
    let rid = sent.event.recurrence_id;
    let mut newer = None;
    let mut before: Option<(Version, Vec<EventData>)> = None;
    for (method, events, version) in mails(seen, uid, rid.as_ref(), this)? {
        if version > this {
            newer = newer.or(Some(method));
        } else if version < this && method != Method::Cancel && before.as_ref().is_none_or(|(v, _)| version > *v) {
            before = Some((version, events));
        }
    }

    let copy = store::stored_event(seen.conn, uid, rid.as_ref(), Some(seen.account), seen.here)?;
    if let Some(copy) = &copy {
        let source = store::calendar(seen.conn, copy.calendar_id)?.source;
        let moved = !same_time(&copy.event, &sent.event, seen.here);
        if copy.event.sequence > this.sequence && moved {
            newer = newer.or(Some(Method::Request));
        // Behind a newer mail the copy may be that one: Graph keeps no
        // SEQUENCE to tell.
        } else if newer.is_none() && older_copy(&copy.event, source, &sent.event, seen.here) && before.as_ref().is_none_or(|(v, _)| v.sequence < copy.event.sequence) {
            let version = Version { sequence: copy.event.sequence, stamp: None };
            let events = match rid {
                Some(_) => vec![copy.event.clone()],
                None => store::stored_series(seen.conn, uid, Some(seen.account))?,
            };
            before = Some((version, events));
        }
    }

    // One occurrence moved for the first time: the series said where it was.
    if let (None, Some(rid)) = (&before, rid) {
        before = occurrence_before(seen, uid, &rid)?.map(|e| (Version { sequence: e.sequence, stamp: None }, vec![e]));
    }
    Ok(History { newer, before: before.map(|(_, events)| events).unwrap_or_default() })
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
fn occurrence_before(seen: &Seen, uid: &str, rid: &Moment) -> Result<Option<EventData>> {
    let mut series_mails = mails(seen, uid, None, Version { sequence: i64::MAX, stamp: None })?;
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
/// account's other messages, each with the events of that UID it carries
/// and its version. A message with a higher SEQUENCE than `this` is newer
/// without reading it.
fn mails(seen: &Seen, uid: &str, rid: Option<&Moment>, this: Version) -> Result<Vec<(Method, Vec<EventData>, Version)>> {
    let mut out = Vec::new();
    for row in store::invitation_versions(seen.conn, seen.account, uid, rid, seen.message)? {
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
