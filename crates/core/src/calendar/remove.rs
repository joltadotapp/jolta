//! Remove from calendar: takes a meeting its organizer cancelled out of the
//! user's calendar, from the card on the cancellation mail. The organizer is
//! never told: they cancelled it.
//!
//! Google and Microsoft 365 usually act on the cancellation themselves, so
//! Jolta asks them first and, when they already removed or cancelled the
//! event, only syncs: a second delete could tell the organizer the user
//! declined. A CalDAV copy is deleted with `If-Match` and SCHEDULE-AGENT=CLIENT.

use chrono::Utc;
use rusqlite::OptionalExtension;

use super::card::{self, Card, Removing};
use super::ical_edit::{Component, Object, Prop};
use super::rsvp::{self, Scope};
use super::sources::{Change, Gone};
use super::{Access, Calendar, CalendarId, CalendarSource, EventData, EventStatus, Moment, ical, invitation, sync, tz};
use crate::model::{AccountId, MessageId};
use crate::{Error, Jolta, Result, imap, store};

/// How a calendar's copy of a cancelled meeting leaves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Ask the server whether it removed or cancelled the event already,
    /// and delete it only when it hasn't.
    AskFirst,
    /// Delete it with `If-Match`, with SCHEDULE-AGENT=CLIENT so the server
    /// sends nothing.
    DeleteQuietly,
}

