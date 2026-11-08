//! What a change to an event is: one guests must hear about, or the
//! user's own. The input to the Send an update? dialog and an op's notify.

use serde::{Deserialize, Serialize};

use super::caps::{Capabilities, Hold};
use super::ops::Notify;
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
    /// What the organizer lets guests do: answer, propose times, see each
    /// other, invite others, edit.
    Options,
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
            Field::Options => a.options != b.options,
            Field::Alarms => a.alarms != b.alarms,
            Field::Color => a.color != b.color,
            Field::Categories => a.categories != b.categories,
            Field::ShowAs => a.show_as != b.show_as,
            Field::Visibility => a.visibility != b.visibility,
        }
    }

    /// Gives `to` this field's value in `from`.
    pub fn copy(self, to: &mut EventData, from: &EventData) {
        match self {
            Field::Time => to.span = from.span,
            Field::Repeat => to.recurrence.clone_from(&from.recurrence),
            Field::Location => to.location.clone_from(&from.location),
            Field::MeetingLink => to.meeting.clone_from(&from.meeting),
            Field::Title => to.title.clone_from(&from.title),
            Field::Description => {
                to.description.clone_from(&from.description);
                to.description_html = from.description_html;
            }
            Field::Status => to.status = from.status,
            Field::People => to.attendees.clone_from(&from.attendees),
            Field::Options => to.options = from.options,
            Field::Alarms => to.alarms.clone_from(&from.alarms),
            Field::Color => to.color.clone_from(&from.color),
            Field::Categories => to.categories.clone_from(&from.categories),
            Field::ShowAs => to.show_as = from.show_as,
            Field::Visibility => to.visibility = from.visibility,
        }
    }

    /// The word for it in "Your new time wasn't saved".
    pub fn noun(self) -> &'static str {
        match self {
            Field::Time => "time",
            Field::Repeat => "repeat",
            Field::Location => "place",
            Field::MeetingLink => "video call",
            Field::Title => "title",
            Field::Description => "description",
            Field::Status => "status",
            Field::People => "guests",
            Field::Options => "guest options",
            Field::Alarms => "alerts",
            Field::Color => "color",
            Field::Categories => "categories",
            Field::ShowAs => "show as",
            Field::Visibility => "visibility",
        }
    }
}

pub const FIELDS: [Field; 14] = [
    Field::Time,
    Field::Repeat,
    Field::Location,
    Field::MeetingLink,
    Field::Title,
    Field::Description,
    Field::Status,
    Field::People,
    Field::Options,
    Field::Alarms,
    Field::Color,
    Field::Categories,
    Field::ShowAs,
    Field::Visibility,
];

/// A changed occurrence after its series went from `base` to `event`: each
/// field the series changed that the occurrence still had the series' old
/// value for takes the new one. Its own time stays, as does every field it
/// changed itself. A guest who stays keeps the answer given to this
/// occurrence alone (check 2).
pub fn carried(base: &EventData, event: &EventData, occurrence: &EventData) -> EventData {
    let mut out = occurrence.clone();
    for f in FIELDS.into_iter().filter(|f| !matches!(f, Field::Time | Field::Repeat)) {
        if f.differs(base, event) && !f.differs(base, occurrence) {
            f.copy(&mut out, event);
        }
    }
    if Field::People.differs(base, event) && !Field::People.differs(base, occurrence) {
        let own = |a: &Attendee| occurrence.attendees.iter().find(|o| o.person.address.eq_ignore_ascii_case(&a.person.address));
        out.attendees = event.attendees.iter().map(|a| own(a).map_or_else(|| a.clone(), |o| Attendee { role: a.role, ..o.clone() })).collect();
    }
    out
}

fn roles(people: &[Attendee]) -> std::collections::BTreeMap<String, super::Role> {
    people.iter().map(|a| (a.person.address.to_lowercase(), a.role)).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Classified {
    /// Changes the guests hear about: time, repeat, location, meeting link,
    /// title, description, status, and a kept guest's role.
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
    let role = |e: &EventData, p: &Person| e.attendees.iter().find(|a| a.person.address.eq_ignore_ascii_case(&p.address)).map(|a| a.role);
    if out.kept.iter().any(|p| role(before, p) != role(after, p)) {
        out.meeting.push(Field::People);
    }
    out
}

/// The Send an update? dialog (C3 spec, section 5), its words all built
/// here so the app only lays them out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateDialog {
    /// "Send an update to 6 people?"
    pub title: String,
    /// Who gets what: "4 get the new time. Jan Peeters is new and gets an
    /// invitation."
    pub body: String,
    /// Under the body, where a message field would be: "To add a note, use
    /// Email guests after saving."
    pub note: Option<String>,
    /// A message field, where the mail carries the user's words.
    pub message: bool,
    /// The buttons, each with what it sends.
    pub choices: Vec<SendChoice>,
}

