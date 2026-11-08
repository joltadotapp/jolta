//! Invitations Jolta sends itself, as the organizer of a meeting in a
//! calendar on this Mac: iMIP mail (RFC 6047) carrying iTIP REQUEST and
//! CANCEL objects (RFC 5546), from the account the calendar sends
//! invitations from (`calendar.send_from`).
//!
//! An op that tells the guests waits the undo window like any other; when
//! the calendar worker takes it, `letters` decides who hears what and the
//! mails go through the account's outbox. Each mail's Message-ID comes from
//! the op, the event's version and the guest, so an op tried twice queues
//! each mail once.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Weekday};
use chrono_tz::{OffsetComponents, OffsetName, Tz};

use rusqlite::Connection;

use super::change::{self, Field};
use super::ical_edit::{self, Component, Object, Prop};
use super::ops::{CalendarOp, Notify, Write};
use super::rsvp::{Mail, imip_mail};
use super::invitation::{self, Invitation, Method};
use super::series::Horizon;
use super::{Calendar, CalendarId, CalendarSource, EventData, EventStatus, Moment, Person, Span, Zone, recur};
use crate::compose::Built;
use crate::model::{Account, AccountId};
use crate::{Jolta, Result, store};

/// Whether Jolta itself mails the guests of meetings in `calendar`.
pub fn sends_itself(calendar: &Calendar) -> bool {
    calendar.source == CalendarSource::Local
}

/// The account that mails the guests of `event`, a meeting in a calendar
/// whose invitations Jolta sends: the account the meeting names as its
/// organizer, else the one the calendar sends from.
pub fn sender(conn: &Connection, calendar: &Calendar, event: &EventData) -> Result<Option<Account>> {
    if let Some(o) = &event.organizer
        && let Some(a) = store::accounts(conn)?.into_iter().find(|a| a.address.eq_ignore_ascii_case(o.address.trim()))
    {
        return Ok(Some(a));
    }
    store::calendar_send_from(conn, calendar.id)?.map(|id| store::account(conn, id)).transpose()
}

/// The accounts a calendar can send invitations from: any of the user's.
pub fn senders(conn: &Connection) -> Result<Vec<Account>> {
    store::accounts(conn)
}

/// The organizer as the guests see it: the sending account.
pub fn organizer_of(sender: &Account) -> Person {
    Person { address: sender.address.clone(), name: sender.display_name.clone() }
}

/// The mails an op sends from `sender`, one per guest: invitations and
/// updates carry the meeting as the op saved it, a series with `changed`,
/// the changed occurrences the store holds for it. Built the same way
/// every time, so a second try finds its mails in the outbox.
pub fn op_mails(op: &CalendarOp, sender: &Account, changed: &[EventData]) -> Result<Vec<Built>> {
    let organizer = organizer_of(sender);
    let mut me = vec![sender.address.clone()];
    if let Some(o) = op.write.event().and_then(|e| e.organizer.as_ref()) {
        me.push(o.address.clone());
    }
    let message = op.message.as_deref();
    let mut out = Vec::new();
    for (to, notice) in letters(&op.write, op.notify, &me) {
        let seed = |e: &EventData, sequence: i64| {
            let rid = e.recurrence_id.as_ref().map(store::moment_text).unwrap_or_default();
            format!("iMIP\n{}\n{rid}\n{sequence}\n{}\n{notice:?}\n{}", e.uid, op.id.0, to.address.to_lowercase())
        };
        let built = match (notice, &op.write) {
            (Notice::Cancellation, Write::Delete(events)) => {
                let Some(master) = events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()) else { continue };
                let sequence = events.iter().map(|e| e.sequence).max().unwrap_or(0) + 1;
                imip_cancel(master, sequence, &organizer, &to, message, seed(master, sequence))?
            }
            (Notice::Cancellation, Write::Cancel { series, occurrence, .. }) => {
                let sequence = series.sequence.max(occurrence.sequence);
                imip_cancel(occurrence, sequence, &organizer, &to, message, seed(occurrence, sequence))?
            }
            // The person taken off is the one the cancellation names.
            (Notice::Cancellation, Write::Update { base, event, .. }) => {
                let removed: Vec<_> = base.attendees.iter().filter(|a| a.person.address.eq_ignore_ascii_case(&to.address)).cloned().collect();
                let uninvited = EventData { attendees: removed, ..event.clone() };
                imip_cancel(&uninvited, event.sequence, &organizer, &to, message, seed(event, event.sequence))?
            }
            (_, Write::Create(events)) => {
                let list: Vec<&EventData> = events.iter().collect();
                let Some(first) = list.first() else { continue };
                imip_request(&list, &organizer, &to, false, message, seed(first, first.sequence))?
            }
            (_, Write::Update { event, .. }) => {
                let mut list = vec![event];
                if event.recurrence_id.is_none() {
                    list.extend(changed.iter().filter(|c| c.uid == event.uid && c.recurrence_id.is_some()));
                }
                imip_request(&list, &organizer, &to, notice == Notice::Update, message, seed(event, event.sequence))?
            }
            (_, Write::Delete(_) | Write::Cancel { .. }) => continue,
        };
        out.push(built);
    }
    Ok(out)
}

/// Makes the user the organizer of a meeting Jolta sends itself, as the
/// sending account, and raises the SEQUENCE of a meeting's new version
/// (RFC 5546): what a local calendar's store holds is what the guests get.
/// A new time or repeat rule sent to the guests asks them again, so their
/// answers start over, as Google and Microsoft 365 do.
pub fn prepare(write: &mut Write, sender: Option<&Account>, notify: Notify) {
    match write {
        Write::Create(events) => events.iter_mut().for_each(|e| stamp(e, sender)),
        Write::Update { base, event, .. } => {
            stamp(event, sender);
            if [&*base, &*event].iter().any(|e| e.organizer.is_some() && !e.attendees.is_empty()) {
                next_sequence(base, event);
                if notify != Notify::Nobody && (Field::Time.differs(base, event) || Field::Repeat.differs(base, event)) {
                    for a in event.attendees.iter_mut().filter(|a| !a.is_me && !sender.is_some_and(|s| s.address.eq_ignore_ascii_case(&a.person.address))) {
                        a.response = super::Response::NeedsAction;
                        a.comment.clear();
                    }
                }
            }
        }
        Write::Cancel { series, .. } => series.sequence += 1,
        Write::Delete(_) => {}
    }
}

/// Makes `sender` the organizer of `e` when it has guests and no organizer.
pub fn stamp(e: &mut EventData, sender: Option<&Account>) {
    if let Some(a) = sender
        && e.organizer.is_none()
        && e.attendees.iter().any(|g| !g.is_me && !g.person.address.eq_ignore_ascii_case(&a.address))
    {
        e.organizer = Some(organizer_of(a));
        e.organizer_is_me = true;
    }
}

/// What one guest hears of an op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    Invitation,
    Update,
    Cancellation,
}

/// Who hears what of `write` when the user chose `notify`. `me` are the
/// user's addresses, who never get mail. A new meeting invites everyone; a
/// change tells the guests who stay and the new ones, or with `Changed`
/// only the new ones, and cancels it for the people taken off; a delete
/// cancels it for everyone. Rooms count as guests: a room's mailbox answers
/// an invitation as a person does.
pub fn letters(write: &Write, notify: Notify, me: &[String]) -> Vec<(Person, Notice)> {
    if notify == Notify::Nobody {
        return Vec::new();
    }
    let others = |events: &[&EventData]| -> Vec<Person> {
        let mut out: Vec<Person> = Vec::new();
        for a in events.iter().flat_map(|e| &e.attendees) {
            let mine = a.is_me || me.iter().any(|m| m.eq_ignore_ascii_case(a.person.address.trim()));
            if !mine && !out.iter().any(|p| p.address.eq_ignore_ascii_case(&a.person.address)) {
                out.push(a.person.clone());
            }
        }
        out
    };
    let all = |people: Vec<Person>, notice| people.into_iter().map(|p| (p, notice)).collect();
    match write {
        Write::Create(events) => all(others(&events.iter().collect::<Vec<_>>()), Notice::Invitation),
        Write::Delete(events) => all(others(&events.iter().collect::<Vec<_>>()), Notice::Cancellation),
        Write::Cancel { occurrence, .. } => all(others(&[occurrence]), Notice::Cancellation),
        Write::Update { base, event, .. } => {
            let c = change::classify(base, event, me);
            let kept = if notify == Notify::Changed { Vec::new() } else { c.kept };
            let mut out: Vec<(Person, Notice)> = kept.into_iter().map(|p| (p, Notice::Update)).collect();
            out.extend(c.added.into_iter().map(|p| (p, Notice::Invitation)));
            out.extend(c.removed.into_iter().map(|p| (p, Notice::Cancellation)));
            out
        }
    }
}