pub fn route(source: CalendarSource) -> Result<Route> {
    match source {
        CalendarSource::Google | CalendarSource::Graph => Ok(Route::AskFirst),
        CalendarSource::CalDav => Ok(Route::DeleteQuietly),
        // The card offers removal only in an account's own calendars.
        CalendarSource::Local => Err(Error::Other("This meeting is in a calendar on this Mac only; delete it there.".into())),
        CalendarSource::Webcal => Err(Error::Other("This meeting is in a subscribed calendar, which is read-only.".into())),
        CalendarSource::EventKit => Err(Error::Other("Remove this meeting in the Calendar app: macOS doesn't let other apps change its invitations.".into())),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removed {
    /// For the toast: "Removed from your calendar."
    pub text: String,
    pub calendar: CalendarId,
}

/// Removes the meeting the cancellation on `message` names from the
/// calendar of the account it came to: the whole event, or the occurrence
/// it cancels.
pub async fn remove(jolta: &Jolta, message: MessageId) -> Result<Removed> {
    let (account, removing) = cancellation(jolta, message).await?;
    let here = tz::here();
    let rid = removing.occurrence.as_ref().map(|(rid, _)| *rid);
    let uid = removing.uid.clone();
    let (calendar, target, added) = target(jolta, account, &uid, rid).await?;

    route(calendar.source)?;
    match added {
        true => added_copy(&calendar)?,
        false => guest_copy(&calendar, &target.event.data, std::slice::from_ref(&target.me))?,
    }
    let source = sync::calendar_source(jolta, &calendar).await?;
    // A CalDAV server knows aliases the copy may not show.
    if !added {
        guest_copy(&calendar, &target.event.data, &source.addresses().await)?;
    }
    let gone = source.remove(&calendar, &target).await?;
    if added && target.occurrence.is_none() && matches!(gone, Gone::Deleted(_) | Gone::Already) {
        jolta.store.write_async(move |tx| store::forget_added(tx, account, &uid)).await?;
    }
    let id = calendar.id;
    match &gone {
        Gone::Deleted(Some(Change::Upsert { remote_id, etag, events, raw })) => {
            let (remote_id, etag, events) = (remote_id.clone(), etag.clone(), events.clone());
            let blob = raw.as_ref().map(|r| jolta.blobs.put(r)).transpose()?;
            let horizon = super::series::Horizon::around(Utc::now(), here);
            jolta.store.write_async(move |tx| store::apply_item(tx, id, &remote_id, etag.as_deref(), &events, blob.as_deref(), &horizon).map(|_| ())).await?;
        }
        Gone::Deleted(_) if target.occurrence.is_none() => {
            let remote_id = target.event.remote_id.clone();
            let horizon = super::series::Horizon::around(Utc::now(), here);
            jolta.store.write_async(move |tx| store::remove_item(tx, id, &remote_id, store::Removal::Deleted, &horizon).map(|_| ())).await?;
        }
        // The server knows what's left of the series.
        Gone::Deleted(_) | Gone::Already => {
            sync::sync_calendar(jolta, id).await?;
        }
        Gone::Changed => {
            sync::sync_calendar(jolta, id).await?;
            return Err(Error::Other(said(&calendar, &gone, None)));
        }
    }
    Ok(Removed { text: said(&calendar, &gone, removing.occurrence.as_ref().map(|(_, day)| day.as_str())), calendar: id })
}

/// Refuses what Remove from calendar leaves alone: a copy in a calendar the
/// account doesn't own, one the user organizes, whose delete cancels the
/// meeting for every guest on Microsoft 365, and one that doesn't list the
/// user as a guest. `mine` holds the account's addresses; the attendees the
/// copy marks as the user's count too.
pub fn guest_copy(calendar: &Calendar, event: &EventData, mine: &[String]) -> Result<()> {
    if calendar.access != Access::Owner {
        return Err(Error::Other("This meeting is in a calendar shared with you, so Jolta leaves it to the calendar's owner.".into()));
    }
    let me: Vec<&str> = mine.iter().map(String::as_str).chain(event.attendees.iter().filter(|a| a.is_me).map(|a| a.person.address.as_str())).collect();
    match &event.organizer {
        Some(o) if !me.iter().any(|m| m.trim().eq_ignore_ascii_case(o.address.trim())) => {}
        Some(_) => return Err(Error::Other("You organize this meeting. Cancel it from your calendar to tell the guests.".into())),
        None => return Err(Error::Other("This event has no organizer, so it isn't a meeting Jolta can remove.".into())),
    }
    if !event.attendees.iter().any(|a| a.is_me) {
        return Err(Error::Other("Your calendar's copy doesn't list you as a guest, so Jolta leaves it alone.".into()));
    }
    Ok(())
}

/// A copy the user put in the calendar with Add to calendar is theirs to
/// remove, whoever the calendar says organizes it: on Microsoft 365 it is
/// the user's own event without guests. It only has to stay writable.
fn added_copy(calendar: &Calendar) -> Result<()> {
    match calendar.access {
        Access::Owner | Access::Write => Ok(()),
        _ => Err(Error::Other("The calendar you added this meeting to is read-only now, so Jolta leaves it alone.".into())),
    }
}

/// The remote id of the object Jolta created when the copy in `calendar`
/// is the one the user added of the invitation `uid` to `account`.
fn added_to(conn: &rusqlite::Connection, account: AccountId, uid: &str, calendar: CalendarId) -> Result<Option<String>> {
    Ok(store::added_copy(conn, account, uid)?.filter(|a| a.calendar_id == calendar).map(|a| a.remote_id))
}

/// The calendar and what removing the meeting `uid`, or its occurrence
/// `rid`, from the calendar of `account` goes to: the account's own copy,
/// else the one the user added, which is the object Jolta created. True
/// for the one added.
pub(super) async fn target(jolta: &Jolta, account: AccountId, uid: &str, rid: Option<Moment>) -> Result<(Calendar, rsvp::Target, bool)> {
    let here = tz::here();
    let (event, object) = jolta
        .store
        .read_async({
            let uid = uid.to_string();
            move |c| {
                let changed = rid.map(|rid| store::own_or_added(c, &uid, Some(&rid), account, here)).transpose()?.flatten();
                let copy = match changed {
                    Some(copy) => copy,
                    None => store::own_or_added(c, &uid, None, account, here)?.ok_or_else(|| Error::NotFound("Your calendar no longer has this meeting.".into()))?,
                };
                Ok((copy.event_id, added_to(c, account, &uid, copy.calendar_id)?))
            }
        })
        .await?;
    let added = object.is_some();
    let (calendar, target) = rsvp::target_of(jolta, event, &rid.map_or(Scope::Series, Scope::Occurrence), object).await?;
    Ok((calendar, target, added))
}

/// Why the account's copy of the meeting `uid`, its own or the one the
/// user added, isn't one to remove: its series or single event, else the
/// occurrence `rid` stored alone. None when it is one, or when there is no
/// copy.
pub(crate) fn refusal(conn: &rusqlite::Connection, uid: &str, rid: Option<&Moment>, account: AccountId, here: chrono_tz::Tz) -> Result<Option<Error>> {
    let copy = match store::own_or_added(conn, uid, None, account, here)? {
        None if rid.is_some() => store::own_or_added(conn, uid, rid, account, here)?,
        found => found,
    };
    let Some(copy) = copy else { return Ok(None) };
    let calendar = store::calendar(conn, copy.calendar_id)?;
    if added_to(conn, account, uid, copy.calendar_id)?.is_some() {
        return Ok(added_copy(&calendar).err());
    }
    let address = store::account(conn, account)?.address;
    Ok(guest_copy(&calendar, &copy.event, &[address]).err())
}

/// "Removed from your calendar.", "Removed Thu 5 Nov from your calendar.",
/// "Google had already removed it from your calendar."
fn said(calendar: &Calendar, gone: &Gone, day: Option<&str>) -> String {
    match (gone, calendar.source) {
        (Gone::Already, CalendarSource::Graph) => "Microsoft 365 had already cancelled it in your calendar.".into(),
        (Gone::Already, CalendarSource::Google) => "Google had already removed it from your calendar.".into(),
        (Gone::Already, _) => "It was already gone from your calendar.".into(),
        (Gone::Changed, _) => "The meeting changed; check it again.".into(),
        (Gone::Deleted(_), _) => match day {
            Some(day) => format!("Removed {day} from your calendar."),
            None => "Removed from your calendar.".into(),
        },
    }
}

/// The cancellation on a message whose meeting the account's calendar still
/// holds, as its card offers it.
async fn cancellation(jolta: &Jolta, message: MessageId) -> Result<(AccountId, Removing)> {
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
            let invitations = invitation::find(&raw, &every);
            let cx = card::Context { message, account, me: &every, sender: &sender, now: Utc::now(), here: tz::here(), blobs: &blobs };
            card::cards(c, &invitations, &cx)?
                .into_iter()
                .find_map(|card| match card {
                    Card::Event(e) => e.removing,
                    Card::Reply(_) => None,
                })
                .map(|r| Ok((account, r)))
                .unwrap_or_else(|| {
                    let cancel = invitations.iter().find(|i| i.method == invitation::Method::Cancel).map(|i| &i.event);
                    let why = match cancel {
                        Some(e) => refusal(c, &e.uid, e.recurrence_id.as_ref(), account, tz::here())?,
                        None => None,
                    };
                    Err(why.unwrap_or_else(|| Error::Other("Your calendar has no copy of this cancelled meeting to remove.".into())))
                })
        })
        .await
}

