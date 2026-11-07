//! The three-way merge of an edit with a change made elsewhere meanwhile,
//! for a write the server refused with 412.

use super::change::Field;
use super::{Attendee, EventData};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merged {
    pub event: EventData,
    /// Fields both sides changed to different values. The server's value
    /// stayed; the user's is in their edit.
    pub lost: Vec<Field>,
}

/// Merges `mine`, the user's edit of `base`, with `theirs`, the server's
/// version now. A field only the user changed keeps the user's value, one
/// only the server changed the server's, and one both changed differently
/// the server's, listed in `lost`. People merge one by one by address, and
/// a guest's answer and comment always come from the server. Everything
/// Jolta doesn't edit, such as the organizer and the sequence, is the server's.
pub fn three_way(base: &EventData, mine: &EventData, theirs: &EventData) -> Merged {
    let mut lost = Vec::new();
    let mut event = theirs.clone();
    let mut pick = |field: Field| pick(field, base, mine, theirs, &mut lost);
    if pick(Field::Title) {
        event.title.clone_from(&mine.title);
    }
    if pick(Field::Location) {
        event.location.clone_from(&mine.location);
    }
    if pick(Field::Description) {
        event.description.clone_from(&mine.description);
        event.description_html = mine.description_html;
    }
    if pick(Field::Time) {
        event.span = mine.span;
    }
    if pick(Field::Repeat) {
        event.recurrence.clone_from(&mine.recurrence);
    }
    if pick(Field::Alarms) {
        event.alarms.clone_from(&mine.alarms);
    }
    if pick(Field::ShowAs) {
        event.show_as = mine.show_as;
    }
    if pick(Field::Visibility) {
        event.visibility = mine.visibility;
    }
    if pick(Field::Categories) {
        event.categories.clone_from(&mine.categories);
    }
    if pick(Field::Color) {
        event.color.clone_from(&mine.color);
    }
    if pick(Field::MeetingLink) {
        event.meeting.clone_from(&mine.meeting);
    }
    let (attendees, people_lost) = people(&base.attendees, &mine.attendees, &theirs.attendees);
    event.attendees = attendees;
    if people_lost {
        lost.push(Field::People);
    }
    lost.sort();
    Merged { event, lost }
}

/// Whether the user's value wins: only the user changed the field. When
/// both changed it to different values, the field goes into `lost`.
fn pick(field: Field, base: &EventData, mine: &EventData, theirs: &EventData, lost: &mut Vec<Field>) -> bool {
    let (i_changed, they_changed) = (field.differs(base, mine), field.differs(base, theirs));
    if i_changed && they_changed && field.differs(mine, theirs) {
        lost.push(field);
    }
    i_changed && !they_changed
}