/// Raises the SEQUENCE of `event`, the saved version of `base`, when guests
/// must take it as a new version (RFC 5546 section 2.1.4): a new time, day,
/// repeat rule, place, title, description, status or guest list. Exchange
/// raises it for a new guest list too (C3 spec, section 2), so a guest
/// invited again after a cancellation gets a version above it.
pub fn next_sequence(base: &EventData, event: &mut EventData) {
    let raises = [Field::Time, Field::Repeat, Field::Location, Field::Title, Field::Description, Field::Status, Field::People];
    if raises.iter().any(|f| f.differs(base, event)) && event.sequence <= base.sequence {
        event.sequence = base.sequence + 1;
    }
}

/// Reads the guests' replies that reached `account` for meetings it
/// organizes in calendars on this Mac, and returns the calendars whose
/// answers changed. A reply waits while an edit of its meeting is still
/// to be sent, and is read once.
pub fn read_replies(jolta: &Jolta, account: AccountId, horizon: &Horizon) -> Result<Vec<CalendarId>> {
    let (account, unread) = jolta.store.read(|c| Ok((store::account(c, account)?, store::unread_local_replies(c, account)?)))?;
    let mut changed = Vec::new();
    for reply in unread {
        let Ok(raw) = jolta.blobs.get(&reply.blob) else { continue };
        let found: Vec<Invitation> = invitation::find(&raw, &[]).into_iter().filter(|i| i.method == Method::Reply).collect();
        jolta.store.write(|tx| {
            let mut waits = false;
            for answer in &found {
                match apply_reply(tx, &account, &reply.from_addr, answer, horizon)? {
                    Read::Taken(id) if !changed.contains(&id) => changed.push(id),
                    Read::Wait => waits = true,
                    _ => {}
                }
            }
            if !waits {
                store::read_reply(tx, account.id, &reply.message_id)?;
            }
            Ok(())
        })?;
    }
    Ok(changed)
}

/// What one reply did to the organizer's meeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Taken(CalendarId),
    /// Not a guest's answer to a meeting `account` organizes here, or older
    /// than the answer the guest gave since.
    Ignored,
    /// An edit of the meeting is still to be sent; read it after.
    Wait,
}

/// Google sends the replies of its users to an organizer outside Google
/// from this address, naming the guest in the reply (`filing`).
const GOOGLE_FOR_GUESTS: &str = "calendar-notification@google.com";

/// Takes a guest's iMIP REPLY, which `sender` mailed to `account`, into the
/// meeting it answers (RFC 5546 section 3.2.3): the guest's answer and
/// note, on the series or the one occurrence it names.
///
/// It counts only for a meeting in a calendar on this Mac that `account`
/// organizes (check 1), for the occurrence its RECURRENCE-ID names as a
/// moment (check 2), from the guest's own address, or Google's for its
/// users, for a guest on the meeting (check 3), and when it is newer by
/// DTSTAMP than the last answer taken from that guest there (check 4). An
/// answer to a time the meeting no longer has, from before an update the
/// guest hasn't answered, doesn't count either.
pub fn apply_reply(conn: &Connection, account: &Account, sender: &str, reply: &Invitation, horizon: &Horizon) -> Result<Read> {
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    let from_google = same(sender, GOOGLE_FOR_GUESTS);
    let Some(answer) = reply.event.attendees.iter().find(|a| from_google || same(&a.person.address, sender)) else { return Ok(Read::Ignored) };
    let uid = &reply.event.uid;
    let here = horizon.here;
    for calendar in store::calendars(conn)?.into_iter().filter(|c| sends_itself(c) && c.is_own(&account.address)) {
        // Judged against the meeting as it is once the user's edits are
        // sent or taken back, a delete's too.
        if store::calendar_op_waiting(conn, calendar.id, uid)? {
            return Ok(Read::Wait);
        }
        let Some(master) = super::ops::stored(conn, calendar.id, uid, None)? else { continue };
        let organizes = master.event.organizer.as_ref().is_some_and(|o| same(&o.address, &account.address));
        let invited = |e: &EventData| e.attendees.iter().any(|a| !a.is_me && same(&a.person.address, &answer.person.address));
        if !organizes {
            continue;
        }
        // The rows the answer lands on: the occurrence's own, one made from
        // the series for an occurrence without one, or the series and the
        // occurrences changed on their own that the guest didn't answer
        // since.
        let key = reply.event.recurrence_id.as_ref().map(|r| reply_key(r, here)).unwrap_or_default();
        let rows: Vec<(Option<super::ops::Stored>, EventData)> = match &reply.event.recurrence_id {
            Some(rid) => {
                let own = super::ops::series(conn, calendar.id, uid)?.into_iter().find(|e| e.recurrence_id.is_some_and(|r| r.names(rid, here)));
                let excluded = master.event.recurrence.as_ref().is_some_and(|r| r.exdates.iter().any(|x| x.names(rid, here)));
                match own {
                    Some(e) if e.status == EventStatus::Cancelled => return Ok(Read::Ignored),
                    Some(e) => vec![(super::ops::stored(conn, calendar.id, uid, e.recurrence_id.as_ref())?, e)],
                    None if excluded => return Ok(Read::Ignored),
                    None if super::ops::is_occurrence(&master.event, rid, here) => match super::series::occurrence(std::slice::from_ref(&master.event), rid, here) {
                        // An unchanged occurrence is named by its start on
                        // the series' clock, whatever clock the reply used.
                        Some(e) => vec![(None, EventData { recurrence_id: Some(e.span.start()), ..e })],
                        None => return Ok(Read::Ignored),
                    },
                    None => return Ok(Read::Ignored),
                }
            }
            None => {
                let mut rows = vec![(None, master.event.clone())];
                for e in super::ops::series(conn, calendar.id, uid)?.into_iter().filter(|e| e.recurrence_id.is_some()) {
                    let own = e.recurrence_id.as_ref().map(|r| reply_key(r, here)).unwrap_or_default();
                    let later = store::guest_reply_stamp(conn, calendar.id, uid, &own, &answer.person.address)?.is_some_and(|t| t > stamp_of(reply));
                    if !later {
                        rows.push((super::ops::stored(conn, calendar.id, uid, e.recurrence_id.as_ref())?, e));
                    }
                }
                rows
            }
        };
        let Some((_, target)) = rows.first() else { return Ok(Read::Ignored) };
        if !invited(target) && !invited(&master.event) {
            return Ok(Read::Ignored);
        }
        // A newer answer for this occurrence, or for the whole series, wins.
        let newer = |k: &str| -> Result<bool> { Ok(store::guest_reply_stamp(conn, calendar.id, uid, k, &answer.person.address)?.is_some_and(|t| t >= stamp_of(reply))) };
        if newer(&key)? || (!key.is_empty() && newer("")?) {
            return Ok(Read::Ignored);
        }
        if reply.event.sequence < target.sequence && reply.dated && !reply.event.span.start().names(&target.span.start(), here) {
            return Ok(Read::Ignored);
        }
        for (stored, mut e) in rows {
            let Some(guest) = e.attendees.iter_mut().find(|a| same(&a.person.address, &answer.person.address)) else { continue };
            guest.response = answer.response;
            guest.comment = answer.comment.trim().to_string();
            let (remote_id, etag, blob) = match &stored {
                Some(s) => (s.remote_id.clone(), s.etag.clone(), s.blob.clone()),
                None => (master.remote_id.clone(), master.etag.clone(), master.blob.clone()),
            };
            store::insert_event(conn, calendar.id, &remote_id, etag.as_deref(), &e, blob.as_deref())?;
        }
        store::set_guest_reply_stamp(conn, calendar.id, uid, &key, &answer.person.address, stamp_of(reply))?;
        store::rebuild_series(conn, calendar.id, uid, horizon)?;
        return Ok(Read::Taken(calendar.id));
    }
    Ok(Read::Ignored)
}

fn stamp_of(reply: &Invitation) -> i64 {
    reply.stamp.map_or(0, |t| t.timestamp())
}

/// An occurrence as `guest_reply` keys it: its instant in UTC, or its day.
fn reply_key(m: &Moment, here: Tz) -> String {
    match m {
        Moment::Date(d) => d.format("%Y%m%d").to_string(),
        Moment::Time(t) => t.instant(here).format("%Y%m%dT%H%M%SZ").to_string(),
    }
}

/// The text of one iTIP object: `events` under `method`, each with its
/// ORGANIZER, and a VTIMEZONE for every zone their times name.
fn itip(method: &str, events: Vec<Component>, zones: &[(Tz, NaiveDateTime)]) -> String {
    let mut cal = Component::new("VCALENDAR");
    cal.add(Prop::text_value("PRODID", concat!("-//Jolta//Jolta ", env!("CARGO_PKG_VERSION"), "//EN")));
    cal.add(Prop::new("VERSION", "2.0"));
    cal.add(Prop::new("CALSCALE", "GREGORIAN"));
    cal.add(Prop::new("METHOD", method));
    let mut written: Vec<Tz> = Vec::new();
    for &(tz, around) in zones {
        if !written.contains(&tz) {
            written.push(tz);
            cal.push(vtimezone(tz, around));
        }
    }
    for e in events {
        cal.push(e);
    }
    Object::new(cal).to_string()
}

