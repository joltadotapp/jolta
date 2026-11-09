//! An event's description as the editor changes it: the user's own text,
//! edited as plain text, and a meeting block that Teams, Google Meet or Zoom
//! put there, kept byte for byte after it.

/// A description split for the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parts {
    /// The user's own text, plain.
    pub editable: String,
    /// The meeting block from its first line to the end, as the description
    /// holds it.
    pub kept: Option<String>,
    /// HTML up to the user's text: the head and the tags opening the body.
    lead: String,
    /// What stood between the user's text and the block: a blank line, or
    /// the tags closing the text's paragraph.
    gap: String,
    html: bool,
}

/// Splits `text` (HTML when `html`) into the user's text and the meeting
/// block after it. The C3 spec described a Teams block as a line of
/// underscores before the link; Exchange's own body starts the block at its
/// "Microsoft Teams meeting" line, with the underscores after the link
/// (captured from the Microsoft 365 test account, 2026-11-07), so both start one.
pub fn split(text: &str, html: bool) -> Parts {
    let start = if html { body_text_start(text) } else { 0 };
    let Some(block) = block_start(text, html) else {
        return Parts { editable: plain(&text[start..], html), kept: None, lead: text[..start].into(), gap: String::new(), html };
    };
    let start = start.min(block);
    let region = &text[start..block];
    let end = if html { last_text_end(region) } else { region.trim_end().len() };
    Parts {
        editable: plain(&region[..end], html),
        kept: Some(text[block..].into()),
        lead: text[..start].into(),
        gap: region[end..].into(),
        html,
    }
}

impl Parts {
    /// The description with the user's text changed to `edited`, and whether
    /// it is HTML. The meeting block and the HTML around the user's text stay
    /// as they were; an HTML description without a block becomes plain text.
    pub fn join(&self, edited: &str) -> (String, bool) {
        let edited = edited.trim_end();
        let Some(kept) = &self.kept else { return (edited.to_string(), false) };
        if edited.is_empty() {
            return (format!("{}{kept}", self.lead), self.html);
        }
        let (text, gap) = match self.html {
            true => (edited.lines().map(escape).collect::<Vec<_>>().join("<br>\n"), if self.gap.is_empty() { "<br>\n" } else { &self.gap }),
            false => (edited.to_string(), if self.gap.is_empty() { "\n\n" } else { &self.gap }),
        };
        (format!("{}{text}{gap}{kept}", self.lead), self.html)
    }

    /// Whether the meeting block is `text`, line for line, however the
    /// server wrapped it in HTML.
    pub fn kept_is(&self, text: &str) -> bool {
        let lines = |t: &str| t.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect::<Vec<_>>();
        self.kept.as_deref().is_some_and(|kept| lines(&plain(kept, self.html)) == lines(text))
    }

    /// Changing the text drops the description's HTML formatting: an HTML
    /// description with no meeting block, which Outlook's editor made.
    pub fn loses_formatting(&self) -> bool {
        self.html && self.kept.is_none() && !self.editable.is_empty()
    }
}

/// Where the meeting block starts: the earliest line that opens a Teams,
/// Google Meet or Zoom block, with, in HTML, the tags opening that line.
fn block_start(text: &str, html: bool) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let teams = |rest: &str| rest.contains("teams.microsoft.com") || rest.contains("teams.live.com");
    let zoom = |rest: &str| rest.contains("zoom.us/") || rest.contains("zoomgov.com/");
    let mut found: Vec<usize> = Vec::new();
    for (needle, follows) in [
        ("microsoft teams meeting", &teams as &dyn Fn(&str) -> bool),
        ("microsoft teams-vergadering", &teams),
        ("réunion microsoft teams", &teams),
        ("________________", &teams),
        ("-::~:~::~", &|_: &str| true),
        ("is inviting you to a scheduled zoom meeting", &zoom),
        ("join zoom meeting", &zoom),
        ("teams.microsoft.com/l/meetup-join", &|_: &str| true),
    ] {
        if let Some(at) = lower.match_indices(needle).map(|(i, _)| i).find(|&i| follows(&lower[i..])) {
            found.push(line_start(text, at, html));
        }
    }
    found.into_iter().min()
}