/// One button of a dialog that sends mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendChoice {
    pub label: String,
    /// What saving with it tells the guests; None for Keep editing.
    pub notify: Option<Notify>,
    pub style: ChoiceStyle,
}

/// Where a choice sits and which key takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceStyle {
    /// Return.
    Primary,
    /// Esc: back to the editor.
    Back,
    /// Apart on the left, as "Don't save" in macOS.
    Apart,
    /// A small link under the buttons.
    Link,
}

/// The dialog for saving `c`, a change to a meeting the user organizes, on
/// a calendar with `caps` whose server `mailer` mails the guests. On a
/// calendar whose invitations Jolta mails itself, `from` is the address it
/// sends them from, when the calendar has one, and the dialog has a message
/// field: the user's words go in the mail and its COMMENT. None when nobody
/// else would hear of it.
pub fn dialog(c: &Classified, caps: &Capabilities, mailer: &str, from: Option<&str>, creating: bool) -> Option<UpdateDialog> {
    if c.is_own_only() {
        return None;
    }
    let people_only = c.meeting.is_empty();
    let told = if people_only && caps.new_people_only { 0 } else { c.kept.len() } + c.added.len() + c.removed.len();
    if told == 0 {
        return None;
    }
    let back = SendChoice { label: "Keep editing".into(), notify: None, style: ChoiceStyle::Back };
    let hold = caps.hold_update == Hold::Always;
    let message = !caps.sends_itself;
    let note = (!message).then(|| "To add a note, use Email guests after saving.".to_string());
    let sent_from = |what: &str| from.filter(|_| message).map_or_else(String::new, |a| format!(" Jolta sends {what} from {a}."));
    let mut choices = Vec::new();
    if hold {
        choices.push(SendChoice { label: "Don't send".into(), notify: Some(Notify::Nobody), style: ChoiceStyle::Apart });
    }
    choices.push(back);
    if creating {
        choices.push(SendChoice { label: "Send".into(), notify: Some(Notify::All), style: ChoiceStyle::Primary });
        if caps.outside_only {
            choices.push(SendChoice { label: "Only guests who don't use Google Calendar".into(), notify: Some(Notify::Outside), style: ChoiceStyle::Link });
        }
        let body = gets(&c.added, "an invitation").trim_end().to_string();
        let (title, body) = match hold {
            true => (format!("Send invitations to {}?", count(told, "person", "people")), format!("{body}{}", sent_from("them"))),
            false => (format!("{mailer} will email {}", count(told, "person", "people")), format!("{body} {mailer} sends the invitations as soon as Jolta saves the meeting.")),
        };
        return Some(UpdateDialog { title, body, note, message, choices });
    }
    if people_only && caps.new_people_only {
        let others = match c.kept.len() {
            0 => String::new(),
            1 => " The other guest gets nothing.".into(),
            n => format!(" The {n} others get nothing."),
        };
        let title = match (c.added.len(), c.removed.len()) {
            (n, 0) => format!("Invite {}?", count(n, "new person", "new people")),
            (0, n) => format!("Send {}?", count(n, "cancellation", "cancellations")),
            (a, r) => format!("Send {} and {}?", count(a, "invitation", "invitations"), count(r, "cancellation", "cancellations")),
        };
        let body = format!("{}{others}{}", [gets(&c.added, "an invitation"), gets(&c.removed, "a cancellation")].concat().trim_end(), sent_from("them"));
        choices.push(SendChoice { label: "Send".into(), notify: Some(Notify::Changed), style: ChoiceStyle::Primary });
        if caps.outside_only {
            choices.push(SendChoice { label: "Only guests who don't use Google Calendar".into(), notify: Some(Notify::Outside), style: ChoiceStyle::Link });
        }
        return Some(UpdateDialog { title, body, note, message, choices });
    }
    choices.push(SendChoice { label: "Send".into(), notify: Some(Notify::All), style: ChoiceStyle::Primary });
    if caps.outside_only {
        choices.push(SendChoice { label: "Only guests who don't use Google Calendar".into(), notify: Some(Notify::Outside), style: ChoiceStyle::Link });
    }
    let (title, body) = if hold {
        let what = match c.meeting.as_slice() {
            [Field::Time] | [Field::Time, Field::Repeat] => "the new time",
            [Field::Repeat] => "the new repeat",
            [Field::Location] => "the new place",
            [Field::Title] => "the new title",
            [Field::Description] => "the new description",
            _ => "the update",
        };
        let kept = match c.kept.as_slice() {
            [] => String::new(),
            [one] => format!("{} gets {what}. ", name(one)),
            [a, b] => format!("{} and {} get {what}. ", name(a), name(b)),
            many => format!("{} get {what}. ", many.len()),
        };
        let body = format!("{kept}{}{}", new_or_gone(&c.added, "new", "an invitation"), new_or_gone(&c.removed, "removed", "a cancellation"));
        (format!("Send an update to {}?", count(told, "person", "people")), format!("{}{}", body.trim_end(), sent_from("it")))
    } else {
        let body = format!("{mailer} sends every change to a meeting's time, title, place or description to its guests. Jolta can't save this change without emailing them.");
        (format!("{mailer} will email {}", count(told, "person", "people")), body)
    };
    Some(UpdateDialog { title, body, note, message, choices })
}