/// Every zone a time of `events` is in, with a wall time it is used at.
fn zones_of(events: &[&EventData]) -> Vec<(Tz, NaiveDateTime)> {
    let mut out = Vec::new();
    let mut add = |m: &Moment| {
        if let Moment::Time(t) = m
            && let Zone::Tz(tz) = t.zone
            && tz != Tz::UTC
        {
            out.push((tz, t.wall));
        }
    };
    for e in events {
        if let Span::Timed { start, end } = &e.span {
            add(&Moment::Time(*start));
            add(&Moment::Time(*end));
        }
        if let Some(rid) = &e.recurrence_id {
            add(rid);
        }
        for m in e.recurrence.iter().flat_map(|r| r.rdates.iter().chain(&r.exdates)) {
            add(m);
        }
    }
    out
}

/// The VEVENT a guest gets: the meeting as the organizer has it, without
/// what is the organizer's own (alerts, color, categories).
fn guest_vevent(e: &EventData, organizer: &Person) -> Component {
    let mut ev = ical_edit::vevent(&EventData { organizer: Some(organizer.clone()), ..e.clone() });
    ev.remove_components("VALARM");
    for own in ["COLOR", "CATEGORIES"] {
        ev.remove(own);
    }
    ev
}

/// The mail inviting `to` to `events`, a meeting or a series with its
/// changed occurrences, or telling them of its new version (`update`). The
/// user's `message` goes in COMMENT and above the details.
pub fn imip_request(events: &[&EventData], organizer: &Person, to: &Person, update: bool, message: Option<&str>, same_as: String) -> Result<Built> {
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    let mut components: Vec<Component> = events.iter().map(|e| guest_vevent(e, organizer)).collect();
    if let (Some(m), Some(first)) = (message, components.first_mut()) {
        first.add(Prop::text_value("COMMENT", m));
    }
    let shown = events.iter().find(|e| e.recurrence_id.is_none()).or(events.first()).copied();
    let title = shown.map(|e| e.title.trim()).filter(|t| !t.is_empty()).unwrap_or("(no title)");
    let who = change::name(organizer);
    let (subject, line) = match update {
        false => (format!("Invitation: {title}"), format!("{who} invites you to {title}.")),
        true => (format!("Updated invitation: {title}"), format!("{who} changed {title}.")),
    };
    let text = letter_text(&line, message, shown);
    imip_mail(Mail {
        from: &organizer.address,
        to: (Some(to.name.clone()).filter(|n| !n.trim().is_empty()), to.address.clone()),
        subject,
        text,
        method: "REQUEST",
        ics: itip("REQUEST", components, &zones_of(events)),
        same_as: Some(same_as),
    })
}

/// The mail cancelling `event` for `to`: the whole meeting or series, or
/// with a recurrence id that one occurrence. Its SEQUENCE is `sequence`,
/// higher than any version the guest has (RFC 5546 section 3.2.5).
pub fn imip_cancel(event: &EventData, sequence: i64, organizer: &Person, to: &Person, message: Option<&str>, same_as: String) -> Result<Built> {
    let message = message.map(str::trim).filter(|m| !m.is_empty());
    let mut ev = Component::new("VEVENT");
    ev.add(Prop::text_value("UID", &event.uid));
    if let Some(rid) = &event.recurrence_id {
        ev.add(Prop::moment("RECURRENCE-ID", rid));
    }
    ev.add(Prop::new("DTSTAMP", &ical_edit::utc_stamp()));
    let shown = EventData { organizer: Some(organizer.clone()), status: EventStatus::Cancelled, sequence, ..event.clone() };
    let full = ical_edit::vevent(&shown);
    for name in ["DTSTART", "DTEND", "RRULE", "SUMMARY", "LOCATION", "ORGANIZER", "ATTENDEE", "SEQUENCE", "STATUS"] {
        for p in full.props().filter(|p| p.is(name)) {
            ev.add(p.clone());
        }
    }
    if let Some(m) = message {
        ev.add(Prop::text_value("COMMENT", m));
    }
    let title = if event.title.trim().is_empty() { "(no title)" } else { event.title.trim() };
    let who = change::name(organizer);
    let (subject, line) = match &event.recurrence_id {
        None => (format!("Cancelled: {title}"), format!("{who} cancelled {title}.")),
        Some(rid) => {
            let day = match rid {
                Moment::Date(d) => *d,
                Moment::Time(t) => t.wall.date(),
            };
            (format!("Cancelled: {title} on {}", day.format("%a %-d %b")), format!("{who} cancelled {title} on {}.", day.format("%a %-d %b %Y")))
        }
    };
    let text = letter_text(&line, message, None);
    imip_mail(Mail {
        from: &organizer.address,
        to: (Some(to.name.clone()).filter(|n| !n.trim().is_empty()), to.address.clone()),
        subject,
        text,
        method: "CANCEL",
        ics: itip("CANCEL", vec![ev], &zones_of(&[event])),
        same_as: Some(same_as),
    })
}

/// The mail's text: what happened, the user's message, and the meeting's
/// time, repeat, place and description.
fn letter_text(line: &str, message: Option<&str>, event: Option<&EventData>) -> String {
    let mut out = vec![line.to_string()];
    if let Some(m) = message {
        out.push(m.to_string());
    }
    if let Some(e) = event {
        let mut details = vec![format!("When: {}", when(&e.span))];
        if let Some(rule) = e.recurrence.as_ref().and_then(|r| recur::describe(r, &e.span)) {
            details.push(format!("Repeats: {rule}"));
        }
        if !e.location.trim().is_empty() {
            details.push(format!("Where: {}", e.location.trim()));
        }
        out.push(details.join("\n"));
        if !e.description.trim().is_empty() && !e.description_html {
            out.push(e.description.trim().to_string());
        }
    }
    out.join("\n\n")
}

/// "Tue 24 Nov 2026, 14:00 to 14:30 (Europe/Brussels)", on the event's own clock.
fn when(span: &Span) -> String {
    match span {
        Span::AllDay { start, end } if *end <= start.succ_opt().unwrap_or(*start) => format!("{}, all day", start.format("%a %-d %b %Y")),
        Span::AllDay { start, end } => format!("{} to {}, all day", start.format("%a %-d %b %Y"), (*end - Duration::days(1)).format("%a %-d %b %Y")),
        Span::Timed { start, end } => {
            let zone = match start.zone {
                Zone::Tz(tz) => format!(" ({tz})"),
                Zone::Floating => String::new(),
            };
            let until = if end.wall.date() == start.wall.date() { end.wall.format("%H:%M").to_string() } else { end.wall.format("%a %-d %b %Y %H:%M").to_string() };
            format!("{}, {} to {until}{zone}", start.wall.format("%a %-d %b %Y"), start.wall.format("%H:%M"))
        }
    }
}

/// A VTIMEZONE for `tz` (RFC 5545 section 3.6.5), which Exchange needs to
/// read a TZID it doesn't know: the zone's offsets and transitions in the
/// year of `around`, each repeating yearly on the same weekday of the month
/// when the next years agree, else listed for ten years.
pub fn vtimezone(tz: Tz, around: NaiveDateTime) -> Component {
    let mut zone = Component::new("VTIMEZONE");
    zone.add(Prop::new("TZID", tz.name()));
    let year = around.year();
    let changes = transitions(tz, year);
    if changes.is_empty() {
        let offset = tz.offset_from_utc_datetime(&around);
        let mut standard = Component::new("STANDARD");
        standard.add(Prop::new("DTSTART", "19700101T000000"));
        let fixed = offset.fix().local_minus_utc();
        standard.add(Prop::new("TZOFFSETFROM", &offset_text(fixed)));
        standard.add(Prop::new("TZOFFSETTO", &offset_text(fixed)));
        if let Some(name) = offset.abbreviation() {
            standard.add(Prop::new("TZNAME", name));
        }
        zone.push(standard);
        return zone;
    }
    for change in &changes {
        let mut part = Component::new(if change.daylight { "DAYLIGHT" } else { "STANDARD" });
        part.add(Prop::new("DTSTART", &change.wall.format("%Y%m%dT%H%M%S").to_string()));
        part.add(Prop::new("TZOFFSETFROM", &offset_text(change.from)));
        part.add(Prop::new("TZOFFSETTO", &offset_text(change.to)));
        let rule = yearly_rule(change.wall.date());
        let repeats = (1..=3).all(|n| {
            transitions(tz, year + n).iter().any(|later| later.daylight == change.daylight && later.from == change.from && yearly_rule(later.wall.date()) == rule)
        });
        if repeats {
            part.add(Prop::new("RRULE", &rule));
        } else {
            for n in 1..=10 {
                for later in transitions(tz, year + n).iter().filter(|l| l.daylight == change.daylight) {
                    part.add(Prop::new("RDATE", &later.wall.format("%Y%m%dT%H%M%S").to_string()));
                }
            }
        }
        if let Some(name) = &change.name {
            part.add(Prop::new("TZNAME", name));
        }
        zone.push(part);
    }
    zone
}