/// The start of the line holding `at`; in HTML, before the tags that open it.
fn line_start(text: &str, at: usize, html: bool) -> usize {
    let breaks: &[char] = if html { &['\n', '>'] } else { &['\n'] };
    let mut start = text[..at].rfind(breaks).map_or(0, |i| i + 1);
    if !html {
        return start;
    }
    loop {
        let before = text[..start].trim_end();
        let Some(lt) = before.ends_with('>').then(|| before.rfind('<')).flatten() else { return start };
        let tag = before[lt + 1..].to_ascii_lowercase();
        if tag.starts_with('/') || tag.starts_with('!') || tag.starts_with("br") || tag.starts_with("body") || tag.starts_with("hr") {
            return start;
        }
        start = lt;
    }
}

/// Where the body's first text is: past the head, the body tag and the
/// tags and spaces that open it.
fn body_text_start(html: &str) -> usize {
    let lower = html.to_ascii_lowercase();
    let mut at = lower.find("<body").and_then(|b| lower[b..].find('>').map(|e| b + e + 1)).unwrap_or(0);
    loop {
        let rest = &html[at..];
        let skipped = rest.len() - rest.trim_start().len();
        at += skipped;
        if html[at..].starts_with('<') {
            match html[at..].find('>') {
                Some(e) => at += e + 1,
                None => return at,
            }
        } else if skipped == 0 {
            return at;
        }
    }
}

/// The end of the last text in an HTML piece, before the tags and spaces
/// that close it.
fn last_text_end(region: &str) -> usize {
    let mut end = region.len();
    loop {
        let trimmed = region[..end].trim_end();
        if trimmed.ends_with('>')
            && let Some(lt) = trimmed.rfind('<')
        {
            end = lt;
            continue;
        }
        return trimmed.len();
    }
}

