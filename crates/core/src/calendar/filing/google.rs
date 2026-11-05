//! What Google's reply puts under its sentence, in the layouts of real
//! replies from 2012 to 2026, in English and Dutch: the event's own title,
//! place and description, its time and the guest list under Google's
//! labels, then Google's footer. A line that fits none of them is someone's
//! words, and so is a footer Jolta hasn't seen, a time other than the
//! event's, a zone whose name Jolta hasn't seen, and a guest the meeting
//! doesn't name.

use chrono::{DateTime, Datelike, NaiveDate, Timelike};
use chrono_tz::Tz;

use crate::calendar::{EventData, Span, Zone};

/// Whether every line under the sentence is Google's for `event`, the
/// reply's own, whose guests may also be among `copy`, the account's copy
/// of the meeting.
pub(super) fn details(text: &str, event: &EventData, copy: &[EventData]) -> bool {
    // format=flowed with delsp=yes: a line ending in a space goes on.
    let text = text.replace("\r\n", "\n").replace(" \n", " ");
    let lines: Vec<String> = text.lines().map(flat).collect();
    let Some(sentence) = lines.iter().position(|l| !l.is_empty()) else { return false };
    let lines = &lines[sentence + 1..];
    let Some(footer) = lines.iter().rposition(|l| FOOTER_STARTS.iter().any(|s| l.starts_with(s))) else { return false };
    let (lines, footer) = lines.split_at(footer);
    footer.split(String::is_empty).filter(|p| !p.is_empty()).all(|p| footer_paragraph(&p.join(" "))) && Layout::of(event, copy).fits(lines)
}

/// The event's own words as Google shows them.
struct Layout {
    title: String,
    location: String,
    /// Title, place and description in one line, between spaces.
    own: String,
    span: Span,
    /// Names and addresses of the organizer and guests, lowercase.
    people: Vec<String>,
}

impl Layout {
    fn of(event: &EventData, copy: &[EventData]) -> Layout {
        let description = if event.description_html { super::html_text(&event.description) } else { event.description.clone() };
        let own = format!(" {} ", flat(&unlinked(&format!("{} {} {description}", event.title, event.location))));
        let people = std::iter::once(event)
            .chain(copy)
            .flat_map(|e| e.organizer.iter().chain(e.attendees.iter().map(|a| &a.person)))
            .flat_map(|p| [flat(&p.name), flat(&p.address)])
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect();
        Layout { title: flat(&event.title), location: flat(&event.location), own, span: event.span, people }
    }

    fn fits(&self, lines: &[String]) -> bool {
        let lines = &lines[lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len())..];
        // In the layout since 2022 the time and its zone follow the title,
        // up to a blank line.
        let lines = match lines.split_first() {
            Some((first, rest)) if !self.title.is_empty() && *first == self.title => {
                let end = rest.iter().position(String::is_empty).unwrap_or(rest.len());
                if !self.time(&rest[..end]) {
                    return false;
                }
                &rest[end..]
            }
            _ => lines,
        };
        let mut guests = false;
        for line in lines.iter().map(String::as_str) {
            if line.is_empty() {
                guests = false;
            } else if PEOPLE.contains(&line) {
                guests = true;
            } else if !(if guests { self.guest(line) || google_s(line) } else { self.fits_line(line) }) {
                return false;
            }
        }
        true
    }

    fn fits_line(&self, line: &str) -> bool {
        if self.own.contains(&format!(" {line} ")) || google_s(line) {
            return true;
        }
        let Some((label, value)) = line.split_once(':').map(|(l, v)| (l, v.trim())) else { return false };
        match label {
            "Title" | "Titel" => value == self.title,
            "Where" | "Waar" => value.is_empty() || value.starts_with(&self.location) && !self.location.is_empty(),
            "When" | "Wanneer" => self.when_2013(value),
            "Calendar" | "Agenda" => self.person(value),
            _ => false,
        }
    }

    /// The time block of the layout since 2022: the event's day and hours
    /// in a zone, then that zone's name.
    fn time(&self, lines: &[String]) -> bool {
        let [when, zone] = lines else { return false };
        ZONES.iter().any(|z| z.names.contains(&zone.as_str()) && self.span_in(z.tz).is_some_and(|span| when_2022(span).contains(when)))
    }

    /// The value after When in the layout of 2013: the event's days, with
    /// its hours and the zone's city when it isn't all-day.
    fn when_2013(&self, value: &str) -> bool {
        ZONES.iter().any(|z| match self.span_in(z.tz) {
            Some(Placed::Days(first, last)) => days_2013(first, last).iter().any(|w| w == value),
            Some(span) => z.cities.iter().any(|city| days_2013_timed(span).iter().any(|w| format!("{w} {city}") == value)),
            None => false,
        })
    }

    fn span_in(&self, tz: Tz) -> Option<Placed> {
        match self.span {
            Span::AllDay { start, end } => Some(Placed::Days(start, end.pred_opt()?)),
            Span::Timed { start, end } if start.zone != Zone::Floating && end.zone != Zone::Floating => {
                Some(Placed::Times(start.instant(Tz::UTC).with_timezone(&tz), end.instant(Tz::UTC).with_timezone(&tz)))
            }
            Span::Timed { .. } => None,
        }
    }

    /// A line of a guest list: someone the meeting names, by name or
    /// address, with the role Google adds.
    fn guest(&self, line: &str) -> bool {
        let line = line.strip_prefix("* ").unwrap_or(line);
        self.person(ROLES.iter().find_map(|r| line.strip_suffix(r)).unwrap_or(line).trim())
    }

    fn person(&self, who: &str) -> bool {
        let who = who.to_lowercase();
        self.people.contains(&who)
    }
}

