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
use super::{Calendar, CalendarSource, EventData, EventStatus, Moment, Person, Span, Zone, recur};
use crate::compose::Built;
use crate::model::Account;
use crate::{Result, store};

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
            (Notice::Cancellation, Write::Update { event, .. }) => imip_cancel(event, event.sequence, &organizer, &to, message, seed(event, event.sequence))?,
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
pub fn prepare(write: &mut Write, sender: Option<&Account>) {
    let stamp = |e: &mut EventData| {
        if let Some(a) = sender
            && e.organizer.is_none()
            && e.attendees.iter().any(|g| !g.is_me && !g.person.address.eq_ignore_ascii_case(&a.address))
        {
            e.organizer = Some(organizer_of(a));
            e.organizer_is_me = true;
        }
    };
    match write {
        Write::Create(events) => events.iter_mut().for_each(stamp),
        Write::Update { base, event, .. } => {
            stamp(event);
            if [&*base, &*event].iter().any(|e| e.organizer.is_some() && !e.attendees.is_empty()) {
                next_sequence(base, event);
            }
        }
        Write::Cancel { series, .. } => series.sequence += 1,
        Write::Delete(_) => {}
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
/// repeat rule, place, title, description or status. A new guest list
/// alone keeps it, as RFC 5546 allows.
pub fn next_sequence(base: &EventData, event: &mut EventData) {
    let raises = [Field::Time, Field::Repeat, Field::Location, Field::Title, Field::Description, Field::Status];
    if raises.iter().any(|f| f.differs(base, event)) && event.sequence <= base.sequence {
        event.sequence = base.sequence + 1;
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
        assert_eq!(people.sequence, 2, "a new guest list alone keeps the version");
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

        use crate::calendar::invitation::tests::jolta;
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
            assert_eq!(outbox(&j)[5..], [("REQUEST".into(), "me@example.com".into(), "dee@example.com".into(), "1".into())], "the others get nothing");

            queue(&j, told(cal, Change::Delete { uid: "p1@jolta.app".into() }, Notify::All, Some("Sorry"))).unwrap();
            run(&j);
            let cancels: Vec<_> = outbox(&j)[6..].iter().map(|(m, _, to, seq)| (m.clone(), to.clone(), seq.clone())).collect();
            assert_eq!(
                cancels,
                [("CANCEL".into(), "ann@example.com".into(), "2".into()), ("CANCEL".into(), "cy@example.com".into(), "2".into()), ("CANCEL".into(), "dee@example.com".into(), "2".into())]
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