/// Each person's presence and role merged on their own. A person on the
/// server keeps the server's record (name, answer, comment) with the
/// merged role; one only the user added keeps the user's.
fn people(base: &[Attendee], mine: &[Attendee], theirs: &[Attendee]) -> (Vec<Attendee>, bool) {
    let find = |list: &[Attendee], address: &str| list.iter().find(|a| a.person.address.eq_ignore_ascii_case(address)).cloned();
    let mut addresses: Vec<&str> = theirs.iter().map(|a| a.person.address.as_str()).collect();
    for a in mine {
        if !addresses.iter().any(|x| x.eq_ignore_ascii_case(&a.person.address)) {
            addresses.push(&a.person.address);
        }
    }
    for a in base {
        if !addresses.iter().any(|x| x.eq_ignore_ascii_case(&a.person.address)) {
            addresses.push(&a.person.address);
        }
    }
    let mut lost = false;
    let mut out = Vec::new();
    for address in addresses {
        let (b, m, t) = (find(base, address), find(mine, address), find(theirs, address));
        let role = |a: &Option<Attendee>| a.as_ref().map(|a| a.role);
        let (i_changed, they_changed) = (role(&b) != role(&m), role(&b) != role(&t));
        lost |= i_changed && they_changed && role(&m) != role(&t);
        let merged = if i_changed && !they_changed { role(&m) } else { role(&t) };
        if let Some(r) = merged {
            let mut person = t.or(m).expect("a merged role comes from one side");
            person.role = r;
            out.push(person);
        }
    }
    (out, lost)
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{
        Alarm, AlarmTrigger, EventStatus, Local, Meeting, MeetingProvider, Person, Recurrence, Response, Role, ShowAs, Span,
        Visibility, Zone,
    };

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn guest(address: &str, role: Role, response: Response) -> Attendee {
        Attendee {
            person: Person { address: address.into(), name: address.split('@').next().unwrap().into() },
            role,
            response,
            comment: String::new(),
            is_me: false,
        }
    }

    fn base() -> EventData {
        EventData {
            title: "Standup".into(),
            location: "Room 1".into(),
            attendees: vec![guest("ann@example.com", Role::Required, Response::NeedsAction)],
            ..EventData::new("u1", Span::Timed { start: at("2026-11-24 09:00"), end: at("2026-11-24 09:30") })
        }
    }

    type Setter = fn(&mut EventData, u8);

    /// Each field with a setter that gives it a value per number.
    fn fields() -> Vec<(Field, Setter)> {
        vec![
            (Field::Title, |e, n| e.title = format!("Title {n}")),
            (Field::Location, |e, n| e.location = format!("Place {n}")),
            (Field::Description, |e, n| e.description = format!("Agenda {n}")),
            (Field::Time, |e, n| {
                let h = 10 + u32::from(n);
                e.span = Span::Timed { start: at(&format!("2026-11-24 {h}:00")), end: at(&format!("2026-11-24 {h}:30")) }
            }),
            (Field::Repeat, |e, n| e.recurrence = Some(Recurrence { rules: vec![format!("FREQ=DAILY;COUNT={n}")], ..Default::default() })),
            (Field::Alarms, |e, n| e.alarms = vec![Alarm { trigger: AlarmTrigger::MinutesBefore(i64::from(n)), email: false }]),
            (Field::ShowAs, |e, n| e.show_as = [ShowAs::Free, ShowAs::Tentative, ShowAs::OutOfOffice][usize::from(n) % 3]),
            (Field::Visibility, |e, n| e.visibility = [Visibility::Private, Visibility::Public, Visibility::Confidential][usize::from(n) % 3]),
            (Field::Categories, |e, n| e.categories = vec![format!("Cat {n}")]),
            (Field::Color, |e, n| e.color = Some(format!("#00000{n}"))),
            (Field::MeetingLink, |e, n| {
                e.meeting = Some(Meeting {
                    provider: MeetingProvider::Zoom,
                    join_url: format!("https://zoom.us/j/{n}"),
                    meeting_id: None,
                    passcode: None,
                    phones: vec![],
                    other_links: vec![],
                })
            }),
        ]
    }

    #[test]
    fn each_field_only_mine_only_theirs_both_same_both_different() {
        for (field, set) in fields() {
            let b = base();
            let with = |n: u8| {
                let mut e = base();
                set(&mut e, n);
                e
            };

            let m = three_way(&b, &with(1), &b);
            assert_eq!(m, Merged { event: with(1), lost: vec![] }, "{field:?}: only mine");

            let m = three_way(&b, &b, &with(2));
            assert_eq!(m, Merged { event: with(2), lost: vec![] }, "{field:?}: only theirs");

            let m = three_way(&b, &with(1), &with(1));
            assert_eq!(m, Merged { event: with(1), lost: vec![] }, "{field:?}: both the same");

            let m = three_way(&b, &with(1), &with(2));
            assert_eq!(m, Merged { event: with(2), lost: vec![field] }, "{field:?}: both different, the server's stays");
        }
    }

    #[test]
    fn different_fields_from_each_side_combine() {
        let b = base();
        let mine = EventData { title: "Daily".into(), ..b.clone() };
        let theirs = EventData { location: "Room 9".into(), sequence: 3, status: EventStatus::Tentative, ..b.clone() };
        let m = three_way(&b, &mine, &theirs);
        assert_eq!((m.event.title.as_str(), m.event.location.as_str(), m.event.sequence, m.event.status), ("Daily", "Room 9", 3, EventStatus::Tentative));
        assert!(m.lost.is_empty());
    }

    #[test]
    fn people_merge_by_address_and_answers_come_from_the_server() {
        let b = EventData {
            attendees: vec![
                guest("ann@example.com", Role::Required, Response::NeedsAction),
                guest("bo@example.com", Role::Required, Response::NeedsAction),
                guest("cy@example.com", Role::Required, Response::NeedsAction),
            ],
            ..base()
        };
        let mut mine = b.clone();
        mine.attendees[0].role = Role::Optional;
        mine.attendees[0].response = Response::Declined;
        mine.attendees.retain(|a| a.person.address != "bo@example.com");
        mine.attendees.push(guest("dee@example.com", Role::Required, Response::NeedsAction));
        let mut theirs = b.clone();
        theirs.attendees[0].response = Response::Accepted;
        theirs.attendees[0].comment = "See you".into();
        theirs.attendees.retain(|a| a.person.address != "cy@example.com");
        theirs.attendees.push(guest("eve@example.com", Role::Optional, Response::Tentative));

        let m = three_way(&b, &mine, &theirs);
        let got: Vec<(&str, Role, Response, &str)> =
            m.event.attendees.iter().map(|a| (a.person.address.as_str(), a.role, a.response, a.comment.as_str())).collect();
        assert_eq!(
            got,
            [
                ("ann@example.com", Role::Optional, Response::Accepted, "See you"),
                ("eve@example.com", Role::Optional, Response::Tentative, ""),
                ("dee@example.com", Role::Required, Response::NeedsAction, ""),
            ],
            "my role for Ann with the server's answer; Bo removed by me, Cy by them; Dee and Eve added by each side"
        );
        assert!(m.lost.is_empty());

        let mut mine = b.clone();
        mine.attendees[1].role = Role::Optional;
        let mut theirs = b.clone();
        theirs.attendees.remove(1);
        let m = three_way(&b, &mine, &theirs);
        assert_eq!(m.lost, [Field::People], "I changed Bo's role, they removed Bo");
        assert!(!m.event.attendees.iter().any(|a| a.person.address == "bo@example.com"), "the server's side stays");

        let mut upper = b.clone();
        upper.attendees[2].person.address = "CY@example.com".into();
        assert!(three_way(&b, &b, &upper).lost.is_empty(), "addresses compare without case");
    }
}