/// One change of a zone's offset: the wall time it happens at on the clock
/// before it, the offsets in seconds, and whether daylight time starts.
struct Transition {
    wall: NaiveDateTime,
    from: i32,
    to: i32,
    daylight: bool,
    name: Option<String>,
}

/// The offset changes of `tz` in `year`, found day by day and then to the
/// second.
fn transitions(tz: Tz, year: i32) -> Vec<Transition> {
    let Some(first) = NaiveDate::from_ymd_opt(year, 1, 1) else { return Vec::new() };
    let offset = |t: NaiveDateTime| tz.offset_from_utc_datetime(&t).fix().local_minus_utc();
    let mut out = Vec::new();
    let mut day = first.and_hms_opt(0, 0, 0).unwrap_or_default();
    while day.year() == year {
        let next = day + Duration::days(1);
        if offset(day) != offset(next) {
            let (mut lo, mut hi) = (day, next);
            while hi - lo > Duration::seconds(1) {
                let mid = lo + (hi - lo) / 2;
                if offset(mid) == offset(lo) { lo = mid } else { hi = mid }
            }
            let at = tz.offset_from_utc_datetime(&hi);
            let from = offset(lo);
            out.push(Transition {
                wall: hi + Duration::seconds(from.into()),
                from,
                to: offset(hi),
                daylight: at.dst_offset() > chrono::TimeDelta::zero(),
                name: at.abbreviation().map(str::to_string),
            });
        }
        day = next;
    }
    out
}

/// "FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU" for a change on the last Sunday of
/// March, "BYDAY=2SU" on the second.
fn yearly_rule(day: NaiveDate) -> String {
    let weekday = match day.weekday() {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    };
    let last = (day + Duration::days(7)).month() != day.month();
    let nth = if last { "-1".to_string() } else { ((day.day() - 1) / 7 + 1).to_string() };
    format!("FREQ=YEARLY;BYMONTH={};BYDAY={nth}{weekday}", day.month())
}

