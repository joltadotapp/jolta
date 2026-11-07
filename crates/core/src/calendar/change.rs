//! What a change to an event is: one guests must hear about, or the
//! user's own. The input to the Send an update? dialog and an op's notify.

use serde::{Deserialize, Serialize};

use super::{Attendee, EventData, Person};

/// A part of an event the user can change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Field {
    Time,
    Repeat,
    Location,
    MeetingLink,
    Title,
    Description,
    Status,
    People,
    Alarms,
    Color,
    Categories,
    ShowAs,
    Visibility,
}

impl Field {
    /// Guests hear about a change to it. Exchange mails them for a changed
    /// description too (C3 spec, section 2), which the plan's list missed.
    pub fn is_meeting(self) -> bool {
        !matches!(self, Field::Alarms | Field::Color | Field::Categories | Field::ShowAs | Field::Visibility)
    }

    /// Whether `a` and `b` differ in this field. People compare by address
    /// and role; answers are the guests' own and never count.
    pub fn differs(self, a: &EventData, b: &EventData) -> bool {
        match self {
            Field::Time => a.span != b.span,
            Field::Repeat => a.recurrence != b.recurrence,
            Field::Location => a.location != b.location,
            Field::MeetingLink => a.meeting.as_ref().map(|m| &m.join_url) != b.meeting.as_ref().map(|m| &m.join_url),
            Field::Title => a.title != b.title,
            Field::Description => (&a.description, a.description_html) != (&b.description, b.description_html),
            Field::Status => a.status != b.status,
            Field::People => roles(&a.attendees) != roles(&b.attendees),
            Field::Alarms => a.alarms != b.alarms,
            Field::Color => a.color != b.color,
            Field::Categories => a.categories != b.categories,
            Field::ShowAs => a.show_as != b.show_as,
            Field::Visibility => a.visibility != b.visibility,
        }
    }
}

pub const FIELDS: [Field; 13] = [
    Field::Time,
    Field::Repeat,
    Field::Location,
    Field::MeetingLink,
    Field::Title,
    Field::Description,
    Field::Status,
    Field::People,
    Field::Alarms,
    Field::Color,
    Field::Categories,
    Field::ShowAs,
    Field::Visibility,
];

