//! What Google's reply puts under its sentence, in the layouts of real
//! replies from 2013 to 2026, in English and Dutch: the event's own title,
//! place and description, its time and the guest list under Google's
//! labels, then Google's footer. A line that fits none of them is someone's
//! words, and so is a footer Jolta hasn't seen.

use crate::calendar::EventData;

/// Whether every line under the sentence is Google's for `event`.
pub(super) fn details(text: &str, event: &EventData) -> bool {
    // format=flowed with delsp=yes: a line ending in a space goes on.
    let text = text.replace("\r\n", "\n").replace(" \n", " ");
    let lines: Vec<String> = text.lines().map(flat).collect();
    let Some(sentence) = lines.iter().position(|l| !l.is_empty()) else { return false };
    let lines = &lines[sentence + 1..];
    let Some(footer) = lines.iter().rposition(|l| FOOTER_STARTS.iter().any(|s| l.starts_with(s))) else { return false };
    let (lines, footer) = lines.split_at(footer);
    footer.split(String::is_empty).filter(|p| !p.is_empty()).all(|p| footer_paragraph(&p.join(" ")))
        && Layout::of(event).fits(lines)
}

/// The event's own words as Google shows them.
struct Layout {
    title: String,
    location: String,
    /// Title, place and description in one line, between spaces.
    own: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum At {
    /// Right under the title in the layout since 2022: the time and the
    /// zone, written in the organizer's language, up to a blank line.
    Time,
    /// A guest list, up to a blank line.
    People,
    Rest,
}

impl Layout {
    fn of(event: &EventData) -> Layout {
        let description = if event.description_html { super::html_text(&event.description) } else { event.description.clone() };
        let own = format!(" {} ", flat(&unlinked(&format!("{} {} {description}", event.title, event.location))));
        Layout { title: flat(&event.title), location: flat(&event.location), own }
    }

    fn fits(&self, lines: &[String]) -> bool {
        let lines = &lines[lines.iter().position(|l| !l.is_empty()).unwrap_or(lines.len())..];
        let mut at = match lines.first() {
            Some(first) if !self.title.is_empty() && *first == self.title => At::Time,
            _ => At::Rest,
        };
        for line in lines.iter().map(String::as_str) {
            if line.is_empty() {
                at = At::Rest;
                continue;
            }
            if matches!(at, At::Time | At::People) {
                continue;
            }
            if PEOPLE.contains(&line) {
                at = At::People;
                continue;
            }
            if !self.fits_line(line) {
                return false;
            }
        }
        true
    }

    fn fits_line(&self, line: &str) -> bool {
        if self.own.contains(&format!(" {line} ")) || LABELS.contains(&line) || LINKS.iter().any(|l| line.starts_with(l)) {
            return true;
        }
        let Some((label, value)) = line.split_once(':').map(|(l, v)| (l, v.trim())) else { return false };
        match label {
            "Title" | "Titel" => value == self.title,
            "Where" | "Waar" => value.is_empty() || value.starts_with(&self.location) && !self.location.is_empty(),
            "When" | "Wanneer" | "Calendar" | "Agenda" => true,
            _ => false,
        }
    }
}

/// The labels above a guest list.
const PEOPLE: &[&str] = &["Who:", "Wie:", "Organizer", "Organisator", "Guests", "Gasten"];

const LABELS: &[&str] = &["Location", "Locatie", "~~//~~"];

/// Lines Google starts with a link of its own.
const LINKS: &[&str] = &["https://www.google.com/maps/search/", "View all guest info https://calendar.google.com/", "Alle gastgegevens bekijken https://calendar.google.com/"];

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