/// An event's time in one zone; an all-day event's first and last day.
#[derive(Clone, Copy)]
enum Placed {
    Days(NaiveDate, NaiveDate),
    Times(DateTime<Tz>, DateTime<Tz>),
}

/// The zones whose names real replies show, as the line under the time
/// since 2022 and as the city after the time in 2013.
struct KnownZone {
    tz: Tz,
    names: &'static [&'static str],
    cities: &'static [&'static str],
}

const ZONES: &[KnownZone] = &[KnownZone {
    tz: chrono_tz::Europe::Brussels,
    names: &["Central European Time - Brussels", "Midden-Europese tijd - Brussel"],
    cities: &["Brussels", "Brussel"],
}];

struct Language {
    weekdays: [&'static str; 7],
    months: [&'static str; 12],
}

const ENGLISH: Language = Language {
    weekdays: ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"],
    months: ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"],
};

const DUTCH: Language = Language {
    weekdays: ["maandag", "dinsdag", "woensdag", "donderdag", "vrijdag", "zaterdag", "zondag"],
    months: ["jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov", "dec"],
};

/// Dutch weekdays as the layout of 2013 writes them.
const DUTCH_SHORT: [&str; 7] = ["ma", "di", "wo", "do", "vr", "za", "zo"];

/// "woensdag 29 jul 2026 ⋅ 09:00 – 14:00", "maandag 5 jan 2026 ⋅ 7pm – 8pm":
/// a meeting within one day, in English or Dutch, on a 24- or 12-hour
/// clock. Google's other forms, such as a series' rule, match nothing.
fn when_2022(span: Placed) -> Vec<String> {
    let Placed::Times(start, end) = span else { return Vec::new() };
    if start.date_naive() != end.date_naive() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for language in [&ENGLISH, &DUTCH] {
        let day = format!(
            "{} {} {} {}",
            language.weekdays[start.weekday().num_days_from_monday() as usize],
            start.day(),
            language.months[start.month0() as usize],
            start.year()
        );
        for clock in [clock_24, clock_12] {
            out.push(format!("{day} \u{22c5} {} \u{2013} {}", clock(start), clock(end)));
        }
    }
    out
}

/// "vr 29. nov" or "vr 29. nov 2013": a day in the layout of 2013, whose
/// year Google leaves out at times.
fn day_2013(date: NaiveDate) -> [String; 2] {
    let day = format!("{} {}. {}", DUTCH_SHORT[date.weekday().num_days_from_monday() as usize], date.day(), DUTCH.months[date.month0() as usize]);
    [format!("{day} {}", date.year()), day]
}

/// "zo 10. nov – za 16. nov 2013" and its other years.
fn days_2013(first: NaiveDate, last: NaiveDate) -> Vec<String> {
    if first == last {
        return Vec::new();
    }
    day_2013(first).iter().flat_map(|a| day_2013(last).map(|b| format!("{a} \u{2013} {b}"))).collect()
}

/// "za 14. dec 2013 16:30 – 23:55" and "vr 29. nov 00:00 – za 30. nov 2013
/// 01:00", without the city.
fn days_2013_timed(span: Placed) -> Vec<String> {
    let Placed::Times(start, end) = span else { return Vec::new() };
    let (from, to) = (clock_24(start), clock_24(end));
    if start.date_naive() == end.date_naive() {
        return day_2013(start.date_naive()).iter().map(|d| format!("{d} {from} \u{2013} {to}")).collect();
    }
    day_2013(start.date_naive()).iter().flat_map(|a| day_2013(end.date_naive()).map(|b| format!("{a} {from} \u{2013} {b} {to}"))).collect()
}

fn clock_24(t: DateTime<Tz>) -> String {
    format!("{:02}:{:02}", t.hour(), t.minute())
}

/// "7pm", "4:30am", "12pm".
fn clock_12(t: DateTime<Tz>) -> String {
    let (pm, hour) = t.hour12();
    let minutes = if t.minute() == 0 { String::new() } else { format!(":{:02}", t.minute()) };
    format!("{hour}{minutes}{}", if pm { "pm" } else { "am" })
}

/// The labels above a guest list.
const PEOPLE: &[&str] = &["Who:", "Wie:", "Organizer", "Organisator", "Guests", "Gasten"];

/// The roles Google writes after a guest.
const ROLES: &[&str] = &["- organizer", "- creator", "- organisator", "- maker"];

/// A label or a link of Google's own on a line of its own.
fn google_s(line: &str) -> bool {
    LABELS.contains(&line) || LINKS.iter().any(|l| line.strip_prefix(l).is_some_and(|link| !link.contains(' ')))
}

const LABELS: &[&str] = &["Location", "Locatie", "~~//~~", "View all guest info", "Alle gastgegevens bekijken"];

/// Google's links, each one word to the end of its line.
const LINKS: &[&str] = &[
    "https://www.google.com/maps/search/",
    "https://calendar.google.com/calendar/event?",
    "View all guest info https://calendar.google.com/calendar/event?",
    "Alle gastgegevens bekijken https://calendar.google.com/calendar/event?",
];

const FOOTER_STARTS: &[&str] = &["Invitation from Google Calendar: ", "Uitnodiging van Google Agenda: "];

/// Google's footer paragraphs; `*` stands for an address.
const FOOTERS: &[&str] = &[
    "Invitation from Google Calendar: https://calendar.google.com/calendar/",
    "Invitation from Google Calendar: https://www.google.com/calendar/",
    "Uitnodiging van Google Agenda: https://calendar.google.com/calendar/",
    "Uitnodiging van Google Agenda: https://www.google.com/calendar/",
    "You are receiving this email because you are an attendee on the event.",
    "You are receiving this email because you are an attendee on the event. To stop receiving future updates for this event, decline this event.",
    "Forwarding this invitation could allow any recipient to send a response to the organizer, be added to the guest list, invite others regardless of their own invitation status, or modify your RSVP.",
    "Learn more https://support.google.com/calendar/answer/37135#forwarding",
    "Je krijgt deze e-mail omdat je deelnemer aan de afspraak bent. Als je geen meldingen meer wilt krijgen voor deze afspraak, kun je deze afspraak weigeren.",
    "Als je deze uitnodiging doorstuurt, kunnen alle ontvangers een reactie naar de organisator sturen en aan de gastenlijst worden toegevoegd. Ze kunnen ook anderen uitnodigen (ongeacht hun eigen uitnodigingsstatus) of je reactiebericht aanpassen.",
    "Meer informatie https://support.google.com/calendar/answer/37135#forwarding",
    "Je ontvangt dit e-mailbericht in de account * omdat je bent aangemeld voor antwoorden op uitnodigingen in de agenda *",
    "Als je deze meldingen niet langer wilt ontvangen, meld je je aan bij https://www.google.com/calendar/ en wijzig je je instellingen voor meldingen voor deze agenda.",
    "Je ontvangt deze e-mail op het adres * omdat je je hebt aangemeld voor updates over antwoorden op uitnodigingen in de agenda van *",
    "Als je deze meldingen niet langer wilt ontvangen, log je in bij https://www.google.com/calendar/ en wijzig je je instellingen voor meldingen van deze agenda.",
];

fn footer_paragraph(paragraph: &str) -> bool {
    FOOTERS.iter().any(|f| {
        let (words, known) = (paragraph.split(' ').collect::<Vec<_>>(), f.split(' ').collect::<Vec<_>>());
        words.len() == known.len() && words.iter().zip(&known).all(|(w, k)| w == k || (*k == "*" && w.contains('@')))
    })
}

/// The text without the `<https://…>` links and `<address>`es that
/// Google leaves out of a description it shows.
fn unlinked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else { break };
        let inner = &rest[start + 1..start + end];
        out.push_str(&rest[..start]);
        if !(inner.contains("://") || inner.contains('@')) || inner.contains(char::is_whitespace) {
            out.push_str(&rest[start..=start + end]);
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// The text with each run of whitespace as one space, trimmed.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
