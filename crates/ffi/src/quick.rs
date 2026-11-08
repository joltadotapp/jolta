//! Quick create (Q, and New event in the palette) and go to date (G).

use chrono::{Duration, Utc};
use jolta_core::ai::{self, Cancel};
use jolta_core::calendar::quick::{self, Lang, Part, Prefs};
use jolta_core::calendar::{Local, Span, Zone, draft, tz};
use jolta_core::people::Match;
use jolta_core::store;

use crate::calendar::PersonRecord;
use crate::editor::EventDraft;
use crate::{Core, CoreError, Result};

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickPartKind {
    When,
    Person,
    Place,
    Repeat,
}

/// A stretch of the typed text that became a field, in UTF-16 offsets as
/// `NSString` counts them.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct QuickPart {
    pub start: u32,
    pub end: u32,
    pub kind: QuickPartKind,
}

/// A name or address as typed, and the person it is when that's clear.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct QuickPerson {
    pub typed: String,
    pub person: Option<PersonRecord>,
}

/// The event a line would make, for the line under the field.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct QuickPreview {
    pub title: String,
    /// "Fri 13 Nov, 12:30 to 13:00".
    pub when: String,
    /// False when the line names no day or time: `when` is the next half
    /// hour, and the editor will put the focus on the time.
    pub has_time: bool,
    /// The repeat rule in words.
    pub repeat: Option<String>,
    pub location: Option<String>,
    pub people: Vec<QuickPerson>,
    pub parts: Vec<QuickPart>,
    /// AI is on and will read the line on Return, as the parser left words.
    pub asks_ai: bool,
}

#[derive(uniffi::Record)]
pub struct QuickDraftInfo {
    pub draft: EventDraft,
    /// Shown as "Read by AI" in the editor.
    pub read_by_ai: bool,
    /// What the model said that failed a check.
    pub dropped: Vec<String>,
    /// No day or time was found: the focus goes to the start time.
    pub focus_time: bool,
}

fn prefs(core: &Core, languages: &[String]) -> Result<Prefs> {
    let minutes = core.jolta.store.read(store::default_event_minutes)?;
    let langs: Vec<Lang> = languages.iter().filter_map(|l| Lang::of(l)).collect();
    Ok(Prefs::new(minutes, &langs))
}

fn utf16(text: &str, byte: usize) -> u32 {
    text[..byte].encode_utf16().count() as u32
}

#[uniffi::export]
impl Core {
    /// What a quick create line would make, read on every keystroke.
    /// `languages` are tags such as "en" and "nl-BE", the app's language
    /// first: a tie between readings goes to the first.
    pub fn quick_parse(&self, text: String, languages: Vec<String>) -> Result<QuickPreview> {
        let prefs = prefs(self, &languages)?;
        let here = tz::here();
        let now = Utc::now();
        let parsed = quick::parse(&text, now.with_timezone(&here), &prefs);
        let span = parsed.when.unwrap_or_else(|| {
            let start = draft::next_half_hour(now, here);
            let at = |wall| Local { wall, zone: Zone::Tz(here) };
            Span::Timed { start: at(start), end: at(start + Duration::minutes(prefs.default_minutes)) }
        });
        let people = self.people()?;
        let ai_on = self.jolta.store.read(ai::settings)?.provider.is_some();
        let repeat = parsed.repeat.as_ref().and_then(|r| {
            let rec = jolta_core::calendar::Recurrence { rules: vec![r.clone()], ..Default::default() };
            jolta_core::calendar::recur::describe(&rec, &span)
        });
        Ok(QuickPreview {
            title: parsed.title.clone(),
            when: quick::when_text(&span, now.with_timezone(&here).date_naive(), here),
            has_time: parsed.when.is_some(),
            repeat,
            location: parsed.location.clone(),
            people: parsed
                .people
                .iter()
                .map(|typed| QuickPerson {
                    typed: typed.clone(),
                    person: match people.resolve(typed) {
                        Match::One(p) => Some(PersonRecord { name: p.name, address: p.address }),
                        Match::Unclear(_) => None,
                    },
                })
                .collect(),
            parts: parsed
                .parts
                .iter()
                .map(|(r, part)| QuickPart {
                    start: utf16(&text, r.start),
                    end: utf16(&text, r.end),
                    kind: match part {
                        Part::When => QuickPartKind::When,
                        Part::Person => QuickPartKind::Person,
                        Part::Place => QuickPartKind::Place,
                        Part::Repeat => QuickPartKind::Repeat,
                    },
                })
                .collect(),
            asks_ai: ai_on && ai::quick::wanted(&text, &parsed),
        })
    }

    /// The editor's draft for a line, read by the model too when the parser
    /// left words and AI is on (at most 5 seconds). Never saved here.
    pub async fn quick_draft(&self, text: String, languages: Vec<String>) -> Result<QuickDraftInfo> {
        let prefs = prefs(self, &languages)?;
        let people = self.people()?;
        let (jolta, engine) = (self.jolta.clone(), self.ai.clone());
        let q = self
            .runtime
            .spawn(async move { ai::quick::draft(&jolta, &engine, &people, &text, Utc::now(), tz::here(), &prefs, &Cancel::default()).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(QuickDraftInfo { draft: q.draft.into(), read_by_ai: q.by_ai, dropped: q.dropped, focus_time: q.focus_time })
    }

    /// G: the day a typed date names, as YYYY-MM-DD. None when it isn't one.
    pub fn go_to_date(&self, text: String, languages: Vec<String>) -> Result<Option<String>> {
        let prefs = prefs(self, &languages)?;
        let today = Utc::now().with_timezone(&tz::here()).date_naive();
        Ok(quick::go_to_date(&text, today, &prefs).map(|d| d.format("%Y-%m-%d").to_string()))
    }
}