/// SCHEDULE-AGENT=CLIENT on every ORGANIZER line of the events of `uid`,
/// so a scheduling server sends nothing for what the attendee's copy does
/// next. None when every line has it already.
pub(crate) fn quiet(text: &str, uid: &str) -> Option<String> {
    let mut obj = Object::parse(text);
    let client = |p: &Prop| p.param("SCHEDULE-AGENT").is_some_and(|a| a.eq_ignore_ascii_case("CLIENT"));
    let changed: usize = obj.events_mut(uid).into_iter().map(|e| e.update("ORGANIZER", |p| !client(p), |p| p.set_param("SCHEDULE-AGENT", "CLIENT"))).sum();
    (changed > 0).then(|| obj.to_string())
}

/// The resource without one occurrence of the series `uid`: an EXDATE on
/// the master in DTSTART's form, the occurrence's override gone, and every
/// ORGANIZER quiet. None when the series already leaves it out.
pub(crate) fn without_occurrence(text: &str, uid: &str, rid: &Moment, here: chrono_tz::Tz) -> Result<Option<String>> {
    let events = ical::read(text, &[])?;
    let master = events
        .iter()
        .find(|e| e.uid == uid && e.recurrence_id.is_none())
        .ok_or_else(|| Error::Other(format!("the calendar object has no series {uid}")))?;
    let excluded = master.recurrence.as_ref().is_some_and(|r| r.exdates.iter().any(|x| x.names(rid, here)));
    let cancelled = events.iter().any(|e| e.uid == uid && e.status == EventStatus::Cancelled && e.recurrence_id.is_some_and(|r| r.names(rid, here)));
    if master.recurrence.is_none() || excluded || cancelled {
        return Ok(None);
    }

    let mut obj = Object::parse(text);
    let changed = obj.calendar().into_iter().flat_map(Component::components).find_map(|c| {
        let p = c.prop("RECURRENCE-ID").filter(|_| c.name() == "VEVENT" && c.prop("UID").is_some_and(|u| u.text() == uid))?;
        rsvp::prop_moment(p).filter(|m| m.names(rid, here)).map(|_| p.value().to_string())
    });
    if let Some(value) = changed {
        obj.remove_event(uid, Some(&value));
    }
    let series = obj.event_mut(uid, None).ok_or_else(|| Error::Other(format!("the calendar object has no series {uid}")))?;
    let start = series.prop("DTSTART").ok_or_else(|| Error::Other("the series has no start".into()))?.clone();
    let value = rsvp::value_like(&start, rid, here).ok_or_else(|| Error::Other("the occurrence and its series disagree on whether they are all-day".into()))?;
    series.add_exdate(&value);
    let text = obj.to_string();
    Ok(Some(quiet(&text, uid).unwrap_or(text)))
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Local, Zone};

    #[test]
    fn each_provider_has_its_route() {
        use CalendarSource::*;
        assert_eq!(route(Google).unwrap(), Route::AskFirst, "Google acts on the organizer's cancellation itself");
        assert_eq!(route(Graph).unwrap(), Route::AskFirst, "so does Exchange");
        assert_eq!(route(CalDav).unwrap(), Route::DeleteQuietly);
        for refused in [Webcal, EventKit, Local] {
            assert!(route(refused).is_err(), "{refused:?}");
        }
    }

    const SERIES: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:standup\r\nDTSTART;TZID=Europe/Brussels:20261110T093000\r\n\
        DTEND;TZID=Europe/Brussels:20261110T094500\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\nORGANIZER;CN=Ann:mailto:ann@example.com\r\n\
        ATTENDEE;PARTSTAT=ACCEPTED:mailto:me@example.com\r\nX-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\nEND:VEVENT\r\n\
        BEGIN:VEVENT\r\nUID:standup\r\nRECURRENCE-ID;TZID=Europe/Brussels:20261117T093000\r\nDTSTART;TZID=Europe/Brussels:20261117T100000\r\n\
        DTEND;TZID=Europe/Brussels:20261117T101500\r\nSUMMARY:Standup\r\nORGANIZER;CN=Ann:mailto:ann@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn brussels(day: u32, h: u32, m: u32) -> Moment {
        Moment::Time(Local { wall: NaiveDate::from_ymd_opt(2026, 11, day).unwrap().and_hms_opt(h, m, 0).unwrap(), zone: Zone::Tz(Brussels) })
    }

    #[test]
    fn an_occurrence_leaves_by_an_exdate_without_telling_the_organizer() {
        let moved = without_occurrence(SERIES, "standup", &brussels(17, 9, 30), Brussels).unwrap().unwrap();
        assert!(moved.contains("EXDATE;TZID=Europe/Brussels:20261117T093000\r\n"), "in DTSTART's form: {moved}");
        assert!(!moved.contains("RECURRENCE-ID"), "its changed occurrence goes with it");
        assert_eq!(moved.matches("ORGANIZER;CN=Ann;SCHEDULE-AGENT=CLIENT:mailto:ann@example.com").count(), 1, "{moved}");
        assert!(moved.contains("X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\n"), "the rest stays");

        let plain = without_occurrence(SERIES, "standup", &brussels(24, 9, 30), Brussels).unwrap().unwrap();
        assert!(plain.contains("EXDATE;TZID=Europe/Brussels:20261124T093000\r\n") && plain.contains("RECURRENCE-ID"));
        assert_eq!(without_occurrence(&plain, "standup", &brussels(24, 9, 30), Brussels).unwrap(), None, "already left out");
        let utc = Moment::Time(Local { wall: NaiveDate::from_ymd_opt(2026, 11, 24).unwrap().and_hms_opt(8, 30, 0).unwrap(), zone: Zone::Tz(chrono_tz::UTC) });
        assert_eq!(without_occurrence(&plain, "standup", &utc, Brussels).unwrap(), None, "the same moment written in UTC");
    }

    #[test]
    fn the_whole_event_goes_quiet_before_its_delete() {
        let quiet_text = quiet(SERIES, "standup").unwrap();
        assert_eq!(quiet_text.matches("SCHEDULE-AGENT=CLIENT").count(), 2, "every ORGANIZER of the series");
        assert_eq!(quiet(&quiet_text, "standup"), None, "nothing left to change: straight to the DELETE");
        assert_eq!(quiet_text.replace(";SCHEDULE-AGENT=CLIENT", ""), SERIES, "only the parameter changes");
    }
}