/// The dialog for deleting a meeting the user organizes, for `people`
/// guests (C3 spec, section 5): Return and Esc keep it, ⌘⌫ cancels it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelDialog {
    /// "Cancel the meeting for 6 people?"
    pub title: String,
    /// "Everyone gets a cancellation from Microsoft 365."
    pub body: String,
    /// A message field, where the cancellation carries the user's words.
    pub message: bool,
    /// Where the message field would be: "To add a note, use Email guests
    /// after cancelling."
    pub note: Option<String>,
    /// The guests it tells.
    pub people: usize,
}

/// What the cancel dialog says for one of the scopes of a delete.
pub fn cancel_dialog(people: usize, caps: &Capabilities, mailer: &str, scope: super::ops::Scope) -> CancelDialog {
    use super::ops::Scope;
    let n = count(people, "person", "people");
    let (title, body) = match scope {
        Scope::All => (format!("Cancel the meeting for {n}?"), format!("Everyone gets a cancellation from {mailer}.")),
        Scope::This => (format!("Cancel this occurrence for {n}?"), format!("Everyone gets a cancellation of this occurrence from {mailer}.")),
        Scope::Following => (format!("Cancel this and the following occurrences for {n}?"), format!("Everyone gets an update from {mailer} that ends the series before this occurrence.")),
    };
    let message = caps.cancel_message && scope != Scope::Following;
    let note = (!message).then(|| "To add a note, use Email guests after cancelling.".to_string());
    CancelDialog { title, body, message, note, people }
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// A person by name, else address.
pub fn name(p: &Person) -> &str {
    if p.name.trim().is_empty() { &p.address } else { p.name.trim() }
}

/// "Jan Peeters gets an invitation. ", or two names, or a count.
fn gets(people: &[Person], what: &str) -> String {
    match people {
        [] => String::new(),
        [one] => format!("{} gets {what}. ", name(one)),
        [a, b] => format!("{} and {} get {what}. ", name(a), name(b)),
        many => format!("{} people get {what}. ", many.len()),
    }
}

/// "Jan Peeters is new and gets an invitation. "
fn new_or_gone(people: &[Person], state: &str, what: &str) -> String {
    match people {
        [] => String::new(),
        [one] => format!("{} is {state} and gets {what}. ", name(one)),
        [a, b] => format!("{} and {} are {state} and get {what}. ", name(a), name(b)),
        many => format!("{} people are {state} and get {what}. ", many.len()),
    }
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

        let mut optional = before.clone();
        optional.attendees[1].role = Role::Optional;
        let c = classify(&before, &optional, &[]);
        assert_eq!(c.meeting, [Field::People], "a guest made optional hears about it");
        assert!(!c.is_own_only());

        let mut renamed_case = before.clone();
        renamed_case.attendees[1].person.address = "Ann@Example.com".into();
        assert!(classify(&before, &renamed_case, &[]).added.is_empty(), "addresses compare without case");
    }
    fn caps_of(source: crate::calendar::CalendarSource) -> Capabilities {
        use crate::calendar::{Access, Calendar, CalendarId};
        crate::calendar::caps::of(&Calendar {
            id: CalendarId(1),
            account_id: None,
            source,
            remote_id: "x".into(),
            name: "x".into(),
            color: None,
            zone: None,
            access: Access::Owner,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: false,
            cursor: None,
            refresh: None,
            synced: None,
        })
    }

    fn six() -> EventData {
        let mut e = meeting();
        e.attendees = std::iter::once(Attendee { is_me: true, ..person("me@example.com") })
            .chain(["ann", "bo", "cy", "dee", "els"].iter().map(|n| person(&format!("{n}@example.com"))))
            .collect();
        e.attendees[5].person.name = "Els Maes".into();
        e
    }

    fn buttons(d: &UpdateDialog) -> Vec<(&str, Option<Notify>, ChoiceStyle)> {
        d.choices.iter().map(|c| (c.label.as_str(), c.notify, c.style)).collect()
    }

    /// Each row of the Send an update? table (C3 spec, section 5).
    #[test]
    fn the_dialog_follows_the_table_per_provider() {
        use crate::calendar::CalendarSource::{CalDav, Google, Graph, Local};
        let before = six();
        let mut after = before.clone();
        after.span = Span::Timed { start: at("2026-11-24 10:00"), end: at("2026-11-24 10:30") };
        after.attendees.retain(|a| !a.person.address.starts_with("els"));
        after.attendees.push(Attendee { person: Person { address: "jan@example.com".into(), name: "Jan Peeters".into() }, ..person("jan@example.com") });
        let c = classify(&before, &after, &[]);

        let g = dialog(&c, &caps_of(Google), "Google", None, false).unwrap();
        assert_eq!(g.title, "Send an update to 6 people?");
        assert_eq!(g.body, "4 get the new time. Jan Peeters is new and gets an invitation. Els Maes is removed and gets a cancellation.");
        assert_eq!(g.note.as_deref(), Some("To add a note, use Email guests after saving."), "no message field on an update");
        assert_eq!(
            buttons(&g),
            [
                ("Don't send", Some(Notify::Nobody), ChoiceStyle::Apart),
                ("Keep editing", None, ChoiceStyle::Back),
                ("Send", Some(Notify::All), ChoiceStyle::Primary),
                ("Only guests who don't use Google Calendar", Some(Notify::Outside), ChoiceStyle::Link),
            ]
        );

        for (source, name) in [(Graph, "Microsoft 365"), (CalDav, "iCloud")] {
            let m = dialog(&c, &caps_of(source), name, None, false).unwrap();
            assert_eq!(m.title, format!("{name} will email 6 people"));
            assert!(m.body.starts_with(&format!("{name} sends every change")), "{}", m.body);
            assert_eq!(buttons(&m), [("Keep editing", None, ChoiceStyle::Back), ("Send", Some(Notify::All), ChoiceStyle::Primary)], "no Don't send on {name}");
        }
        let l = dialog(&c, &caps_of(Local), "Jolta", Some("me@example.com"), false).unwrap();
        assert_eq!(l.title, "Send an update to 6 people?");
        assert!(l.body.ends_with(" Jolta sends it from me@example.com."), "{}", l.body);
        assert!(l.message && l.note.is_none(), "Jolta's own mail carries a message");
        assert_eq!(buttons(&l), [("Don't send", Some(Notify::Nobody), ChoiceStyle::Apart), ("Keep editing", None, ChoiceStyle::Back), ("Send", Some(Notify::All), ChoiceStyle::Primary)]);
        assert!(!g.message, "Google's update carries no message");

        let mut added = before.clone();
        added.attendees.push(Attendee { person: Person { address: "jan@example.com".into(), name: "Jan Peeters".into() }, ..person("jan@example.com") });
        let people = classify(&before, &added, &[]);
        let g = dialog(&people, &caps_of(Google), "Google", None, false).unwrap();
        assert_eq!((g.title.as_str(), g.body.as_str()), ("Invite 1 new person?", "Jan Peeters gets an invitation. The 5 others get nothing."));
        assert!(buttons(&g).contains(&("Send", Some(Notify::Changed), ChoiceStyle::Primary)) && buttons(&g).contains(&("Don't send", Some(Notify::Nobody), ChoiceStyle::Apart)));
        let m = dialog(&people, &caps_of(Graph), "Microsoft 365", None, false).unwrap();
        assert_eq!(buttons(&m), [("Keep editing", None, ChoiceStyle::Back), ("Send", Some(Notify::Changed), ChoiceStyle::Primary)]);
        let l = dialog(&people, &caps_of(Local), "Jolta", Some("me@example.com"), false).unwrap();
        assert_eq!((l.title.as_str(), l.body.as_str()), ("Invite 1 new person?", "Jan Peeters gets an invitation. The 5 others get nothing. Jolta sends them from me@example.com."));
        assert!(buttons(&l).contains(&("Send", Some(Notify::Changed), ChoiceStyle::Primary)) && buttons(&l).contains(&("Don't send", Some(Notify::Nobody), ChoiceStyle::Apart)));
        let i = dialog(&people, &caps_of(CalDav), "Your calendar server", None, false).unwrap();
        assert_eq!(i.title, "Your calendar server will email 6 people", "a CalDAV server not measured may tell everyone");

        let mut gone = before.clone();
        gone.attendees.retain(|a| !a.person.address.starts_with("els"));
        let g = dialog(&classify(&before, &gone, &[]), &caps_of(Google), "Google", None, false).unwrap();
        assert_eq!((g.title.as_str(), g.body.as_str()), ("Send 1 cancellation?", "Els Maes gets a cancellation. The 4 others get nothing."));

        let mut alarm = before.clone();
        alarm.alarms = vec![crate::calendar::Alarm { trigger: crate::calendar::AlarmTrigger::MinutesBefore(5), email: false }];
        assert_eq!(dialog(&classify(&before, &alarm, &[]), &caps_of(Graph), "Microsoft 365", None, false), None, "the user's own fields mail nobody");

        let lonely = EventData { attendees: vec![Attendee { is_me: true, ..person("me@example.com") }], ..meeting() };
        let moved = EventData { title: "Moved".into(), ..lonely.clone() };
        assert_eq!(dialog(&classify(&lonely, &moved, &[]), &caps_of(Google), "Google", None, false), None, "nobody else on it");

        let blank = EventData::new("u1", before.span);
        let new = dialog(&classify(&blank, &before, &[]), &caps_of(Google), "Google", None, true).unwrap();
        assert_eq!((new.title.as_str(), new.body.as_str()), ("Send invitations to 5 people?", "5 people get an invitation."));
        let new = dialog(&classify(&blank, &before, &[]), &caps_of(Graph), "Microsoft 365", None, true).unwrap();
        assert_eq!(new.title, "Microsoft 365 will email 5 people");
        let new = dialog(&classify(&blank, &before, &[]), &caps_of(Local), "Jolta", None, true).unwrap();
        assert_eq!((new.title.as_str(), new.body.as_str()), ("Send invitations to 5 people?", "5 people get an invitation."), "no address before the user picks one");
    }

    #[test]
    fn the_cancel_dialog_carries_a_message_where_the_provider_does() {
        use crate::calendar::CalendarSource::{Google, Graph};
        use crate::calendar::ops::Scope;
        let m = cancel_dialog(6, &caps_of(Graph), "Microsoft 365", Scope::All);
        assert_eq!((m.title.as_str(), m.body.as_str(), m.message, m.note), ("Cancel the meeting for 6 people?", "Everyone gets a cancellation from Microsoft 365.", true, None));
        let g = cancel_dialog(1, &caps_of(Google), "Google", Scope::All);
        assert_eq!((g.title.as_str(), g.message), ("Cancel the meeting for 1 person?", false), "Google's delete takes no message (decision 4)");
        assert_eq!(g.note.as_deref(), Some("To add a note, use Email guests after cancelling."));
        assert!(!cancel_dialog(6, &caps_of(Graph), "Microsoft 365", Scope::Following).message, "ending a series is an update, with no comment");
    }

    /// Check 2: a change to the whole series reaches an occurrence that kept
    /// the series' people, and each guest keeps the answer given to that
    /// occurrence alone.
    #[test]
    fn a_series_wide_change_keeps_answers_given_to_one_occurrence() {
        let series = meeting();
        let mut occurrence = series.clone();
        occurrence.recurrence_id = Some(crate::calendar::Moment::Time(at("2026-12-01 09:00")));
        occurrence.attendees[1].response = Response::Declined;
        occurrence.attendees[1].comment = "Away that week".into();
        let mut renamed = series.clone();
        renamed.title = "Daily".into();
        renamed.attendees.push(person("cy@example.com"));
        let out = carried(&series, &renamed, &occurrence);
        assert_eq!(out.title, "Daily");
        let ann = out.attendees.iter().find(|a| a.person.address == "ann@example.com").unwrap();
        assert_eq!((ann.response, ann.comment.as_str()), (Response::Declined, "Away that week"), "the occurrence's own answer stays");
        assert!(out.attendees.iter().any(|a| a.person.address == "cy@example.com"), "the new guest reaches it");
    }

}