fn roles(people: &[Attendee]) -> std::collections::BTreeMap<String, super::Role> {
    people.iter().map(|a| (a.person.address.to_lowercase(), a.role)).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Classified {
    /// Changes the guests hear about: time, repeat, location, meeting link,
    /// title, description, status.
    pub meeting: Vec<Field>,
    /// The user's own: alarms, color, categories, show as, visibility.
    pub own: Vec<Field>,
    pub added: Vec<Person>,
    pub removed: Vec<Person>,
    pub kept: Vec<Person>,
}

impl Classified {
    /// Nobody but the user would notice.
    pub fn is_own_only(&self) -> bool {
        self.meeting.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// Sorts the change from `before` to `after`. People are everyone on the
/// event except the user: an attendee marked as the user, or one of `me`.
pub fn classify(before: &EventData, after: &EventData, me: &[String]) -> Classified {
    let mut out = Classified::default();
    for field in FIELDS.into_iter().filter(|&f| f != Field::People && f.differs(before, after)) {
        if field.is_meeting() { &mut out.meeting } else { &mut out.own }.push(field);
    }
    let others = |e: &EventData| -> Vec<Person> {
        e.attendees
            .iter()
            .filter(|a| !a.is_me && !me.iter().any(|m| m.eq_ignore_ascii_case(&a.person.address)))
            .map(|a| a.person.clone())
            .collect()
    };
    let (was, now) = (others(before), others(after));
    let has = |list: &[Person], p: &Person| list.iter().any(|q| q.address.eq_ignore_ascii_case(&p.address));
    for p in &now {
        if has(&was, p) { &mut out.kept } else { &mut out.added }.push(p.clone());
    }
    out.removed = was.into_iter().filter(|p| !has(&now, p)).collect();
    out
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{
        Alarm, AlarmTrigger, EventStatus, Local, Meeting, MeetingProvider, Recurrence, Response, Role, ShowAs, Span,
        Visibility, Zone,
    };

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn person(address: &str) -> Attendee {
        Attendee {
            person: Person { address: address.into(), name: String::new() },
            role: Role::Required,
            response: Response::NeedsAction,
            comment: String::new(),
            is_me: false,
        }
    }

    fn meeting() -> EventData {
        EventData {
            title: "Standup".into(),
            location: "Room 1".into(),
            attendees: vec![
                Attendee { is_me: true, ..person("me@example.com") },
                person("ann@example.com"),
                person("bo@example.com"),
            ],
            ..EventData::new("u1", Span::Timed { start: at("2026-11-24 09:00"), end: at("2026-11-24 09:30") })
        }
    }

    #[test]
    fn each_field_lands_in_its_list() {
        let base = meeting();
        let cases: Vec<(&str, Box<dyn Fn(&mut EventData)>, Option<Field>, Option<Field>)> = vec![
            ("time", Box::new(|e| e.span = Span::Timed { start: at("2026-11-24 10:00"), end: at("2026-11-24 10:30") }), Some(Field::Time), None),
            ("repeat", Box::new(|e| e.recurrence = Some(Recurrence { rules: vec!["FREQ=WEEKLY".into()], ..Default::default() })), Some(Field::Repeat), None),
            ("location", Box::new(|e| e.location = "Room 2".into()), Some(Field::Location), None),
            (
                "meeting link",
                Box::new(|e| {
                    e.meeting = Some(Meeting {
                        provider: MeetingProvider::Meet,
                        join_url: "https://meet.google.com/abc-defg-hij".into(),
                        meeting_id: None,
                        passcode: None,
                        phones: vec![],
                        other_links: vec![],
                    })
                }),
                Some(Field::MeetingLink),
                None,
            ),
            ("title", Box::new(|e| e.title = "Daily".into()), Some(Field::Title), None),
            ("description", Box::new(|e| e.description = "Agenda".into()), Some(Field::Description), None),
            ("cancelled", Box::new(|e| e.status = EventStatus::Cancelled), Some(Field::Status), None),
            ("alarm", Box::new(|e| e.alarms = vec![Alarm { trigger: AlarmTrigger::MinutesBefore(10), email: false }]), None, Some(Field::Alarms)),
            ("color", Box::new(|e| e.color = Some("#ff0000".into())), None, Some(Field::Color)),
            ("categories", Box::new(|e| e.categories = vec!["Blue".into()]), None, Some(Field::Categories)),
            ("show as", Box::new(|e| e.show_as = ShowAs::Free), None, Some(Field::ShowAs)),
            ("visibility", Box::new(|e| e.visibility = Visibility::Private), None, Some(Field::Visibility)),
        ];
        for (name, change, meeting_field, own_field) in cases {
            let mut after = base.clone();
            change(&mut after);
            let c = classify(&base, &after, &[]);
            assert_eq!(c.meeting, meeting_field.into_iter().collect::<Vec<_>>(), "{name}");
            assert_eq!(c.own, own_field.into_iter().collect::<Vec<_>>(), "{name}");
            assert_eq!(c.is_own_only(), meeting_field.is_none(), "{name}");
            assert!(c.added.is_empty() && c.removed.is_empty(), "{name}");
        }
        assert_eq!(classify(&base, &base, &[]), Classified { kept: vec![base.attendees[1].person.clone(), base.attendees[2].person.clone()], ..Default::default() });
    }

    #[test]
    fn people_added_removed_and_kept_leave_the_user_out() {
        let before = meeting();
        let mut after = before.clone();
        after.attendees.retain(|a| a.person.address != "bo@example.com");
        after.attendees.push(person("cy@example.com"));
        after.attendees[1].response = Response::Accepted;
        let c = classify(&before, &after, &["ME@example.com".into()]);
        fn addresses(list: &[Person]) -> Vec<&str> { list.iter().map(|p| p.address.as_str()).collect() }
        assert_eq!(addresses(&c.added), ["cy@example.com"]);
        assert_eq!(addresses(&c.removed), ["bo@example.com"]);
        assert_eq!(addresses(&c.kept), ["ann@example.com"], "an answer is no change to the people");
        assert!(c.meeting.is_empty() && !c.is_own_only());

        let mut alias = before.clone();
        alias.attendees[0].is_me = false;
        alias.attendees[0].person.address = "alias@example.com".into();
        let c = classify(&alias, &alias, &["alias@example.com".into()]);
        assert_eq!(addresses(&c.kept), ["ann@example.com", "bo@example.com"], "one of the user's addresses is never a guest");

        let mut renamed_case = before.clone();
        renamed_case.attendees[1].person.address = "Ann@Example.com".into();
        assert!(classify(&before, &renamed_case, &[]).added.is_empty(), "addresses compare without case");
    }
}