/// "+0100", "-0430".
fn offset_text(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.abs() / 60;
    format!("{sign}{:02}{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Attendee, Local, Recurrence, Response, Role};

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn person(address: &str, name: &str) -> Person {
        Person { address: address.into(), name: name.into() }
    }

    fn guest(address: &str, role: Role) -> Attendee {
        Attendee { person: person(address, ""), role, response: Response::NeedsAction, comment: String::new(), is_me: false }
    }

    fn me() -> Person {
        person("me@example.com", "Me Organizer")
    }

    fn meeting() -> EventData {
        EventData {
            title: "Planning".into(),
            location: "Room 1".into(),
            attendees: vec![guest("ann@example.com", Role::Required), guest("bo@example.com", Role::Optional)],
            organizer: Some(me()),
            organizer_is_me: true,
            sequence: 2,
            alarms: vec![crate::calendar::Alarm { trigger: crate::calendar::AlarmTrigger::MinutesBefore(10), email: false }],
            color: Some("#ff0000".into()),
            ..EventData::new("u1@jolta.app", Span::Timed { start: at("2026-11-24 14:00"), end: at("2026-11-24 14:30") })
        }
    }

    fn calendar_part(built: &Built) -> (String, String) {
        let msg = mail_parser::MessageParser::default().parse(&built.raw).expect("a message");
        let part = msg.parts().find(|p| p.content_type().is_some_and(|c| c.subtype() == Some("calendar"))).expect("a text/calendar part");
        let ics = String::from_utf8_lossy(&part.decoded()).into_owned();
        let text = msg.body_text(0).unwrap().replace("\r\n", "\n");
        (ics, text)
    }

    #[test]
    fn a_request_carries_the_organizer_every_guest_the_sequence_and_the_zone() {
        let built = imip_request(&[&meeting()], &me(), &person("ann@example.com", "Ann"), false, None, "seed".into()).unwrap();
        let (ics, text) = calendar_part(&built);
        let cal = Object::parse(&ics);
        let cal = cal.calendar().unwrap();
        assert_eq!(cal.prop("METHOD").unwrap().value(), "REQUEST");
        let ev = cal.components().find(|c| c.name() == "VEVENT").unwrap();
        assert_eq!(ev.prop("UID").unwrap().text(), "u1@jolta.app");
        assert_eq!(ev.prop("ORGANIZER").unwrap().address(), Some("me@example.com"));
        assert_eq!(ev.props().filter(|p| p.is("ATTENDEE")).count(), 2, "{ics}");
        assert_eq!(ev.prop("SEQUENCE").unwrap().value(), "2");
        assert!(ev.prop("DTSTAMP").is_some_and(|p| p.value().ends_with('Z')));
        assert_eq!(ev.prop("DTSTART").unwrap().param("TZID"), Some("Europe/Brussels"));
        assert!(ev.components().next().is_none() && ev.prop("COLOR").is_none(), "the organizer's own alerts and color stay home: {ics}");
        let zone = cal.components().find(|c| c.name() == "VTIMEZONE").expect("the zone DTSTART names");
        assert_eq!(zone.prop("TZID").unwrap().value(), "Europe/Brussels");
        assert_eq!(built.recipients, ["ann@example.com"]);
        assert_eq!(built.from, "me@example.com");
        assert!(text.starts_with("Me Organizer invites you to Planning.") && text.contains("Tue 24 Nov 2026, 14:00 to 14:30 (Europe/Brussels)") && text.contains("Where: Room 1"), "{text}");
    }

    #[test]
    fn the_same_letter_built_twice_has_the_same_message_id() {
        let a = imip_request(&[&meeting()], &me(), &person("ann@example.com", ""), false, None, "op 1".into()).unwrap();
        let b = imip_request(&[&meeting()], &me(), &person("ann@example.com", ""), false, None, "op 1".into()).unwrap();
        let c = imip_request(&[&meeting()], &me(), &person("ann@example.com", ""), false, None, "op 2".into()).unwrap();
        assert_eq!(a.message_id, b.message_id);
        assert_ne!(a.message_id, c.message_id);
    }

    #[test]
    fn a_series_request_holds_the_series_and_its_changed_occurrences() {
        let mut series = meeting();
        series.recurrence = Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=4".into()], ..Default::default() });
        let moved = EventData { recurrence_id: Some(Moment::Time(at("2026-12-01 14:00"))), span: Span::Timed { start: at("2026-12-01 15:00"), end: at("2026-12-01 15:30") }, ..meeting() };
        let built = imip_request(&[&series, &moved], &me(), &person("bo@example.com", ""), true, Some("Moved week 2"), "s".into()).unwrap();
        let (ics, text) = calendar_part(&built);
        let obj = Object::parse(&ics);
        let events: Vec<&Component> = obj.calendar().unwrap().components().filter(|c| c.name() == "VEVENT").collect();
        assert_eq!(events.len(), 2);
        assert!(events[0].prop("RRULE").is_some() && events[0].prop("RECURRENCE-ID").is_none());
        assert_eq!(events[1].prop("RECURRENCE-ID").unwrap().value(), "20261201T140000");
        assert_eq!(events[0].prop("COMMENT").unwrap().text(), "Moved week 2");
        assert!(text.starts_with("Me Organizer changed Planning.\n\nMoved week 2") && text.contains("Repeats: "), "{text}");
    }

    #[test]
    fn a_cancel_says_cancelled_with_a_higher_sequence_for_the_whole_meeting_or_one_occurrence() {
        let built = imip_cancel(&meeting(), 3, &me(), &person("ann@example.com", ""), Some("Sorry"), "c".into()).unwrap();
        let (ics, text) = calendar_part(&built);
        let obj = Object::parse(&ics);
        let cal = obj.calendar().unwrap();
        assert_eq!(cal.prop("METHOD").unwrap().value(), "CANCEL");
        let ev = cal.components().find(|c| c.name() == "VEVENT").unwrap();
        assert_eq!(ev.prop("STATUS").unwrap().value(), "CANCELLED");
        assert_eq!(ev.prop("SEQUENCE").unwrap().value(), "3");
        assert_eq!(ev.prop("ORGANIZER").unwrap().address(), Some("me@example.com"));
        assert_eq!(ev.prop("COMMENT").unwrap().text(), "Sorry");
        assert!(ev.prop("RECURRENCE-ID").is_none());
        assert!(text.starts_with("Me Organizer cancelled Planning.\n\nSorry"), "{text}");

        let one = EventData { recurrence_id: Some(Moment::Time(at("2026-12-01 14:00"))), ..meeting() };
        let built = imip_cancel(&one, 3, &me(), &person("ann@example.com", ""), None, "c1".into()).unwrap();
        let (ics, _) = calendar_part(&built);
        assert!(ics.contains("RECURRENCE-ID;TZID=Europe/Brussels:20261201T140000") && ics.contains("BEGIN:VTIMEZONE"), "{ics}");
    }

    #[test]
    fn who_hears_what() {
        let me = ["me@example.com".to_string()];
        let base = meeting();
        let mut moved = base.clone();
        moved.attendees.retain(|a| a.person.address != "bo@example.com");
        moved.attendees.push(guest("cy@example.com", Role::Required));
        moved.span = Span::Timed { start: at("2026-11-24 15:00"), end: at("2026-11-24 15:30") };
        let update = Write::Update { base: base.clone(), event: moved, dropped: Vec::new(), fresh: false };
        let names = |l: Vec<(Person, Notice)>| l.into_iter().map(|(p, n)| (p.address, n)).collect::<Vec<_>>();
        assert_eq!(
            names(letters(&update, Notify::All, &me)),
            [("ann@example.com".into(), Notice::Update), ("cy@example.com".into(), Notice::Invitation), ("bo@example.com".into(), Notice::Cancellation)]
        );
        assert_eq!(names(letters(&update, Notify::Changed, &me)), [("cy@example.com".into(), Notice::Invitation), ("bo@example.com".into(), Notice::Cancellation)]);
        assert!(letters(&update, Notify::Nobody, &me).is_empty());

        let mut with_me = base.clone();
        with_me.attendees.push(guest("ME@example.com", Role::Required));
        assert_eq!(names(letters(&Write::Create(vec![with_me.clone()]), Notify::All, &me)).len(), 2, "the user gets no invitation");
        assert!(names(letters(&Write::Delete(vec![with_me]), Notify::All, &me)).iter().all(|(_, n)| *n == Notice::Cancellation));
    }

    #[test]
    fn the_sequence_rises_for_what_guests_must_take_as_new() {
        let base = meeting();
        let mut renamed = EventData { title: "Planning 2".into(), ..base.clone() };
        next_sequence(&base, &mut renamed);
        assert_eq!(renamed.sequence, 3);
        let mut quiet = EventData { alarms: Vec::new(), ..base.clone() };
        next_sequence(&base, &mut quiet);
        assert_eq!(quiet.sequence, 2, "the user's own alerts are no new version");
        let mut people = base.clone();
        people.attendees.pop();
        next_sequence(&base, &mut people);
        assert_eq!(people.sequence, 3, "a new guest list is a new version");
    }

    #[test]
    fn a_zone_with_daylight_time_repeats_its_changes_yearly() {
        let zone = vtimezone(Brussels, at("2026-11-24 14:00").wall);
        let text = Object::new(zone).to_string();
        assert!(text.contains("BEGIN:DAYLIGHT\r\nDTSTART:20260329T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nTZNAME:CEST"), "{text}");
        assert!(text.contains("BEGIN:STANDARD\r\nDTSTART:20261025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nTZNAME:CET"), "{text}");
        let ny = Object::new(vtimezone(chrono_tz::America::New_York, at("2026-06-01 09:00").wall)).to_string();
        assert!(ny.contains("DTSTART:20260308T020000") && ny.contains("BYMONTH=3;BYDAY=2SU") && ny.contains("BYMONTH=11;BYDAY=1SU") && ny.contains("TZOFFSETTO:-0400"), "{ny}");
        let tokyo = Object::new(vtimezone(chrono_tz::Asia::Tokyo, at("2026-06-01 09:00").wall)).to_string();
        assert!(tokyo.contains("BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:+0900\r\nTZOFFSETTO:+0900") && !tokyo.contains("DAYLIGHT"), "{tokyo}");
    }

    #[test]
    fn the_reader_takes_a_request_back_as_written() {
        let built = imip_request(&[&meeting()], &me(), &person("ann@example.com", ""), false, None, "r".into()).unwrap();
        let found = crate::calendar::invitation::find(&built.raw, &["ann@example.com".to_string()]);
        let [one] = found.as_slice() else { panic!("{found:?}") };
        assert_eq!(one.method, crate::calendar::invitation::Method::Request);
        assert_eq!((one.event.span, one.event.sequence, one.event.title.as_str()), (meeting().span, 2, "Planning"));
        assert_eq!(one.event.organizer.as_ref().map(|o| o.address.as_str()), Some("me@example.com"));
    }

    mod queue {
        use chrono::Utc;

        use crate::calendar::invitation::tests::{import, jolta};
        use crate::calendar::ops::{self, Change, Edit, Notify};
        use crate::calendar::series::Horizon;
        use crate::calendar::{CalendarId, EventData, Span, draft, tz};
        use crate::{AccountId, Jolta, store};

        use super::*;

        /// A calendar on this Mac that sends from the test account
        /// (me@example.com), unless `send_from` is false.
        fn local(name: &str, send_from: bool) -> (Jolta, CalendarId) {
            let (j, _) = jolta(name);
            let cal = j.store.write(|tx| ops::new_local_calendar(tx, "Here", None)).unwrap().id;
            if send_from {
                j.store.write(|tx| store::set_calendar_send_from(tx, cal, Some(AccountId(1)))).unwrap();
            }
            (j, cal)
        }

        fn horizon() -> Horizon {
            Horizon::around(Utc::now(), tz::here())
        }

        /// Queued so the undo window is over.
        fn queue(j: &Jolta, edit: Edit) -> crate::Result<Option<ops::CalendarOpId>> {
            j.store.write(|tx| ops::queue(tx, &edit, Utc::now().timestamp() - 20, &horizon()))
        }

        fn run(j: &Jolta) -> Vec<ops::Settled> {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(ops::run_due(j, horizon())).unwrap()
        }

        /// The outbox: (method, sender, recipient, SEQUENCE).
        fn outbox(j: &Jolta) -> Vec<(String, String, String, String)> {
            let rows: Vec<(String, String, String)> = j
                .store
                .read(|c| {
                    let mut stmt = c.prepare("SELECT blob, sender, recipients FROM outbox ORDER BY id")?;
                    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                    Ok(rows.collect::<rusqlite::Result<_>>()?)
                })
                .unwrap();
            rows.into_iter()
                .map(|(blob, from, to)| {
                    let raw = j.blobs.get(&blob).unwrap();
                    let found = crate::calendar::invitation::find(&raw, &[]);
                    (found[0].method.to_string(), from, to, found[0].event.sequence.to_string())
                })
                .collect()
        }

        fn meeting(guests: &[&str]) -> EventData {
            let start = Local { wall: NaiveDateTime::parse_from_str("2026-11-24 14:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) };
            EventData {
                title: "Planning".into(),
                attendees: guests.iter().map(|g| guest(g, Role::Required)).collect(),
                ..EventData::new("p1@jolta.app", Span::Timed { start, end: Local { wall: start.wall + Duration::minutes(30), ..start } })
            }
        }

        fn told(cal: CalendarId, change: Change, notify: Notify, message: Option<&str>) -> Edit {
            Edit { notify, message: message.map(Into::into), ..Edit::quiet(cal, change) }
        }

        #[test]
        fn a_new_meeting_invites_each_guest_once_from_the_sending_account() {
            let (j, cal) = local("u5b-create", true);
            let op = queue(&j, told(cal, Change::Create(vec![meeting(&["ann@example.com", "bo@example.com"])]), Notify::All, None)).unwrap().unwrap();
            let stored = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            assert_eq!((stored.organizer.as_ref().map(|o| o.address.as_str()), stored.organizer_is_me), (Some("me@example.com"), true), "the user organizes it");
            let settled = run(&j);
            assert_eq!(settled.iter().map(|s| s.mailed).collect::<Vec<_>>(), [Some(AccountId(1))], "the account's outbox is woken");
            let sent = outbox(&j);
            assert_eq!(
                sent,
                [
                    ("REQUEST".into(), "me@example.com".into(), "ann@example.com".into(), "0".into()),
                    ("REQUEST".into(), "me@example.com".into(), "bo@example.com".into(), "0".into())
                ]
            );
            assert_eq!(j.store.read(|c| store::calendar_op_outbox(c, op)).unwrap().len(), 2, "the op links its mails");

            // A crash after the mails were queued: the op is tried again and
            // finds its mails in the outbox (check 4).
            j.store.write(|tx| Ok(tx.execute("UPDATE calendar_op SET state = 'pending' WHERE id = ?1", [op.0])?)).unwrap();
            run(&j);
            assert_eq!(outbox(&j).len(), 2, "each mail once");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        #[test]
        fn undo_inside_the_window_mails_nobody_and_the_same_save_twice_mails_once() {
            let (j, cal) = local("u5b-undo", true);
            queue(&j, told(cal, Change::Create(vec![meeting(&["ann@example.com"])]), Notify::Nobody, None)).unwrap();
            run(&j);
            let now = Utc::now().timestamp();
            let mut moved = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            moved.location = "Room 2".into();
            let edit = told(cal, Change::Update(moved.clone()), Notify::All, None);
            let op = j.store.write(|tx| ops::queue(tx, &edit, now, &horizon())).unwrap().unwrap();
            assert_eq!(j.store.write(|tx| ops::undo(tx, op, now, &horizon())).unwrap(), ops::Undone::Dropped);
            run(&j);
            assert!(outbox(&j).is_empty(), "Don't send, then Undo inside the window: nothing went");

            assert!(queue(&j, edit.clone()).unwrap().is_some());
            assert_eq!(queue(&j, edit).unwrap(), None, "the same save again queues nothing");
            run(&j);
            assert_eq!(outbox(&j), [("REQUEST".into(), "me@example.com".into(), "ann@example.com".into(), "1".into())], "a new place is a new version");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        #[test]
        fn a_change_tells_who_stays_invites_who_is_new_and_cancels_for_who_left() {
            let (j, cal) = local("u5b-update", true);
            queue(&j, told(cal, Change::Create(vec![meeting(&["ann@example.com", "bo@example.com"])]), Notify::All, None)).unwrap();
            run(&j);
            let mut e = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            e.attendees.retain(|a| a.person.address != "bo@example.com");
            e.attendees.push(guest("cy@example.com", Role::Required));
            e.title = "Planning, new time".into();
            queue(&j, told(cal, Change::Update(e.clone()), Notify::All, Some("See you"))).unwrap();
            run(&j);
            let sent = outbox(&j);
            assert_eq!(
                sent[2..],
                [
                    ("REQUEST".into(), "me@example.com".into(), "ann@example.com".into(), "1".into()),
                    ("REQUEST".into(), "me@example.com".into(), "cy@example.com".into(), "1".into()),
                    ("CANCEL".into(), "me@example.com".into(), "bo@example.com".into(), "1".into())
                ]
            );

            // Only the people: Send to new people only.
            let mut e = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            e.attendees.push(guest("dee@example.com", Role::Optional));
            queue(&j, told(cal, Change::Update(e), Notify::Changed, None)).unwrap();
            run(&j);
            assert_eq!(outbox(&j)[5..], [("REQUEST".into(), "me@example.com".into(), "dee@example.com".into(), "2".into())], "the others get nothing");

            queue(&j, told(cal, Change::Delete { uid: "p1@jolta.app".into() }, Notify::All, Some("Sorry"))).unwrap();
            run(&j);
            let cancels: Vec<_> = outbox(&j)[6..].iter().map(|(m, _, to, seq)| (m.clone(), to.clone(), seq.clone())).collect();
            assert_eq!(
                cancels,
                [("CANCEL".into(), "ann@example.com".into(), "3".into()), ("CANCEL".into(), "cy@example.com".into(), "3".into()), ("CANCEL".into(), "dee@example.com".into(), "3".into())]
            );
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        #[test]
        fn without_an_account_to_send_from_nothing_is_sent_but_dont_send_saves() {
            let (j, cal) = local("u5b-nosender", false);
            let err = queue(&j, told(cal, Change::Create(vec![meeting(&["ann@example.com"])]), Notify::All, None)).unwrap_err();
            assert!(err.to_string().contains("Choose the account Jolta sends the invitations for Here from"), "{err}");
            queue(&j, told(cal, Change::Create(vec![meeting(&["ann@example.com"])]), Notify::Nobody, None)).unwrap();
            run(&j);
            assert!(outbox(&j).is_empty());
            let err = queue(&j, told(cal, Change::Create(vec![EventData { uid: "p2".into(), ..meeting(&["ann@example.com"]) }]), Notify::Outside, None)).unwrap_err();
            assert!(err.to_string().contains("mails all of them or none"), "{err}");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Check 1: a meeting someone else organizes, imported into a
        /// calendar on this Mac, is never mailed as the user's.
        #[test]
        fn someone_elses_meeting_here_takes_only_the_users_own_settings() {
            let (j, cal) = local("u5b-other", true);
            let theirs = EventData {
                organizer: Some(Person { address: "ann@example.com".into(), name: "Ann".into() }),
                attendees: vec![guest("me@example.com", Role::Required), guest("bo@example.com", Role::Required)],
                ..meeting(&[])
            };
            queue(&j, told(cal, Change::Create(vec![theirs.clone()]), Notify::Nobody, None)).unwrap();
            let renamed = EventData { title: "Mine now".into(), ..theirs };
            let err = queue(&j, told(cal, Change::Update(renamed), Notify::All, None)).unwrap_err();
            assert!(err.to_string().contains("Ann organizes Planning"), "{err}");
            run(&j);
            assert!(outbox(&j).is_empty());
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// A guest's iMIP reply mail from `from`, answering as `attendee`.
        fn reply(from: &str, attendee: &str, partstat: &str, rid: Option<&str>, stamp: &str, extra: &str) -> Vec<u8> {
            let rid = rid.map_or(String::new(), |r| format!("RECURRENCE-ID:{r}\r\n"));
            format!(
                "From: {from}\r\nTo: me@example.com\r\nSubject: Reply\r\nMessage-ID: <{}@example.com>\r\nDate: Mon, 16 Nov 2026 10:00:00 +0000\r\n\
                 MIME-Version: 1.0\r\nContent-Type: text/calendar; method=REPLY; charset=utf-8\r\n\r\n\
                 BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Test//EN\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:p1@jolta.app\r\n{rid}\
                 DTSTAMP:{stamp}\r\nORGANIZER:mailto:me@example.com\r\nATTENDEE;PARTSTAT={partstat}:mailto:{attendee}\r\n{extra}END:VEVENT\r\nEND:VCALENDAR\r\n",
                crate::calendar::ops::new_uid().unwrap()
            )
            .into_bytes()
        }

        fn read(j: &Jolta) -> Vec<CalendarId> {
            read_replies(j, AccountId(1), &horizon()).unwrap()
        }

        fn answers(j: &Jolta, cal: CalendarId, rid: Option<&Moment>) -> Vec<(String, crate::calendar::Response, String)> {
            let e = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", rid)).unwrap().unwrap().event;
            e.attendees.into_iter().map(|a| (a.person.address, a.response, a.comment)).collect()
        }

        fn sent_meeting(j: &Jolta, cal: CalendarId, weekly: bool) {
            let mut m = meeting(&["ann@example.com", "bo@example.com"]);
            if weekly {
                m.recurrence = Some(crate::calendar::Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=4".into()], ..Default::default() });
            }
            queue(j, told(cal, Change::Create(vec![m]), Notify::All, None)).unwrap();
            run(j);
        }

        #[test]
        fn a_guests_reply_lands_in_the_organizers_meeting_once_and_never_over_a_newer_one() {
            use crate::calendar::Response::{Accepted, Declined, NeedsAction};
            let (j, cal) = local("u5b-reply", true);
            sent_meeting(&j, cal, false);
            import(&j, &reply("ann@example.com", "ann@example.com", "DECLINED", None, "20261116T100000Z", "COMMENT:Away\r\n"));
            assert_eq!(read(&j), [cal]);
            assert_eq!(answers(&j, cal, None)[0], ("ann@example.com".into(), Declined, "Away".into()));
            assert!(read(&j).is_empty(), "each reply is read once");

            // Check 4: an older answer arriving later changes nothing.
            import(&j, &reply("ann@example.com", "ann@example.com", "ACCEPTED", None, "20261116T090000Z", ""));
            read(&j);
            assert_eq!(answers(&j, cal, None)[0].1, Declined);
            import(&j, &reply("ann@example.com", "ann@example.com", "ACCEPTED", None, "20261116T110000Z", ""));
            read(&j);
            assert_eq!(answers(&j, cal, None)[0], ("ann@example.com".into(), Accepted, String::new()));

            // Check 3: only the guest answers for the guest, and only guests.
            import(&j, &reply("ann@example.com", "bo@example.com", "DECLINED", None, "20261116T120000Z", ""));
            import(&j, &reply("eve@example.com", "eve@example.com", "ACCEPTED", None, "20261116T120000Z", ""));
            assert!(read(&j).is_empty());
            assert_eq!(answers(&j, cal, None)[1].1, NeedsAction, "Ann can't answer for Bo");
            assert_eq!(answers(&j, cal, None).len(), 2, "a stranger isn't added");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Check 2: an occurrence named in UTC is the Brussels occurrence.
        #[test]
        fn a_reply_for_one_occurrence_answers_that_occurrence_alone() {
            use crate::calendar::Response::{Accepted, NeedsAction, Tentative};
            let (j, cal) = local("u5b-reply-one", true);
            sent_meeting(&j, cal, true);
            import(&j, &reply("bo@example.com", "bo@example.com", "TENTATIVE", Some("20261201T130000Z"), "20261116T100000Z", ""));
            assert_eq!(read(&j), [cal]);
            let week2 = Moment::Time(Local { wall: NaiveDateTime::parse_from_str("2026-12-01 14:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) });
            assert_eq!(answers(&j, cal, Some(&week2))[1].1, Tentative, "the occurrence holds its own answer");
            assert_eq!(answers(&j, cal, None)[1].1, NeedsAction, "the series keeps its own");
            import(&j, &reply("bo@example.com", "bo@example.com", "ACCEPTED", None, "20261116T090000Z", ""));
            read(&j);
            assert_eq!((answers(&j, cal, None)[1].1, answers(&j, cal, Some(&week2))[1].1), (Accepted, Tentative), "an older series answer leaves a newer occurrence answer");
            import(&j, &reply("bo@example.com", "bo@example.com", "ACCEPTED", Some("20261229T130000Z"), "20261116T110000Z", ""));
            import(&j, &reply("bo@example.com", "bo@example.com", "ACCEPTED", Some("20261202T130000Z"), "20261116T110000Z", ""));
            assert!(read(&j).is_empty(), "week 5 and a Wednesday aren't occurrences");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Check 1: a reply counts only for a meeting the receiving account
        /// organizes here.
        #[test]
        fn a_reply_to_someone_elses_meeting_changes_nothing() {
            let (j, cal) = local("u5b-reply-theirs", true);
            let theirs = EventData {
                organizer: Some(Person { address: "zed@example.com".into(), name: "Zed".into() }),
                attendees: vec![guest("me@example.com", Role::Required), guest("ann@example.com", Role::Required)],
                ..meeting(&[])
            };
            queue(&j, told(cal, Change::Create(vec![theirs]), Notify::Nobody, None)).unwrap();
            run(&j);
            import(&j, &reply("ann@example.com", "ann@example.com", "ACCEPTED", None, "20261116T100000Z", ""));
            assert!(read(&j).is_empty());
            assert_eq!(answers(&j, cal, None)[1].1, crate::calendar::Response::NeedsAction);
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        #[test]
        fn a_new_time_asks_again_and_an_answer_to_the_old_time_waits_out() {
            use crate::calendar::Response::{Accepted, NeedsAction};
            let (j, cal) = local("u5b-reply-moved", true);
            sent_meeting(&j, cal, false);
            import(&j, &reply("ann@example.com", "ann@example.com", "ACCEPTED", None, "20261116T100000Z", "SEQUENCE:0\r\nDTSTART;TZID=Europe/Brussels:20261124T140000\r\n"));
            read(&j);
            assert_eq!(answers(&j, cal, None)[0].1, Accepted);
            let mut moved = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            moved.span = Span::Timed {
                start: Local { wall: NaiveDateTime::parse_from_str("2026-11-24 16:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) },
                end: Local { wall: NaiveDateTime::parse_from_str("2026-11-24 16:30", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) },
            };
            let edit = told(cal, Change::Update(moved), Notify::All, None);
            j.store.write(|tx| ops::queue(tx, &edit, Utc::now().timestamp(), &horizon())).unwrap();
            assert_eq!(answers(&j, cal, None)[0].1, NeedsAction, "a new time asks again");
            import(&j, &reply("bo@example.com", "bo@example.com", "ACCEPTED", None, "20261116T110000Z", "SEQUENCE:1\r\nDTSTART;TZID=Europe/Brussels:20261124T160000\r\n"));
            assert!(read(&j).is_empty(), "the reply waits while the update waits to go");
            assert_eq!(answers(&j, cal, None)[1].1, NeedsAction);
            j.store.write(|tx| Ok(tx.execute("UPDATE calendar_op SET send_after = 0", [])?)).unwrap();
            run(&j);
            assert_eq!(read(&j), [cal], "then it is read");
            assert_eq!(answers(&j, cal, None)[1].1, Accepted);
            import(&j, &reply("ann@example.com", "ann@example.com", "DECLINED", None, "20261116T120000Z", "SEQUENCE:0\r\nDTSTART;TZID=Europe/Brussels:20261124T140000\r\n"));
            read(&j);
            assert_eq!(answers(&j, cal, None)[0].1, NeedsAction, "an answer to the old time doesn't count");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// The VEVENTs of each mail, as (RECURRENCE-ID or "", SEQUENCE).
        fn vevents(j: &Jolta) -> Vec<(String, Vec<(String, String)>)> {
            let blobs: Vec<String> = j.store.read(|c| {
                let mut stmt = c.prepare("SELECT blob FROM outbox ORDER BY id")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            }).unwrap();
            blobs.iter().map(|b| {
                let raw = String::from_utf8_lossy(&j.blobs.get(b).unwrap()).replace("\r\n ", "");
                let ics = &raw[raw.find("BEGIN:VCALENDAR").unwrap()..];
                let obj = Object::parse(ics);
                let cal = obj.calendar().unwrap();
                let method = cal.prop("METHOD").unwrap().value().to_string();
                let list = cal.components().filter(|c| c.name() == "VEVENT")
                    .map(|e| (e.prop("RECURRENCE-ID").map(|p| p.value().to_string()).unwrap_or_default(), e.prop("SEQUENCE").unwrap().value().to_string()))
                    .collect();
                (method, list)
            }).collect()
        }

        /// Check 2: one occurrence moved, one cancelled, then the series.
        #[test]
        fn a_series_tells_of_one_occurrence_alone_and_of_the_series_with_its_changes() {
            let (j, cal) = local("u5b-series", true);
            let mut series = meeting(&["ann@example.com"]);
            series.recurrence = Some(crate::calendar::Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=4".into()], ..Default::default() });
            queue(&j, told(cal, Change::Create(vec![series]), Notify::All, None)).unwrap();
            run(&j);
            let week2 = Moment::Time(Local { wall: NaiveDateTime::parse_from_str("2026-12-01 14:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) });
            let master = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            let mut moved = crate::calendar::series::occurrence(std::slice::from_ref(&master), &week2, tz::here()).unwrap();
            moved.span = Span::Timed {
                start: Local { wall: NaiveDateTime::parse_from_str("2026-12-01 15:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) },
                end: Local { wall: NaiveDateTime::parse_from_str("2026-12-01 15:30", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) },
            };
            queue(&j, told(cal, Change::Update(moved), Notify::All, None)).unwrap();
            run(&j);
            let week3 = Moment::Time(Local { wall: NaiveDateTime::parse_from_str("2026-12-08 14:00", "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) });
            queue(&j, told(cal, Change::DeleteOccurrence { uid: "p1@jolta.app".into(), recurrence_id: week3 }, Notify::All, None)).unwrap();
            run(&j);
            let mut renamed = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            renamed.title = "Planning weekly".into();
            queue(&j, told(cal, Change::Update(renamed), Notify::All, None)).unwrap();
            run(&j);
            let mails = vevents(&j);
            let row = |m: &str, list: &[(&str, &str)]| (m.to_string(), list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>());
            assert_eq!(
                mails,
                [
                    row("REQUEST", &[("", "0")]),
                    row("REQUEST", &[("20261201T140000", "1")]),
                    row("CANCEL", &[("20261208T140000", "1")]),
                    row("REQUEST", &[("", "2"), ("20261201T140000", "1")]),
                ],
                "the moved occurrence alone, the cancelled one alone with a higher SEQUENCE, then the series with its moved occurrence"
            );
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        fn at(s: &str) -> Local {
            Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
        }

        fn weekly(j: &Jolta, cal: CalendarId) -> EventData {
            let mut series = meeting(&["ann@example.com", "bo@example.com"]);
            series.recurrence = Some(crate::calendar::Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=4".into()], ..Default::default() });
            queue(j, told(cal, Change::Create(vec![series]), Notify::All, None)).unwrap();
            run(j);
            j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event
        }

        /// Sol S1: a series update carries its changed occurrences as it
        /// left them, not a later move still inside its undo window.
        #[test]
        fn a_series_update_never_carries_a_later_edit_still_to_be_undone() {
            let (j, cal) = local("u5b-s1", true);
            let mut renamed = weekly(&j, cal);
            renamed.title = "Planning weekly".into();
            queue(&j, told(cal, Change::Update(renamed), Notify::All, None)).unwrap();
            let week2 = Moment::Time(at("2026-12-01 14:00"));
            let master = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            let mut moved = crate::calendar::series::occurrence(std::slice::from_ref(&master), &week2, tz::here()).unwrap();
            moved.span = Span::Timed { start: at("2026-12-01 16:00"), end: at("2026-12-01 16:30") };
            let now = Utc::now().timestamp();
            let pending = j.store.write(|tx| ops::queue(tx, &told(cal, Change::Update(moved), Notify::All, None), now, &horizon())).unwrap().unwrap();
            run(&j);
            assert_eq!(j.store.write(|tx| ops::undo(tx, pending, now, &horizon())).unwrap(), ops::Undone::Dropped);
            let rename: Vec<_> = vevents(&j).into_iter().skip(2).collect();
            assert!(rename.iter().all(|(_, list)| list.len() == 1), "the rename went without the move taken back: {rename:?}");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Sol S2: a late reply to a cancelled occurrence doesn't bring it back.
        #[test]
        fn a_reply_to_a_cancelled_occurrence_changes_nothing() {
            let (j, cal) = local("u5b-s2", true);
            weekly(&j, cal);
            queue(&j, told(cal, Change::DeleteOccurrence { uid: "p1@jolta.app".into(), recurrence_id: Moment::Time(at("2026-12-08 14:00")) }, Notify::All, None)).unwrap();
            run(&j);
            import(&j, &reply("ann@example.com", "ann@example.com", "ACCEPTED", Some("20261208T130000Z"), "20261116T100000Z", ""));
            assert!(read(&j).is_empty());
            assert_eq!(j.store.read(|c| ops::series(c, cal, "p1@jolta.app")).unwrap().len(), 1, "no row for the cancelled occurrence");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Sol S3: the guest taken off is the one their cancellation names.
        #[test]
        fn a_removed_guests_cancellation_names_them() {
            let (j, cal) = local("u5b-s3", true);
            sent_meeting(&j, cal, false);
            let mut e = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            e.attendees.retain(|a| a.person.address != "bo@example.com");
            queue(&j, told(cal, Change::Update(e), Notify::Changed, None)).unwrap();
            run(&j);
            let blob: String = j.store.read(|c| Ok(c.query_row("SELECT blob FROM outbox ORDER BY id DESC LIMIT 1", [], |r| r.get(0))?)).unwrap();
            let raw = String::from_utf8_lossy(&j.blobs.get(&blob).unwrap()).replace("\r\n ", "");
            let ics = &raw[raw.find("BEGIN:VCALENDAR").unwrap()..];
            let obj = Object::parse(ics);
            let ev = obj.calendar().unwrap().components().find(|c| c.name() == "VEVENT").unwrap().clone();
            let attendees: Vec<&str> = ev.props().filter(|p| p.is("ATTENDEE")).filter_map(|p| p.address()).collect();
            assert_eq!(attendees, ["bo@example.com"]);
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Sol S4: an older answer for one occurrence loses to a newer one
        /// for the whole series.
        #[test]
        fn an_older_occurrence_reply_loses_to_a_newer_series_reply() {
            use crate::calendar::Response::Accepted;
            let (j, cal) = local("u5b-s4", true);
            weekly(&j, cal);
            import(&j, &reply("ann@example.com", "ann@example.com", "ACCEPTED", None, "20261116T110000Z", ""));
            read(&j);
            import(&j, &reply("ann@example.com", "ann@example.com", "DECLINED", Some("20261201T130000Z"), "20261116T100000Z", ""));
            assert!(read(&j).is_empty());
            let week2 = Moment::Time(at("2026-12-01 14:00"));
            let shown = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", Some(&week2))).unwrap();
            assert!(shown.is_none_or(|s| s.event.attendees[0].response == Accepted), "week 2 keeps the series' newer accept");
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Sol S5: an answer to the old time waits out an edit that Undo
        /// may take back, and counts once it is.
        #[test]
        fn a_reply_waits_out_an_edit_that_undo_takes_back() {
            use crate::calendar::Response::Accepted;
            let (j, cal) = local("u5b-s5", true);
            sent_meeting(&j, cal, false);
            let mut moved = j.store.read(|c| ops::stored(c, cal, "p1@jolta.app", None)).unwrap().unwrap().event;
            moved.span = Span::Timed { start: at("2026-11-24 16:00"), end: at("2026-11-24 16:30") };
            let now = Utc::now().timestamp();
            let op = j.store.write(|tx| ops::queue(tx, &told(cal, Change::Update(moved), Notify::All, None), now, &horizon())).unwrap().unwrap();
            import(&j, &reply("ann@example.com", "ann@example.com", "ACCEPTED", None, "20261116T100000Z", "SEQUENCE:0\r\nDTSTART;TZID=Europe/Brussels:20261124T140000\r\n"));
            assert!(read(&j).is_empty(), "it waits");
            j.store.write(|tx| ops::undo(tx, op, now, &horizon())).unwrap();
            assert_eq!(read(&j), [cal], "the time it answered is the meeting's again");
            assert_eq!(answers(&j, cal, None)[0].1, Accepted);
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Sol S5, round 2: a reply waits out a delete Undo takes back.
        #[test]
        fn a_reply_waits_out_a_delete_that_undo_takes_back() {
            use crate::calendar::Response::Accepted;
            let (j, cal) = local("u5b-s5b", true);
            sent_meeting(&j, cal, false);
            let now = Utc::now().timestamp();
            let op = j.store.write(|tx| ops::queue(tx, &told(cal, Change::Delete { uid: "p1@jolta.app".into() }, Notify::All, None), now, &horizon())).unwrap().unwrap();
            // The delete lands after the reader listed the reply.
            let raw = reply("ann@example.com", "ann@example.com", "ACCEPTED", None, "20261116T100000Z", "");
            let answer = crate::calendar::invitation::find(&raw, &[]).remove(0);
            let account = j.store.read(|c| store::account(c, AccountId(1))).unwrap();
            assert_eq!(j.store.write(|tx| apply_reply(tx, &account, "ann@example.com", &answer, &horizon())).unwrap(), Read::Wait, "it waits");
            j.store.write(|tx| ops::undo(tx, op, now, &horizon())).unwrap();
            import(&j, &raw);
            assert_eq!(read(&j), [cal], "the meeting is back, and so is the answer");
            assert_eq!(answers(&j, cal, None)[0].1, Accepted);
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        /// Sol S6: the same new meeting saved twice queues one op.
        #[test]
        fn the_same_new_meeting_twice_queues_once() {
            let (j, cal) = local("u5b-s6", true);
            let edit = told(cal, Change::Create(vec![meeting(&["ann@example.com"])]), Notify::All, None);
            assert!(queue(&j, edit.clone()).unwrap().is_some());
            assert_eq!(queue(&j, edit).unwrap(), None);
            std::fs::remove_dir_all(&j.data_dir).ok();
        }

        #[test]
        fn the_editor_asks_for_the_sending_account_once_then_names_it() {
            let (j, cal) = local("u5b-draft", false);
            let here = tz::here();
            let mut d = j.store.read(|c| draft::draft_for_new(c, Some(cal), None, Utc::now(), here)).unwrap();
            d.title = "Planning".into();
            d.people = vec![guest("ann@example.com", Role::Required)];
            let plan = j.store.read(|c| draft::plan_save(c, &d, here)).unwrap();
            assert_eq!(plan.senders.iter().map(|s| s.address.as_str()).collect::<Vec<_>>(), ["me@example.com"]);
            let dialog = plan.dialog.unwrap();
            assert_eq!((dialog.title.as_str(), dialog.message), ("Send invitations to 1 person?", true));
            j.store.write(|tx| store::set_calendar_send_from(tx, cal, Some(AccountId(1)))).unwrap();
            let plan = j.store.read(|c| draft::plan_save(c, &d, here)).unwrap();
            assert!(plan.senders.is_empty(), "asked once per calendar");
            assert_eq!(plan.dialog.unwrap().body, "ann@example.com gets an invitation. Jolta sends them from me@example.com.");
            let saved = j.store.write(|tx| draft::save_draft(tx, &d, None, Some(Notify::All), Some("Agenda attached".into()), Utc::now().timestamp() - 20, &horizon(), here)).unwrap();
            assert_eq!(saved.message, "Invitation sent to 1 person");
            run(&j);
            let raw = j.store.read(|c| Ok(c.query_row("SELECT blob FROM outbox", [], |r| r.get::<_, String>(0))?)).unwrap();
            let text = String::from_utf8_lossy(&j.blobs.get(&raw).unwrap()).into_owned();
            assert!(text.contains("COMMENT:Agenda attached"), "the message travels with it");

            let event = j.store.read(|c| ops::stored(c, cal, &d.uid, None)).unwrap().unwrap().id;
            let delete = j.store.read(|c| draft::plan_delete(c, event, None, here)).unwrap();
            let [(_, cancel)] = delete.cancel.as_slice() else { panic!("{delete:?}") };
            assert_eq!((cancel.title.as_str(), cancel.body.as_str(), cancel.message), ("Cancel the meeting for 1 person?", "Everyone gets a cancellation from me@example.com.", true));
            std::fs::remove_dir_all(&j.data_dir).ok();
        }
    }
}