fn escape(line: &str) -> String {
    line.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn plain(piece: &str, html: bool) -> String {
    if !html {
        return piece.trim().to_string();
    }
    let text = mail_parser::html_to_text(piece);
    text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::ical::tests::{fixture, read_events};

    /// Exchange's body for a Teams meeting made without text: 1,960
    /// characters, captured from the Microsoft 365 test account with the
    /// tenant, organizer and meeting ids replaced.
    const TEAMS: &str = include_str!("../../tests/fixtures/descriptions/teams-graph.html");
    /// The same with "Agenda for the probe." as the text Jolta sent.
    const TEAMS_TEXT: &str = include_str!("../../tests/fixtures/descriptions/teams-graph-text.html");

    fn description(ics: &str) -> (String, bool) {
        let e = read_events(ics).into_iter().next().expect("an event");
        (e.description, e.description_html)
    }

    #[test]
    fn exchange_teams_body_without_text_keeps_the_whole_block() {
        assert_eq!(TEAMS.len(), 1960);
        let parts = split(TEAMS, true);
        assert_eq!(parts.editable, "");
        let kept = parts.kept.clone().expect("a block");
        assert!(kept.contains("Microsoft Teams meeting") && kept.contains("meetup-join") && kept.contains("Meeting options"), "{kept}");
        let (joined, html) = parts.join("Bring the numbers.\nAnd the plan for January.");
        assert!(html);
        assert!(joined.ends_with(&kept), "the block stays byte for byte after the text");
        assert!(joined.contains("Bring the numbers.<br>\nAnd the plan for January.<br>\n"), "{joined}");
        assert!(joined.starts_with(&TEAMS[..TEAMS.find("<body>").unwrap() + 6]), "the head stays");
        assert_eq!(parts.join(""), (TEAMS.to_string(), true), "an empty text writes the body back unchanged");
    }

    #[test]
    fn exchange_teams_body_changes_only_the_text() {
        let parts = split(TEAMS_TEXT, true);
        assert_eq!(parts.editable, "Agenda for the probe.");
        assert_eq!(parts.join("Agenda for the probe."), (TEAMS_TEXT.to_string(), true));
        assert_eq!(parts.join("Numbers & plan <Q4>").0, TEAMS_TEXT.replace("Agenda for the probe.", "Numbers &amp; plan &lt;Q4&gt;"));
    }

    /// Outlook's text DESCRIPTION, as `outlook-series.ics` holds it.
    const OUTLOOK_TEXT: &str = "Wekelijkse planning.\n\n______________________________________________________________________________\nMicrosoft Teams meeting\nJoin on your computer, mobile app or room device\nClick here to join the meeting<https://teams.microsoft.com/l/meetup-join/19%3ameeting_ZmFrZQ%40thread.v2/0?context=%7b%22Tid%22%3a%22example%22%7d>\nMeeting ID: 312 456 789 012\nPasscode: aB3cD4\n";

    #[test]
    fn outlook_teams_text_keeps_the_line_of_underscores_and_what_follows() {
        let parts = split(OUTLOOK_TEXT, false);
        assert_eq!(parts.editable, "Wekelijkse planning.");
        assert!(parts.kept.as_deref().unwrap().starts_with("______"));
        assert_eq!(parts.join("Nieuwe planning.\nMet cijfers.").0, OUTLOOK_TEXT.replace("Wekelijkse planning.", "Nieuwe planning.\nMet cijfers."));
    }

    #[test]
    fn outlook_html_keeps_the_teams_link_it_holds_alone() {
        let (text, html) = description(fixture!("outlook-series.ics"));
        assert!(html);
        let parts = split(&text, true);
        assert_eq!(parts.editable, "Wekelijkse planning.");
        let kept = parts.kept.clone().unwrap();
        assert!(kept.starts_with("<div><a href=\"https://teams.microsoft.com/l/meetup-join"), "{kept}");
        let (joined, _) = parts.join("Nieuwe planning.");
        assert_eq!(joined, text.replace("Wekelijkse planning.", "Nieuwe planning."));
    }

    #[test]
    fn google_meet_footer_stays() {
        let (text, html) = description(fixture!("google.ics"));
        assert!(!html);
        let parts = split(&text, false);
        assert_eq!(parts.editable, "Design review voor de nieuwe inbox.");
        assert!(parts.kept.as_deref().unwrap().starts_with("-::~:~::~"));
        assert_eq!(parts.join("Andere tekst").0, text.replace("Design review voor de nieuwe inbox.", "Andere tekst"));
    }

    #[test]
    fn zoom_invitation_stays_after_new_text() {
        let (text, _) = description(fixture!("zoom.ics"));
        let parts = split(&text, false);
        assert_eq!(parts.editable, "");
        assert_eq!(parts.kept.as_deref(), Some(text.as_str()));
        assert_eq!(parts.join("Agenda first.").0, format!("Agenda first.\n\n{text}"));
    }

    #[test]
    fn a_description_without_a_block_is_all_text() {
        let parts = split("Bring the numbers.\n", false);
        assert_eq!((parts.editable.as_str(), parts.kept.is_none(), parts.loses_formatting()), ("Bring the numbers.", true, false));
        assert_eq!(parts.join("Other"), ("Other".to_string(), false));

        let html = "<html><body><p>Bring the <b>numbers</b>.</p><p>And the plan.</p></body></html>";
        let parts = split(html, true);
        assert_eq!(parts.editable, "Bring the numbers.\nAnd the plan.");
        assert!(parts.loses_formatting(), "Outlook's formatting goes when the text changes");
        assert_eq!(parts.join("Plain now"), ("Plain now".to_string(), false));
    }

    #[test]
    fn a_line_of_underscores_alone_is_the_users() {
        let text = "Notes\n____________________\nMore notes";
        assert_eq!(split(text, false).kept, None);
    }
}
