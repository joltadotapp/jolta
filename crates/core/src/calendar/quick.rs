//! Quick create (C3 spec, section 7): a line such as "Lunch with Jan fri
//! 12:30 at Mok" read in English, Dutch and French into an event's fields;
//! the clock times the editor's time fields take; and the day `G` goes to.
//!
//! The text is cut into words, and each language's reading takes words into
//! fields in two passes: repeat phrases first ("every other friday"), so a
//! later "until 18 dec" or "for 10 weeks" knows a rule is there, then dates,
//! times, lengths, people, places and zones from left to right. The reading
//! that takes the most words wins; what it doesn't take is the title.

use std::ops::Range;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use rusqlite::Connection;

use super::draft::{self, EventDraft, Unresolved};
use super::{Attendee, Local, Person, Recurrence, Response, Role, Span, Zone, recur, repeat, tz};
use crate::Result;
use crate::people::{Match, People};

/// A clock time typed in English, Dutch or French: "9", "930", "14:30",
/// "9.30", "2pm", "9u30", "9h", "noon", "midi". A bare hour from 1 to 7,
/// without minutes after a mark, am or pm, or a leading zero, is the
/// afternoon ("4" is 16:00, "130" is 13:30), as in quick create; "7u",
/// "7:00" and "07" stay in the morning.
pub fn time(text: &str) -> Option<NaiveTime> {
    let text: String = text.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if !text.is_ascii() {
        return None;
    }
    match text.as_str() {
        "noon" | "midday" | "midi" | "middag" => return NaiveTime::from_hms_opt(12, 0, 0),
        "midnight" | "minuit" | "middernacht" => return NaiveTime::from_hms_opt(0, 0, 0),
        _ => {}
    }
    let (digits, half) = match text.strip_suffix("pm").or_else(|| text.strip_suffix("p.m.")) {
        Some(d) => (d, Some(true)),
        None => match text.strip_suffix("am").or_else(|| text.strip_suffix("a.m.")) {
            Some(d) => (d, Some(false)),
            None => (text.as_str(), None),
        },
    };
    let (hour, minute, marked) = match digits.split_once([':', '.', 'h', 'u']) {
        Some((h, m)) => (number(h)?, if m.is_empty() { 0 } else { number(m).filter(|_| m.len() == 2)? }, true),
        None => match digits.len() {
            1 | 2 => (number(digits)?, 0, false),
            3 | 4 => (number(&digits[..digits.len() - 2])?, number(&digits[digits.len() - 2..])?, false),
            _ => return None,
        },
    };
    let hour = match half {
        Some(pm) if (1..=12).contains(&hour) => hour % 12 + if pm { 12 } else { 0 },
        Some(_) => return None,
        None if !marked && !digits.starts_with('0') && (1..=7).contains(&hour) => hour + 12,
        None => hour,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

fn number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    En,
    Nl,
    Fr,
}

impl Lang {
    /// The language of a tag such as "nl-BE" or "fr".
    pub fn of(tag: &str) -> Option<Lang> {
        match tag.get(..2)?.to_ascii_lowercase().as_str() {
            "en" => Some(Lang::En),
            "nl" => Some(Lang::Nl),
            "fr" => Some(Lang::Fr),
            _ => None,
        }
    }
}

/// How to read: the week's first day, a new event's length, and the
/// languages in the order that breaks a tie.
#[derive(Clone, Debug)]
pub struct Prefs {
    pub week_start: Weekday,
    pub default_minutes: i64,
    pub languages: Vec<Lang>,
}

impl Prefs {
    /// Weeks from Monday, and every language: `first` in that order, then the others.
    pub fn new(default_minutes: i64, first: &[Lang]) -> Prefs {
        let mut languages: Vec<Lang> = Vec::new();
        for l in first.iter().chain(&[Lang::En, Lang::Nl, Lang::Fr]) {
            if !languages.contains(l) {
                languages.push(*l);
            }
        }
        Prefs { week_start: Weekday::Mon, default_minutes, languages }
    }
}

/// What a range of the text became, for highlighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    When,
    Person,
    Place,
    Repeat,
}

/// A line read into an event.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed {
    /// The text without the words that became fields; names stay ("Lunch with Jan").
    pub title: String,
    /// None when the text names no day and no time: the editor starts at
    /// the next half hour and puts the focus on the time.
    pub when: Option<Span>,
    /// Names or addresses as typed.
    pub people: Vec<String>,
    pub location: Option<String>,
    /// An RRULE, UNTIL in UTC.
    pub repeat: Option<String>,
    /// Byte ranges of the text and what they became.
    pub parts: Vec<(Range<usize>, Part)>,
    /// Words that look like a date or time but weren't understood.
    pub leftover: Vec<String>,
    pub language: Lang,
}

/// Reads `text` as an event, relative to `now` in the Mac's zone.
pub fn parse(text: &str, now: DateTime<Tz>, prefs: &Prefs) -> Parsed {
    let toks = tokens(text);
    let today = now.date_naive();
    let mut best: Option<Reading> = None;
    for &lang in &prefs.languages {
        let r = Reading::read(&toks, lang, today, prefs.week_start, false);
        if best.as_ref().is_none_or(|b| r.score() > b.score()) {
            best = Some(r);
        }
    }
    best.expect("at least one language").finish(text, now, prefs)
}

/// The day `G` goes to: "fri", "next fri", "14 dec", "morgen", "2026-12-14",
/// "next week", "december". A day and month without a year is the nearest
/// one, before or after today.
pub fn go_to_date(text: &str, today: NaiveDate, prefs: &Prefs) -> Option<NaiveDate> {
    let toks = tokens(text);
    if toks.is_empty() {
        return None;
    }
    for &lang in &prefs.languages {
        let mut r = Reading::read(&toks, lang, today, prefs.week_start, true);
        if r.used.iter().all(|u| *u) && r.start.is_none() && r.rule.is_none() && r.people.is_empty() && r.place.is_none() {
            if let Some(day) = r.day.take() {
                return Some(match day {
                    Day::On(d) => d,
                    Day::Next(rule) => rule.next(today, true),
                });
            }
        }
        if toks.len() == 1 {
            if let Some(d) = r.lone_day(0) {
                return Some(d);
            }
        }
    }
    None
}

/// The one weekday a line names in any of the three languages ("the
/// friday after next"), to check the AI's day against. Short forms such
/// as "sun" and "sat" are words too often to count.
pub fn weekday_named(text: &str) -> Option<Weekday> {
    let mut found: Option<Weekday> = None;
    for tok in tokens(text).into_iter().filter(|t| t.key.chars().count() > 3) {
        for w in [&EN, &NL, &FR] {
            let key = tok.key.strip_suffix('s').filter(|_| std::ptr::eq(w, &FR)).unwrap_or(&tok.key);
            if let Some(n) = w.weekdays.iter().position(|names| names.contains(&key)) {
                match found {
                    Some(d) if d != WEEKDAYS[n] => return None,
                    _ => found = Some(WEEKDAYS[n]),
                }
            }
        }
    }
    found
}

/// The editor's draft for what `parse` read, in the default calendar. A
/// name becomes a guest only on one clear match among the people the user
/// has written to; otherwise it waits in `unresolved` for the user.
pub fn draft(conn: &Connection, people: &People, parsed: &Parsed, now: DateTime<Utc>, here: Tz) -> Result<EventDraft> {
    let mut d = draft::draft_for_new(conn, None, parsed.when, now, here)?;
    d.title.clone_from(&parsed.title);
    d.location = parsed.location.clone().unwrap_or_default();
    if let Some(rule) = &parsed.repeat {
        let rec = Recurrence { rules: vec![rule.clone()], ..Default::default() };
        d.repeat = recur::describe(&rec, &d.span);
        d.rule = Some(rec);
    }
    let person = |p: crate::people::Person| Person { address: p.address, name: p.name };
    for name in &parsed.people {
        match people.resolve(name) {
            Match::One(p) if d.people.iter().any(|a| a.person.address.eq_ignore_ascii_case(&p.address)) => {}
            Match::One(p) => d.people.push(Attendee { person: person(p), role: Role::Required, response: Response::NeedsAction, comment: String::new(), is_me: false }),
            Match::Unclear(choices) => d.unresolved.push(Unresolved { typed: name.clone(), choices: choices.into_iter().map(person).collect() }),
        }
    }
    Ok(d)
}

/// A draft's time on one line: "Fri 13 Nov, 12:30 to 13:00", "Tue 22 Dec
/// to Wed 23 Dec, all day", "Sat 14 Nov, 07:15 to 07:45 Lisbon (08:15 to
/// 08:45 here)". The year shows when it isn't this one.
pub fn when_text(span: &Span, today: NaiveDate, here: Tz) -> String {
    let day = |d: NaiveDate| if d.year() == today.year() { d.format("%a %-d %b").to_string() } else { d.format("%a %-d %b %Y").to_string() };
    match span {
        Span::AllDay { start, end } => {
            let last = (*end - Duration::days(1)).max(*start);
            if last == *start { format!("{}, all day", day(*start)) } else { format!("{} to {}, all day", day(*start), day(last)) }
        }
        Span::Timed { start, end } => {
            let text = match super::detail::when(span, span.bounds(here), here) {
                super::detail::When::Timed { text, .. } => text,
                super::detail::When::AllDay { .. } => String::new(),
            };
            let one_day = end.wall.date() == start.wall.date() || *end == Local { wall: (start.wall.date() + Duration::days(1)).and_time(NaiveTime::MIN), ..*start };
            if one_day { format!("{}, {text}", day(start.wall.date())) } else { text }
        }
    }
}

// ---------------------------------------------------------------------------
// Words

struct Tok {
    range: Range<usize>,
    /// Lowercase, accents and curly quotes folded: "déc" is "dec".
    key: String,
    text: String,
    capital: bool,
}

fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ä' | 'ã' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ò' | 'ó' | 'ô' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ñ' => 'n',
            '’' | '‘' | '`' => '\'',
            '–' | '—' => '-',
            c => c,
        })
        .collect()
}

fn tokens(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                start = None;
                let word = &text[s..i];
                let lead = word.len() - word.trim_start_matches(['(', '[', '"', '\'', '«', '“', '‘']).len();
                let mut end = word.len();
                loop {
                    let w = &word[lead..end];
                    let Some(last) = w.chars().last() else { break };
                    let keep_dot = last == '.' && (w.ends_with("a.m.") || w.ends_with("p.m."));
                    if !keep_dot && ",;:!?)]\"'»”’….".contains(last) {
                        end -= last.len_utf8();
                    } else {
                        break;
                    }
                }
                if lead < end {
                    let w = &word[lead..end];
                    out.push(Tok { range: s + lead..s + end, key: fold(w), text: w.to_string(), capital: w.chars().next().is_some_and(char::is_uppercase) });
                }
            }
            _ => {}
        }
    }
    out
}

/// Each language's words, folded.
struct Words {
    weekdays: [&'static [&'static str]; 7],
    /// Dutch "ma", "di", "do": weekdays only next to a time or date.
    short_weekdays: [&'static str; 7],
    /// The weekday of next week: "next fri", "volgende vrijdag"; "vendredi prochain" after.
    next: &'static [&'static str],
    next_after: &'static [&'static str],
    this: &'static [&'static str],
    /// Words in front of a date that go with it: "on", "op", "le".
    on: &'static [&'static str],
    days: &'static [(&'static [&'static str], i64)],
    /// In front of a time: "at", "om", "à".
    at: &'static [&'static str],
    /// "in 2 hours", "over 2 uur", "dans 2 heures".
    after: &'static [&'static str],
    from: &'static [&'static str],
    to: &'static [&'static [&'static str]],
    /// In front of a length: "for", "pendant".
    lasting: &'static [&'static str],
    lengths: &'static [(&'static [&'static str], i64)],
    all_day: &'static [&'static [&'static str]],
    with: &'static [&'static str],
    and: &'static [&'static str],
    place: &'static [&'static str],
    /// Not names: "with the team".
    not_names: &'static [&'static str],
    numbers: &'static [(&'static str, u32)],
    repeats: &'static [(&'static [P], Kind)],
    until: &'static [&'static [&'static str]],
    /// "for <n> weeks", "<n> weken lang", "pendant <n> semaines": the words around the number.
    for_weeks: &'static [(&'static [&'static str], &'static [&'static str])],
    times: &'static [&'static str],
    /// "Lisbon time", "Lissabon tijd" after a city; "heure de Lisbonne" before.
    zone_after: &'static [&'static str],
    zone_before: &'static [&'static [&'static str]],
    /// Words that point at a day or time, to tell the AI fallback something was left.
    datey: &'static [&'static str],
}

/// A piece of a repeat phrase.
#[derive(Clone, Copy)]
enum P {
    W(&'static str),
    /// A weekday, plural too: "lundis".
    Wd,
    /// 2, "two", "twee", "deux".
    N,
    /// "25th", "25e", "25".
    Nth,
}

#[derive(Clone, Copy)]
enum Kind {
    Every(Freq, u32),
    /// Every n weeks on the weekday.
    OnWeekday(u32),
    Workdays,
    MonthDay,
    LastWeekday,
}

use P::{N, Nth, W, Wd};

const MINUTE: i64 = 1;
const HOUR: i64 = 60;

static EN: Words = Words {
    weekdays: [
        &["monday", "mon"],
        &["tuesday", "tue", "tues"],
        &["wednesday", "wed"],
        &["thursday", "thu", "thur", "thurs"],
        &["friday", "fri"],
        &["saturday", "sat"],
        &["sunday", "sun"],
    ],
    short_weekdays: ["", "", "", "", "", "", ""],
    next: &["next"],
    next_after: &[],
    this: &["this"],
    on: &["on"],
    days: &[(&["today"], 0), (&["tomorrow"], 1), (&["tmrw"], 1), (&["day", "after", "tomorrow"], 2)],
    at: &["at", "@"],
    after: &["in"],
    from: &["from"],
    to: &[&["to"], &["until"], &["till"], &["-"], &["through"]],
    lasting: &["for"],
    lengths: &[
        (&["m", "min", "mins", "minute", "minutes"], MINUTE),
        (&["h", "hr", "hrs", "hour", "hours"], HOUR),
    ],
    all_day: &[&["all", "day"], &["all-day"], &["allday"], &["the", "whole", "day"], &["whole", "day"]],
    with: &["with"],
    and: &["and", "&"],
    place: &["at", "@", "in"],
    not_names: &["the", "a", "an", "my", "our", "your", "his", "her", "their", "team", "everyone", "me", "us", "them"],
    numbers: &[
        ("one", 1),
        ("two", 2),
        ("three", 3),
        ("four", 4),
        ("five", 5),
        ("six", 6),
        ("seven", 7),
        ("eight", 8),
        ("nine", 9),
        ("ten", 10),
        ("eleven", 11),
        ("twelve", 12),
        ("an", 1),
        ("a", 1),
    ],
    repeats: &[
        (&[W("every"), W("other"), Wd], Kind::OnWeekday(2)),
        (&[W("every"), N, W("weeks")], Kind::Every(Freq::Weekly, 0)),
        (&[W("every"), N, W("days")], Kind::Every(Freq::Daily, 0)),
        (&[W("every"), N, W("months")], Kind::Every(Freq::Monthly, 0)),
        (&[W("every"), N, W("years")], Kind::Every(Freq::Yearly, 0)),
        (&[W("every"), W("other"), W("week")], Kind::Every(Freq::Weekly, 2)),
        (&[W("every"), W("other"), W("day")], Kind::Every(Freq::Daily, 2)),
        (&[W("biweekly")], Kind::Every(Freq::Weekly, 2)),
        (&[W("every"), W("weekday")], Kind::Workdays),
        (&[W("every"), W("workday")], Kind::Workdays),
        (&[W("weekdays")], Kind::Workdays),
        (&[W("every"), W("last"), Wd, W("of"), W("the"), W("month")], Kind::LastWeekday),
        (&[W("the"), W("last"), Wd, W("of"), W("the"), W("month")], Kind::LastWeekday),
        (&[W("last"), Wd, W("of"), W("the"), W("month")], Kind::LastWeekday),
        (&[W("last"), Wd, W("of"), W("every"), W("month")], Kind::LastWeekday),
        (&[W("every"), Wd], Kind::OnWeekday(1)),
        (&[W("every"), W("day")], Kind::Every(Freq::Daily, 1)),
        (&[W("daily")], Kind::Every(Freq::Daily, 1)),
        (&[W("every"), W("week")], Kind::Every(Freq::Weekly, 1)),
        (&[W("weekly")], Kind::Every(Freq::Weekly, 1)),
        (&[W("monthly"), W("on"), W("the"), Nth], Kind::MonthDay),
        (&[W("monthly"), W("on"), Nth], Kind::MonthDay),
        (&[W("every"), W("month"), W("on"), W("the"), Nth], Kind::MonthDay),
        (&[W("every"), W("month"), W("on"), Nth], Kind::MonthDay),
        (&[W("on"), W("the"), Nth, W("of"), W("every"), W("month")], Kind::MonthDay),
        (&[W("every"), W("month")], Kind::Every(Freq::Monthly, 1)),
        (&[W("monthly")], Kind::Every(Freq::Monthly, 1)),
        (&[W("every"), W("year")], Kind::Every(Freq::Yearly, 1)),
        (&[W("yearly")], Kind::Every(Freq::Yearly, 1)),
        (&[W("annually")], Kind::Every(Freq::Yearly, 1)),
    ],
    until: &[&["until"], &["till"], &["through"]],
    for_weeks: &[(&["for"], &["weeks"])],
    times: &["times"],
    zone_after: &["time"],
    zone_before: &[],
    datey: &[
        "next", "last", "this", "after", "before", "around", "about", "morning", "afternoon", "evening", "tonight", "night", "week",
        "weekend", "month", "year", "o'clock", "oclock", "midday", "lunchtime", "noon", "two", "three", "four", "five", "six", "seven",
        "eight", "nine", "ten", "eleven", "twelve", "january", "february", "march", "april", "june", "july", "august", "september",
        "october", "november", "december", "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "today",
        "tomorrow", "yesterday", "fortnight",
    ],
};

static NL: Words = Words {
    weekdays: [&["maandag"], &["dinsdag"], &["woensdag"], &["donderdag"], &["vrijdag"], &["zaterdag"], &["zondag"]],
    short_weekdays: ["ma", "di", "wo", "do", "vr", "za", "zo"],
    next: &["volgende", "komende"],
    next_after: &[],
    this: &["deze"],
    on: &["op"],
    days: &[(&["vandaag"], 0), (&["morgen"], 1), (&["overmorgen"], 2)],
    at: &["om", "@"],
    after: &["over", "binnen"],
    from: &["van"],
    to: &[&["tot", "en", "met"], &["tot"], &["t/m"], &["-"]],
    lasting: &["gedurende"],
    lengths: &[
        (&["m", "min", "minuut", "minuten"], MINUTE),
        (&["u", "uur", "uren"], HOUR),
    ],
    all_day: &[&["de", "hele", "dag"], &["hele", "dag"], &["heel", "de", "dag"], &["ganse", "dag"], &["de", "ganse", "dag"]],
    with: &["met"],
    and: &["en", "&"],
    place: &["bij", "in", "@"],
    not_names: &["de", "het", "een", "mijn", "ons", "onze", "je", "jouw", "team", "iedereen", "mij", "ons", "hen"],
    numbers: &[
        ("een", 1),
        ("twee", 2),
        ("drie", 3),
        ("vier", 4),
        ("vijf", 5),
        ("zes", 6),
        ("zeven", 7),
        ("acht", 8),
        ("negen", 9),
        ("tien", 10),
        ("elf", 11),
        ("twaalf", 12),
    ],
    repeats: &[
        (&[W("om"), W("de"), N, W("weken")], Kind::Every(Freq::Weekly, 0)),
        (&[W("om"), W("de"), N, W("dagen")], Kind::Every(Freq::Daily, 0)),
        (&[W("om"), W("de"), N, W("maanden")], Kind::Every(Freq::Monthly, 0)),
        (&[W("om"), W("de"), N, W("jaar")], Kind::Every(Freq::Yearly, 0)),
        (&[W("om"), W("de"), W("week")], Kind::Every(Freq::Weekly, 2)),
        (&[W("tweewekelijks")], Kind::Every(Freq::Weekly, 2)),
        (&[W("elke"), W("werkdag")], Kind::Workdays),
        (&[W("iedere"), W("werkdag")], Kind::Workdays),
        (&[W("alle"), W("werkdagen")], Kind::Workdays),
        (&[W("elke"), W("laatste"), Wd, W("van"), W("de"), W("maand")], Kind::LastWeekday),
        (&[W("de"), W("laatste"), Wd, W("van"), W("de"), W("maand")], Kind::LastWeekday),
        (&[W("laatste"), Wd, W("van"), W("de"), W("maand")], Kind::LastWeekday),
        (&[W("elke"), Wd], Kind::OnWeekday(1)),
        (&[W("iedere"), Wd], Kind::OnWeekday(1)),
        (&[W("elke"), W("dag")], Kind::Every(Freq::Daily, 1)),
        (&[W("iedere"), W("dag")], Kind::Every(Freq::Daily, 1)),
        (&[W("dagelijks")], Kind::Every(Freq::Daily, 1)),
        (&[W("elke"), W("week")], Kind::Every(Freq::Weekly, 1)),
        (&[W("iedere"), W("week")], Kind::Every(Freq::Weekly, 1)),
        (&[W("wekelijks")], Kind::Every(Freq::Weekly, 1)),
        (&[W("maandelijks"), W("op"), W("de"), Nth], Kind::MonthDay),
        (&[W("elke"), W("maand"), W("op"), W("de"), Nth], Kind::MonthDay),
        (&[W("elke"), W("maand")], Kind::Every(Freq::Monthly, 1)),
        (&[W("iedere"), W("maand")], Kind::Every(Freq::Monthly, 1)),
        (&[W("maandelijks")], Kind::Every(Freq::Monthly, 1)),
        (&[W("elk"), W("jaar")], Kind::Every(Freq::Yearly, 1)),
        (&[W("ieder"), W("jaar")], Kind::Every(Freq::Yearly, 1)),
        (&[W("jaarlijks")], Kind::Every(Freq::Yearly, 1)),
    ],
    until: &[&["tot", "en", "met"], &["t/m"], &["tot"]],
    for_weeks: &[(&["gedurende"], &["weken"]), (&[], &["weken", "lang"])],
    times: &["keer", "maal"],
    zone_after: &["tijd"],
    zone_before: &[],
    datey: &[
        "volgende", "vorige", "deze", "na", "voor", "rond", "rond", "ochtend", "voormiddag", "namiddag", "middag", "avond", "vanavond",
        "vanmiddag", "vanochtend", "nacht", "week", "weekend", "maand", "jaar", "uur", "twee", "drie", "vier", "vijf", "zes", "zeven",
        "acht", "negen", "tien", "elf", "twaalf", "januari", "februari", "maart", "april", "mei", "juni", "juli", "augustus",
        "september", "oktober", "november", "december", "maandag", "dinsdag", "woensdag", "donderdag", "vrijdag", "zaterdag",
        "zondag", "vandaag", "morgen", "overmorgen", "gisteren", "kwart", "half",
    ],
};

static FR: Words = Words {
    weekdays: [&["lundi"], &["mardi"], &["mercredi"], &["jeudi"], &["vendredi"], &["samedi"], &["dimanche"]],
    short_weekdays: ["", "", "", "", "", "", ""],
    next: &["prochain", "prochaine"],
    next_after: &["prochain", "prochaine"],
    this: &["ce"],
    on: &["le"],
    days: &[(&["aujourd'hui"], 0), (&["demain"], 1), (&["apres-demain"], 2), (&["apres", "demain"], 2)],
    at: &["a", "@", "vers"],
    after: &["dans"],
    from: &["de", "du"],
    to: &[&["a"], &["au"], &["jusqu'a"], &["jusqu'au"], &["-"]],
    lasting: &["pendant", "durant", "pour"],
    lengths: &[
        (&["m", "min", "mn", "minute", "minutes"], MINUTE),
        (&["h", "heure", "heures"], HOUR),
    ],
    all_day: &[&["toute", "la", "journee"], &["journee", "entiere"], &["toute", "la", "journee", "entiere"]],
    with: &["avec"],
    and: &["et", "&"],
    place: &["chez", "a", "@"],
    not_names: &["le", "la", "les", "l'", "un", "une", "mon", "ma", "mes", "notre", "nos", "ton", "ta", "son", "sa", "equipe", "tous", "moi", "nous", "eux"],
    numbers: &[
        ("un", 1),
        ("une", 1),
        ("deux", 2),
        ("trois", 3),
        ("quatre", 4),
        ("cinq", 5),
        ("six", 6),
        ("sept", 7),
        ("huit", 8),
        ("neuf", 9),
        ("dix", 10),
        ("onze", 11),
        ("douze", 12),
    ],
    repeats: &[
        (&[W("un"), Wd, W("sur"), N], Kind::OnWeekday(0)),
        (&[W("toutes"), W("les"), N, W("semaines")], Kind::Every(Freq::Weekly, 0)),
        (&[W("tous"), W("les"), N, W("jours")], Kind::Every(Freq::Daily, 0)),
        (&[W("tous"), W("les"), N, W("mois")], Kind::Every(Freq::Monthly, 0)),
        (&[W("tous"), W("les"), N, W("ans")], Kind::Every(Freq::Yearly, 0)),
        (&[W("chaque"), W("jour"), W("ouvrable")], Kind::Workdays),
        (&[W("tous"), W("les"), W("jours"), W("ouvrables")], Kind::Workdays),
        (&[W("en"), W("semaine")], Kind::Workdays),
        (&[W("chaque"), W("dernier"), Wd, W("du"), W("mois")], Kind::LastWeekday),
        (&[W("le"), W("dernier"), Wd, W("du"), W("mois")], Kind::LastWeekday),
        (&[W("dernier"), Wd, W("du"), W("mois")], Kind::LastWeekday),
        (&[W("tous"), W("les"), Wd], Kind::OnWeekday(1)),
        (&[W("chaque"), Wd], Kind::OnWeekday(1)),
        (&[W("tous"), W("les"), W("jours")], Kind::Every(Freq::Daily, 1)),
        (&[W("chaque"), W("jour")], Kind::Every(Freq::Daily, 1)),
        (&[W("quotidien")], Kind::Every(Freq::Daily, 1)),
        (&[W("chaque"), W("semaine")], Kind::Every(Freq::Weekly, 1)),
        (&[W("toutes"), W("les"), W("semaines")], Kind::Every(Freq::Weekly, 1)),
        (&[W("le"), Nth, W("de"), W("chaque"), W("mois")], Kind::MonthDay),
        (&[W("tous"), W("les"), W("mois"), W("le"), Nth], Kind::MonthDay),
        (&[W("tous"), W("les"), W("mois")], Kind::Every(Freq::Monthly, 1)),
        (&[W("chaque"), W("mois")], Kind::Every(Freq::Monthly, 1)),
        (&[W("tous"), W("les"), W("ans")], Kind::Every(Freq::Yearly, 1)),
        (&[W("chaque"), W("annee")], Kind::Every(Freq::Yearly, 1)),
        (&[W("chaque"), W("an")], Kind::Every(Freq::Yearly, 1)),
    ],
    until: &[&["jusqu'au"], &["jusqu'a"], &["jusqu'au", "au"]],
    for_weeks: &[(&["pendant"], &["semaines"])],
    times: &["fois"],
    zone_after: &[],
    zone_before: &[&["heure", "de"], &["heure", "d'"]],
    datey: &[
        "prochain", "prochaine", "dernier", "derniere", "ce", "apres", "avant", "vers", "matin", "apres-midi", "soir", "ce soir",
        "nuit", "semaine", "week-end", "mois", "an", "annee", "heure", "heures", "deux", "trois", "quatre", "cinq", "six", "sept",
        "huit", "neuf", "dix", "onze", "douze", "janvier", "fevrier", "mars", "avril", "mai", "juin", "juillet", "aout",
        "septembre", "octobre", "novembre", "decembre", "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche",
        "aujourd'hui", "demain", "apres-demain", "hier", "midi", "minuit",
    ],
};

/// Month names and abbreviations in the three languages, folded.
const MONTHS: [&[&str]; 12] = [
    &["january", "jan", "januari", "janvier", "janv"],
    &["february", "feb", "februari", "fevrier", "fevr", "fev"],
    &["march", "mar", "maart", "mrt", "mars"],
    &["april", "apr", "avril", "avr"],
    &["may", "mei", "mai"],
    &["june", "jun", "juni", "juin"],
    &["july", "jul", "juli", "juillet", "juil"],
    &["august", "aug", "augustus", "aout"],
    &["september", "sep", "sept", "septembre"],
    &["october", "oct", "oktober", "okt", "octobre"],
    &["november", "nov", "novembre"],
    &["december", "dec", "decembre"],
];

/// City names in Dutch and French that IANA spells otherwise.
const CITIES: &[(&str, Tz)] = &[
    ("lisbonne", Tz::Europe__Lisbon),
    ("lissabon", Tz::Europe__Lisbon),
    ("londres", Tz::Europe__London),
    ("londen", Tz::Europe__London),
    ("bruxelles", Tz::Europe__Brussels),
    ("brussel", Tz::Europe__Brussels),
    ("parijs", Tz::Europe__Paris),
    ("rome", Tz::Europe__Rome),
    ("athenes", Tz::Europe__Athens),
    ("athene", Tz::Europe__Athens),
    ("varsovie", Tz::Europe__Warsaw),
    ("warschau", Tz::Europe__Warsaw),
    ("moscou", Tz::Europe__Moscow),
    ("moskou", Tz::Europe__Moscow),
    ("new york", Tz::America__New_York),
    ("san francisco", Tz::America__Los_Angeles),
    ("tokio", Tz::Asia__Tokyo),
    ("pekin", Tz::Asia__Shanghai),
    ("peking", Tz::Asia__Shanghai),
    ("beijing", Tz::Asia__Shanghai),
];

fn words(lang: Lang) -> &'static Words {
    match lang {
        Lang::En => &EN,
        Lang::Nl => &NL,
        Lang::Fr => &FR,
    }
}

fn month(key: &str) -> Option<u32> {
    let key = key.trim_end_matches('.');
    MONTHS.iter().position(|names| names.contains(&key)).map(|i| i as u32 + 1)
}

/// "25", "25th", "1st", "25e", "1er", "2ste".
fn day_number(key: &str) -> Option<u32> {
    let digits = key.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &key[digits.len()..];
    if !["", "st", "nd", "rd", "th", "e", "ste", "de", "er", "eme"].contains(&suffix) || digits.len() > 2 {
        return None;
    }
    number(digits).filter(|d| (1..=31).contains(d))
}

const WEEKDAYS: [Weekday; 7] = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat, Weekday::Sun];

fn last_weekday_of_month(d: NaiveDate, wd: Weekday) -> bool {
    d.weekday() == wd && (d + Duration::days(7)).month() != d.month()
}

// ---------------------------------------------------------------------------
// Reading

#[derive(Clone, Copy, Debug)]
enum DayRule {
    Weekday(Weekday),
    Workday,
    MonthDay(u32),
    LastWeekday(Weekday),
}

impl DayRule {
    fn matches(self, d: NaiveDate) -> bool {
        match self {
            DayRule::Weekday(wd) => d.weekday() == wd,
            DayRule::Workday => d.weekday().num_days_from_monday() < 5,
            DayRule::MonthDay(n) => d.day() == n,
            DayRule::LastWeekday(wd) => last_weekday_of_month(d, wd),
        }
    }

    /// The first day from `today` it matches, today only when `today_ok`.
    fn next(self, today: NaiveDate, today_ok: bool) -> NaiveDate {
        let from = if today_ok { 0 } else { 1 };
        (from..=400).map(|n| today + Duration::days(n)).find(|d| self.matches(*d)).unwrap_or(today)
    }
}

#[derive(Clone, Copy, Debug)]
enum Day {
    On(NaiveDate),
    /// The next day this matches, today included while its time is ahead.
    Next(DayRule),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Clone, Debug)]
enum By {
    Nothing,
    Days(Vec<Weekday>),
    MonthDay(u32),
    Last(Weekday),
}

#[derive(Clone, Debug)]
struct Rule {
    freq: Freq,
    interval: u32,
    by: By,
}

enum Effect {
    Day(Day),
    /// The first and last day of an all-day range.
    Days(NaiveDate, NaiveDate),
    /// A clock time; `lasts` is what it means as a length ("1h30") when it
    /// would end the event before it starts.
    Time { at: NaiveTime, lasts: Option<Duration> },
    Times(NaiveTime, NaiveTime),
    Lasts(Duration),
    /// From now: "in 2 hours".
    After(Duration),
    AllDay,
    Zone(Tz),
    Rule(Rule, Option<DayRule>),
    Until(NaiveDate),
    ForWeeks(u32),
    Count(u32),
    /// Names stay in the title (`keep`, token indices); addresses leave it.
    People { names: Vec<String>, keep: Vec<usize>, parts: Vec<Range<usize>> },
    Place(String),
}

struct Hit {
    len: usize,
    effect: Effect,
}

impl Hit {
    fn new(len: usize, effect: Effect) -> Option<Hit> {
        Some(Hit { len, effect })
    }
}

struct Reading<'a> {
    toks: &'a [Tok],
    w: &'static Words,
    lang: Lang,
    today: NaiveDate,
    week_start: Weekday,
    /// `G`: a day and month without a year is the nearest one.
    nearest: bool,
    used: Vec<bool>,
    kept: Vec<bool>,
    /// Token ranges and what they became.
    hits: Vec<(Range<usize>, Part)>,
    day: Option<Day>,
    last_day: Option<NaiveDate>,
    start: Option<NaiveTime>,
    end: Option<NaiveTime>,
    lasts: Option<Duration>,
    after: Option<Duration>,
    all_day: bool,
    zone: Option<Tz>,
    rule: Option<Rule>,
    rule_day: Option<DayRule>,
    until: Option<NaiveDate>,
    for_weeks: Option<u32>,
    count: Option<u32>,
    people: Vec<String>,
    place: Option<String>,
    /// The token after the last day read, so "monday 9" reads the 9 as a time.
    after_day: Option<usize>,
}

impl<'a> Reading<'a> {
    fn read(toks: &'a [Tok], lang: Lang, today: NaiveDate, week_start: Weekday, nearest: bool) -> Reading<'a> {
        let mut r = Reading {
            toks,
            w: words(lang),
            lang,
            today,
            week_start,
            nearest,
            used: vec![false; toks.len()],
            kept: vec![false; toks.len()],
            hits: Vec::new(),
            day: None,
            last_day: None,
            start: None,
            end: None,
            lasts: None,
            after: None,
            all_day: false,
            zone: None,
            rule: None,
            rule_day: None,
            until: None,
            for_weeks: None,
            count: None,
            people: Vec::new(),
            place: None,
            after_day: None,
        };
        if !nearest {
            for i in 0..toks.len() {
                if let Some(hit) = r.repeat_at(i) {
                    r.take(i, hit);
                }
            }
        }
        let mut i = 0;
        while i < toks.len() {
            if !r.used[i] {
                if let Some(hit) = r.hit_at(i) {
                    let len = hit.len;
                    if r.take(i, hit) {
                        i += len;
                        continue;
                    }
                }
            }
            i += 1;
        }
        r
    }

    fn score(&self) -> usize {
        self.used.iter().zip(&self.kept).filter(|(u, k)| **u || **k).count()
    }

    /// The folded word at `i`, or "" past the end or once taken.
    fn k(&self, i: usize) -> &str {
        match self.toks.get(i) {
            Some(t) if !self.used[i] => &t.key,
            _ => "",
        }
    }

    /// Whether the words from `i` are `seq`.
    fn seq(&self, i: usize, seq: &[&str]) -> bool {
        !seq.is_empty() && seq.iter().enumerate().all(|(n, s)| self.k(i + n) == *s)
    }

    /// The longest of `seqs` at `i`, as its length.
    fn any_seq(&self, i: usize, seqs: &[&[&str]]) -> Option<usize> {
        seqs.iter().filter(|s| self.seq(i, s)).map(|s| s.len()).max()
    }

    fn one_of(&self, i: usize, list: &[&str]) -> bool {
        let k = self.k(i);
        !k.is_empty() && list.contains(&k)
    }

    fn amount(&self, i: usize) -> Option<u32> {
        let k = self.k(i);
        number(k).filter(|_| k.len() <= 3).or_else(|| self.w.numbers.iter().find(|(w, _)| *w == k).map(|(_, n)| *n))
    }

    fn weekday(&self, i: usize, plural: bool) -> Option<Weekday> {
        let k = self.k(i);
        let k = if plural && self.lang == Lang::Fr { k.strip_suffix('s').unwrap_or(k) } else { k };
        self.w.weekdays.iter().position(|names| names.contains(&k)).map(|n| WEEKDAYS[n])
    }

    /// Records a hit if its field is still free.
    fn take(&mut self, i: usize, hit: Hit) -> bool {
        let span = i..i + hit.len;
        let part = match &hit.effect {
            Effect::Rule(..) | Effect::Until(_) | Effect::ForWeeks(_) | Effect::Count(_) => Part::Repeat,
            Effect::People { .. } => Part::Person,
            Effect::Place(_) => Part::Place,
            _ => Part::When,
        };
        let is_day = matches!(hit.effect, Effect::Day(_) | Effect::Days(..));
        match hit.effect {
            Effect::Day(d) => {
                if self.day.is_some() || self.after.is_some() || self.rule_day.is_some() {
                    return false;
                }
                self.day = Some(d);
            }
            Effect::Days(first, last) => {
                if self.day.is_some() || self.after.is_some() || self.rule_day.is_some() || self.start.is_some() {
                    return false;
                }
                self.day = Some(Day::On(first));
                self.last_day = Some(last);
            }
            Effect::Time { at, lasts } => match (self.start, self.end, self.lasts) {
                _ if self.after.is_some() || self.all_day || self.last_day.is_some() => return false,
                (None, _, _) => self.start = Some(at),
                (Some(s), None, None) if at <= s => match lasts {
                    Some(l) => self.lasts = Some(l),
                    None => return false,
                },
                (Some(_), None, None) => self.end = Some(at),
                _ => return false,
            },
            Effect::Times(a, b) => {
                if self.start.is_some() || self.after.is_some() || self.all_day || self.last_day.is_some() {
                    return false;
                }
                self.start = Some(a);
                self.end = Some(b);
            }
            Effect::Lasts(d) => {
                if self.lasts.is_some() || self.end.is_some() || self.all_day {
                    return false;
                }
                self.lasts = Some(d);
            }
            Effect::After(d) => {
                if self.after.is_some() || self.day.is_some() || self.start.is_some() || self.rule_day.is_some() {
                    return false;
                }
                self.after = Some(d);
            }
            Effect::AllDay => {
                if self.all_day || self.start.is_some() || self.after.is_some() {
                    return false;
                }
                self.all_day = true;
            }
            Effect::Zone(z) => {
                if self.zone.is_some() {
                    return false;
                }
                self.zone = Some(z);
            }
            Effect::Rule(rule, day) => {
                if self.rule.is_some() {
                    return false;
                }
                self.rule = Some(rule);
                self.rule_day = day;
            }
            Effect::Until(d) => {
                if self.until.is_some() || self.for_weeks.is_some() || self.count.is_some() {
                    return false;
                }
                self.until = Some(d);
            }
            Effect::ForWeeks(n) => {
                if self.until.is_some() || self.for_weeks.is_some() || self.count.is_some() {
                    return false;
                }
                self.for_weeks = Some(n);
            }
            Effect::Count(n) => {
                if self.until.is_some() || self.for_weeks.is_some() || self.count.is_some() {
                    return false;
                }
                self.count = Some(n);
            }
            Effect::People { names, keep, parts } => {
                self.people.extend(names);
                for t in span.clone() {
                    if keep.contains(&t) {
                        self.kept[t] = true;
                    } else {
                        self.used[t] = true;
                    }
                }
                self.hits.extend(parts.into_iter().map(|p| (p, Part::Person)));
                return true;
            }
            Effect::Place(p) => {
                if self.place.is_some() {
                    return false;
                }
                self.place = Some(p);
            }
        }
        if is_day {
            self.after_day = Some(span.end);
        }
        for t in span.clone() {
            self.used[t] = true;
        }
        self.hits.push((span, part));
        true
    }

    // -- Repeats

    fn repeat_at(&self, i: usize) -> Option<Hit> {
        'patterns: for (pattern, kind) in self.w.repeats {
            let (mut wd, mut n, mut nth) = (None, None, None);
            for (p, piece) in pattern.iter().enumerate() {
                let at = i + p;
                match piece {
                    W(word) if self.k(at) == *word => {}
                    Wd => match self.weekday(at, true) {
                        Some(d) => wd = Some(d),
                        None => continue 'patterns,
                    },
                    N => match self.amount(at).filter(|n| (1..=99).contains(n)) {
                        Some(x) => n = Some(x),
                        None => continue 'patterns,
                    },
                    Nth => match day_number(self.k(at)) {
                        Some(d) => nth = Some(d),
                        None => continue 'patterns,
                    },
                    _ => continue 'patterns,
                }
            }
            let (rule, day) = match *kind {
                Kind::Every(freq, fixed) => (Rule { freq, interval: if fixed == 0 { n? } else { fixed }, by: By::Nothing }, None),
                Kind::OnWeekday(fixed) => {
                    let wd = wd?;
                    let interval = if fixed == 0 { n? } else { fixed };
                    (Rule { freq: Freq::Weekly, interval, by: By::Days(vec![wd]) }, Some(DayRule::Weekday(wd)))
                }
                Kind::Workdays => (
                    Rule { freq: Freq::Weekly, interval: 1, by: By::Days(WEEKDAYS[..5].to_vec()) },
                    Some(DayRule::Workday),
                ),
                Kind::MonthDay => {
                    let d = nth?;
                    (Rule { freq: Freq::Monthly, interval: 1, by: By::MonthDay(d) }, Some(DayRule::MonthDay(d)))
                }
                Kind::LastWeekday => {
                    let wd = wd?;
                    (Rule { freq: Freq::Monthly, interval: 1, by: By::Last(wd) }, Some(DayRule::LastWeekday(wd)))
                }
            };
            return Hit::new(pattern.len(), Effect::Rule(rule, day));
        }
        None
    }

    /// "until 18 dec", "for 10 weeks", "5 times", once a rule was read.
    fn repeat_end_at(&self, i: usize) -> Option<Hit> {
        self.rule.as_ref()?;
        if let Some(len) = self.any_seq(i, self.w.until) {
            if let Some((n, d)) = self.date_at(i + len) {
                return Hit::new(len + n, Effect::Until(d));
            }
        }
        for (before, after) in self.w.for_weeks {
            let at = i + before.len();
            if (before.is_empty() || self.seq(i, before)) && self.seq(at + 1, after) {
                if let Some(n) = self.amount(at).filter(|n| (1..=520).contains(n)) {
                    return Hit::new(before.len() + 1 + after.len(), Effect::ForWeeks(n));
                }
            }
        }
        if let Some(n) = self.amount(i).filter(|n| (1..=999).contains(n)) {
            if self.one_of(i + 1, self.w.times) {
                return Hit::new(2, Effect::Count(n));
            }
        }
        None
    }

    // -- Everything else, in the order a word is tried

    fn hit_at(&self, i: usize) -> Option<Hit> {
        self.repeat_end_at(i)
            .or_else(|| self.after_at(i))
            .or_else(|| self.day_range_at(i))
            .or_else(|| self.day_at(i))
            .or_else(|| self.time_range_at(i))
            .or_else(|| if self.lang == Lang::En { self.length_at(i).or_else(|| self.time_at(i)) } else { self.time_at(i).or_else(|| self.length_at(i)) })
            .or_else(|| self.any_seq(i, self.w.all_day).and_then(|len| Hit::new(len, Effect::AllDay)))
            .or_else(|| self.zone_at(i))
            .or_else(|| if self.nearest { None } else { self.people_at(i).or_else(|| self.place_at(i)) })
    }

    /// Whether a field other than a person or place starts at `i`.
    fn starts_part(&self, i: usize) -> bool {
        self.repeat_end_at(i).is_some()
            || self.after_at(i).is_some()
            || self.day_range_at(i).is_some()
            || self.day_at(i).is_some()
            || self.time_range_at(i).is_some()
            || self.time_at(i).is_some()
            || self.length_at(i).is_some()
            || self.any_seq(i, self.w.all_day).is_some()
            || self.zone_at(i).is_some()
    }

    // -- Days

    /// A day and month as typed, the year if given: "14 dec", "dec 14",
    /// "14/12", "2026-12-14", "14 december 2026".
    fn day_month_at(&self, i: usize) -> Option<(usize, u32, u32, Option<i32>)> {
        let k = self.k(i);
        if k.is_empty() {
            return None;
        }
        if let Ok(d) = NaiveDate::parse_from_str(k, "%Y-%m-%d") {
            return Some((1, d.month(), d.day(), Some(d.year())));
        }
        if let Some((d, rest)) = k.split_once('/') {
            let (m, y) = match rest.split_once('/') {
                Some((m, y)) => (m, Some(y)),
                None => (rest, None),
            };
            let (d, m) = (number(d).filter(|_| d.len() <= 2)?, number(m).filter(|_| m.len() <= 2)?);
            let y = match y {
                Some(y) if y.len() == 4 => Some(number(y)? as i32),
                Some(y) if y.len() == 2 => Some(2000 + number(y)? as i32),
                Some(_) => return None,
                None => None,
            };
            return Some((1, m, d, y));
        }
        let year = |at: usize| number(self.k(at)).filter(|y| (1970..=2200).contains(y) && self.k(at).len() == 4).map(|y| y as i32);
        if let (Some(d), Some(m)) = (day_number(k), month(self.k(i + 1))) {
            let y = year(i + 2);
            return Some((2 + usize::from(y.is_some()), m, d, y));
        }
        if let (Some(m), Some(d)) = (month(k), day_number(self.k(i + 1))) {
            let y = year(i + 2);
            return Some((2 + usize::from(y.is_some()), m, d, y));
        }
        None
    }

    /// A day and month without a year: the next one from today, or for `G`
    /// the nearest one.
    fn resolve(&self, m: u32, d: u32, y: Option<i32>) -> Option<NaiveDate> {
        if let Some(y) = y {
            return NaiveDate::from_ymd_opt(y, m, d);
        }
        let this = self.today.year();
        if self.nearest {
            return [this - 1, this, this + 1]
                .into_iter()
                .filter_map(|y| NaiveDate::from_ymd_opt(y, m, d))
                .min_by_key(|date| (*date - self.today).num_days().abs());
        }
        let date = NaiveDate::from_ymd_opt(this, m, d).or_else(|| NaiveDate::from_ymd_opt(this + 1, m, d))?;
        if date < self.today { NaiveDate::from_ymd_opt(this + 1, m, d) } else { Some(date) }
    }

    fn date_at(&self, i: usize) -> Option<(usize, NaiveDate)> {
        let (len, m, d, y) = self.day_month_at(i)?;
        Some((len, self.resolve(m, d, y)?))
    }

    /// A date, a weekday or a word for a day, with a word in front that goes
    /// with it ("on", "op", "le").
    fn day_at(&self, i: usize) -> Option<Hit> {
        if self.one_of(i, self.w.on) {
            if let Some(hit) = self.bare_day_at(i + 1) {
                return Hit::new(hit.len + 1, hit.effect);
            }
        }
        self.bare_day_at(i)
    }

    fn bare_day_at(&self, i: usize) -> Option<Hit> {
        if let Some((len, d)) = self.date_at(i) {
            return Hit::new(len, Effect::Day(Day::On(d)));
        }
        if let Some((words, ahead)) = self.w.days.iter().filter(|(w, _)| self.seq(i, w)).max_by_key(|(w, _)| w.len()) {
            return Hit::new(words.len(), Effect::Day(Day::On(self.today + Duration::days(*ahead))));
        }
        let monday = self.today - Duration::days(i64::from(self.today.weekday().days_since(self.week_start)));
        let in_week = |wd: Weekday, weeks: i64| Day::On(monday + Duration::days(7 * weeks + i64::from(wd.days_since(self.week_start))));
        if self.one_of(i, self.w.next) {
            if let Some(wd) = self.weekday(i + 1, false) {
                return Hit::new(2, Effect::Day(in_week(wd, 1)));
            }
        }
        if self.one_of(i, self.w.this) {
            if let Some(wd) = self.weekday(i + 1, false) {
                return Hit::new(2, Effect::Day(in_week(wd, 0)));
            }
        }
        if let Some(wd) = self.weekday(i, false) {
            if self.one_of(i + 1, self.w.next_after) {
                return Hit::new(2, Effect::Day(in_week(wd, 1)));
            }
            return Hit::new(1, Effect::Day(Day::Next(DayRule::Weekday(wd))));
        }
        if let Some(n) = self.w.short_weekdays.iter().position(|s| !s.is_empty() && *s == self.k(i)) {
            let next_to_time = self.clock_at(i + 1).is_some()
                || self.date_at(i + 1).is_some()
                || (i > 0 && self.toks.get(i - 1).is_some_and(|t| time(&t.key).is_some() || day_number(&t.key).is_some()));
            if next_to_time {
                return Hit::new(1, Effect::Day(Day::Next(DayRule::Weekday(WEEKDAYS[n]))));
            }
        }
        None
    }

    /// "dec 22-23", "22 to 26 dec", "from 24 dec to 2 jan", "van 24 dec tot
    /// 2 jan", "du 24 déc au 2 janv": all-day, the last day included.
    fn day_range_at(&self, i: usize) -> Option<Hit> {
        let open = usize::from(self.one_of(i, self.w.from));
        let at = i + open;
        // "dec 22-23" and "22-23 dec"
        let split = |k: &str| -> Option<(u32, u32)> {
            let (a, b) = k.split_once('-')?;
            Some((day_number(a)?, day_number(b)?))
        };
        if let (Some(m), Some((a, b))) = (month(self.k(at)), split(self.k(at + 1))) {
            return self.range_hit(open + 2, (m, a, None), (m, b, None));
        }
        if let (Some((a, b)), Some(m)) = (split(self.k(at)), month(self.k(at + 1))) {
            return self.range_hit(open + 2, (m, a, None), (m, b, None));
        }
        let (first_len, first) = match self.day_month_at(at) {
            Some((len, m, d, y)) => (len, (m, d, y)),
            None => (1, (0, day_number(self.k(at))?, None)),
        };
        let conn = self.any_seq(at + first_len, self.w.to)?;
        let (last_len, m, d, y) = self.day_month_at(at + first_len + conn)?;
        let first = if first.0 == 0 {
            let m = if first.1 > d { if m == 1 { 12 } else { m - 1 } } else { m };
            (m, first.1, None)
        } else {
            first
        };
        self.range_hit(open + first_len + conn + last_len, first, (m, d, y))
    }

    fn range_hit(&self, len: usize, first: (u32, u32, Option<i32>), last: (u32, u32, Option<i32>)) -> Option<Hit> {
        let start = match (first.2, last.2) {
            (None, Some(y)) => {
                let s = NaiveDate::from_ymd_opt(y, first.0, first.1)?;
                if (s.month(), s.day()) > (last.0, last.1) { NaiveDate::from_ymd_opt(y - 1, first.0, first.1)? } else { s }
            }
            _ => self.resolve(first.0, first.1, first.2)?,
        };
        let end = match last.2 {
            Some(y) => NaiveDate::from_ymd_opt(y, last.0, last.1)?,
            None => {
                let e = NaiveDate::from_ymd_opt(start.year(), last.0, last.1)?;
                if e < start { NaiveDate::from_ymd_opt(start.year() + 1, last.0, last.1)? } else { e }
            }
        };
        (end >= start && (end - start).num_days() < 366).then_some(())?;
        Hit::new(len, Effect::Days(start, end))
    }

    /// For `G`: a month alone ("december", the first of the next one) or a
    /// day number alone ("14", this month's).
    fn lone_day(&self, i: usize) -> Option<NaiveDate> {
        let k = self.k(i);
        if let Some(m) = month(k).filter(|_| k.len() > 2) {
            let y = if m < self.today.month() { self.today.year() + 1 } else { self.today.year() };
            return NaiveDate::from_ymd_opt(y, m, 1);
        }
        let d = day_number(k)?;
        NaiveDate::from_ymd_opt(self.today.year(), self.today.month(), d)
    }

    // -- Times

    /// A clock time at `i`: its length in words, the time, whether it had a
    /// mark (":", "h", "u", am, pm, a word), and what it means as a length.
    fn clock_at(&self, i: usize) -> Option<(usize, NaiveTime, bool, Option<Duration>)> {
        let k = self.k(i);
        if k.is_empty() {
            return None;
        }
        if k.chars().all(char::is_alphabetic) {
            if let Some(t) = time(k) {
                return Some((1, t, true, None));
            }
        }
        if self.lang == Lang::Nl {
            let hour = |at: usize| self.amount(at).filter(|h| (1..=12).contains(h));
            let shift = |h: u32, m: u32| time(&format!("{h}:{m:02}")).map(|t| if (1..=7).contains(&h) { t + Duration::hours(12) } else { t });
            if k == "half" {
                if let Some(h) = hour(i + 1) {
                    return shift(if h == 1 { 12 } else { h - 1 }, 30).map(|t| (2, t, true, None));
                }
            }
            if k == "kwart" && self.one_of(i + 1, &["over", "voor"]) {
                if let Some(h) = hour(i + 2) {
                    let (h, m) = if self.k(i + 1) == "over" { (h, 15) } else { (if h == 1 { 12 } else { h - 1 }, 45) };
                    return shift(h, m).map(|t| (3, t, true, None));
                }
            }
        }
        if !k.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            return None;
        }
        if number(k).is_some() {
            if self.one_of(i + 1, &["am", "pm", "a.m.", "p.m."]) {
                return time(&format!("{k}{}", self.k(i + 1))).map(|t| (2, t, true, None));
            }
            if self.lang == Lang::Nl && self.k(i + 1) == "uur" {
                let h = number(k)?;
                return time(&format!("{k}u")).map(|t| (2, t, true, Some(Duration::hours(h.into()))));
            }
            if self.lang == Lang::Fr && self.one_of(i + 1, &["h", "heures", "heure"]) && k.len() <= 2 {
                return time(&format!("{k}h")).map(|t| (2, t, true, None));
            }
        }
        let t = time(k)?;
        let marked = k.contains([':', '.', 'h', 'u']) || k.ends_with('m') || k.ends_with("m.");
        let lasts = if k.contains(['h', 'u']) && !k.contains([':', '.']) { length_word(k) } else { None };
        if self.lang == Lang::En && k.contains('h') {
            return None;
        }
        Some((1, t, marked, lasts))
    }

    fn time_at(&self, i: usize) -> Option<Hit> {
        if self.one_of(i, self.w.at) {
            if let Some((len, at, _, lasts)) = self.clock_at(i + 1) {
                return Hit::new(len + 1, Effect::Time { at, lasts });
            }
        }
        let (len, at, marked, lasts) = self.clock_at(i)?;
        (marked || self.after_day == Some(i)).then_some(Hit { len, effect: Effect::Time { at, lasts } })
    }

    /// "3-4pm", "15:00-16:30", "10u-12u30", "from 3 to 5", "van 15 tot 16
    /// uur", "de 15h à 16h".
    fn time_range_at(&self, i: usize) -> Option<Hit> {
        let k = self.k(i);
        if let Some((a, b)) = k.split_once('-') {
            if !a.is_empty() && !b.is_empty() && a.bytes().next().is_some_and(|c| c.is_ascii_digit()) {
                let (a, b, marked) = pair(a, b)?;
                if marked {
                    return Hit::new(1, Effect::Times(a, b));
                }
            }
        }
        let open = usize::from(self.one_of(i, self.w.from));
        let at = i + open;
        let (a_len, a, a_marked, _) = self.clock_word_at(at)?;
        let conn = self.any_seq(at + a_len, self.w.to)?;
        let (b_len, b, b_marked, _) = self.clock_word_at(at + a_len + conn)?;
        let uur = usize::from(self.lang == Lang::Nl && self.k(at + a_len + conn + b_len) == "uur");
        if open == 0 && !(a_marked || b_marked || uur == 1) {
            return None;
        }
        let (a, b, _) = pair(&a, &b)?;
        Hit::new(open + a_len + conn + b_len + uur, Effect::Times(a, b))
    }

    /// One word of a time range, kept as typed so "3" can take the pm of "4pm".
    fn clock_word_at(&self, i: usize) -> Option<(usize, String, bool, ())> {
        let (len, _, marked, _) = self.clock_at(i).or_else(|| {
            let k = self.k(i);
            (self.lang == Lang::En && k.contains('h') && time(k).is_some()).then_some((1, NaiveTime::MIN, true, None))
        })?;
        let text = match (len, self.k(i + 1)) {
            (2, "uur") => format!("{}u", self.k(i)),
            (2, "heure" | "heures") => format!("{}h", self.k(i)),
            _ => (0..len).map(|n| self.k(i + n)).collect::<String>(),
        };
        // "16:00 until 18 dec" ends a repeat, not the meeting.
        if month(self.k(i + len)).is_some() {
            return None;
        }
        Some((len, text, marked, ()))
    }

    /// "for 45 min", "45m", "2 hours", "anderhalf uur", "pendant 1h30".
    fn length_at(&self, i: usize) -> Option<Hit> {
        let lead = usize::from(self.one_of(i, self.w.lasting));
        let at = i + lead;
        let k = self.k(at);
        let phrases: &[(&[&str], i64)] = match self.lang {
            Lang::En => &[(&["half", "an", "hour"], 30), (&["an", "hour"], 60), (&["one", "hour"], 60)],
            Lang::Nl => &[(&["anderhalf", "uur"], 90), (&["anderhalve", "uur"], 90), (&["een", "half", "uur"], 30), (&["een", "uur"], 60), (&["half", "uur"], 30)],
            Lang::Fr => &[(&["une", "demi-heure"], 30), (&["demi-heure"], 30), (&["une", "heure"], 60), (&["une", "heure", "et", "demie"], 90)],
        };
        if let Some((words, minutes)) = phrases.iter().filter(|(w, _)| self.seq(at, w)).max_by_key(|(w, _)| w.len()) {
            return Hit::new(lead + words.len(), Effect::Lasts(Duration::minutes(*minutes)));
        }
        let unit = |w: &str| self.w.lengths.iter().find(|(names, _)| names.contains(&w)).map(|(_, per)| *per);
        if let Some(n) = self.amount(at).filter(|n| *n > 0) {
            if let Some(per) = unit(self.k(at + 1)) {
                return Hit::new(lead + 2, Effect::Lasts(Duration::minutes(i64::from(n) * per)));
            }
        }
        if let Some((n, rest)) = k.split_once([',', '.']).filter(|(n, r)| number(n).is_some() && r.len() == 1 && number(r).is_some()) {
            if unit(self.k(at + 1)) == Some(HOUR) {
                let minutes = i64::from(number(n)?) * 60 + i64::from(number(rest)?) * 6;
                return Hit::new(lead + 2, Effect::Lasts(Duration::minutes(minutes)));
            }
        }
        if lead == 1 || self.lang == Lang::En || k.ends_with('m') || k.ends_with("min") {
            if let Some(d) = length_word(k) {
                return Hit::new(lead + 1, Effect::Lasts(d));
            }
        }
        None
    }

    /// "in 2 hours", "over 2 uur", "dans 2 heures"; "in 3 days" is a day.
    fn after_at(&self, i: usize) -> Option<Hit> {
        if !self.one_of(i, self.w.after) {
            return None;
        }
        let n = i64::from(self.amount(i + 1).filter(|n| *n > 0)?);
        let unit = self.k(i + 2);
        let days: &[&str] = &["day", "days", "dag", "dagen", "jour", "jours"];
        let weeks: &[&str] = &["week", "weeks", "weken", "semaine", "semaines"];
        if days.contains(&unit) {
            return Hit::new(3, Effect::Day(Day::On(self.today + Duration::days(n))));
        }
        if weeks.contains(&unit) {
            return Hit::new(3, Effect::Day(Day::On(self.today + Duration::days(7 * n))));
        }
        let per = self.w.lengths.iter().find(|(names, _)| names.contains(&unit)).map(|(_, per)| *per)?;
        if self.nearest {
            return None;
        }
        Hit::new(3, Effect::After(Duration::minutes(n * per)))
    }

    // -- Zones

    /// "Europe/Lisbon", "Lisbon time", "Lissabon tijd", "heure de Lisbonne".
    fn zone_at(&self, i: usize) -> Option<Hit> {
        let tok = self.toks.get(i).filter(|_| !self.used[i])?;
        if tok.text.contains('/') && tok.text.chars().next().is_some_and(char::is_alphabetic) {
            if let Some(z) = tz::zone(&tok.text) {
                return Hit::new(1, Effect::Zone(z));
            }
        }
        for n in 1..=3 {
            if self.one_of(i + n, self.w.zone_after) {
                if let Some(z) = self.city(i, n) {
                    return Hit::new(n + 1, Effect::Zone(z));
                }
            }
        }
        for before in self.w.zone_before {
            if self.seq(i, before) {
                for n in (1..=3).rev() {
                    if let Some(z) = self.city(i + before.len(), n) {
                        return Hit::new(before.len() + n, Effect::Zone(z));
                    }
                }
            }
            // "heure d'Athènes" folds to one word after "heure".
            if before.len() == 2 && before[1] == "d'" && self.k(i) == before[0] {
                if let Some(rest) = self.k(i + 1).strip_prefix("d'") {
                    if let Some(z) = city_zone(rest) {
                        return Hit::new(2, Effect::Zone(z));
                    }
                }
            }
        }
        None
    }

    fn city(&self, i: usize, n: usize) -> Option<Tz> {
        let name = (0..n).map(|m| self.k(i + m)).collect::<Vec<_>>();
        if name.iter().any(|w| w.is_empty()) {
            return None;
        }
        city_zone(&name.join(" "))
    }

    // -- People and places

    /// "with Jan", "with Jan and Els", "met Jan", "avec Jan", and addresses.
    fn people_at(&self, i: usize) -> Option<Hit> {
        if is_address(self.k(i)) {
            let tok = &self.toks[i];
            return Hit::new(1, Effect::People { names: vec![tok.text.clone()], keep: Vec::new(), parts: vec![tok.range.clone()] });
        }
        if !self.one_of(i, self.w.with) {
            return None;
        }
        let mut j = i + 1;
        let mut items: Vec<(Range<usize>, bool)> = Vec::new();
        loop {
            let k = self.k(j);
            if k.is_empty() || self.starts_part(j) {
                break;
            }
            if is_address(k) {
                items.push((j..j + 1, true));
                j += 1;
            } else if self.is_name(j, true) {
                let second = self.is_name(j + 1, false) && self.toks[j + 1].capital && !self.starts_part(j + 1);
                let end = j + 1 + usize::from(second);
                items.push((j..end, false));
                j = end;
            } else {
                break;
            }
            if self.one_of(j, self.w.and) && (is_address(self.k(j + 1)) || self.is_name(j + 1, true)) && !self.starts_part(j + 1) {
                j += 1;
            } else {
                break;
            }
        }
        if items.is_empty() {
            return None;
        }
        let names = items.iter().map(|(r, _)| self.toks[r.clone()].iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ")).collect();
        let text_range = |r: &Range<usize>| self.toks[r.start].range.start..self.toks[r.end - 1].range.end;
        let len = j - i;
        if items.iter().all(|(_, address)| *address) {
            let whole = self.toks[i].range.start..self.toks[j - 1].range.end;
            return Hit::new(len, Effect::People { names, keep: Vec::new(), parts: vec![whole] });
        }
        // Names stay in the title with their "with" and "and"; an address
        // leaves with the "and" in front of it.
        let mut keep: Vec<usize> = (i..j).collect();
        let mut parts = Vec::new();
        for (r, address) in &items {
            if *address {
                let from = if r.start > i + 1 && self.one_of(r.start - 1, self.w.and) { r.start - 1 } else { r.start };
                keep.retain(|t| !(from..r.end).contains(t));
                parts.push(self.toks[from].range.start..self.toks[r.end - 1].range.end);
            } else {
                parts.push(text_range(r));
            }
        }
        Hit::new(len, Effect::People { names, keep, parts })
    }

    fn is_name(&self, i: usize, first: bool) -> bool {
        let k = self.k(i);
        !k.is_empty()
            && k.chars().next().is_some_and(char::is_alphabetic)
            && !(first && self.w.not_names.contains(&k))
            && !self.w.and.contains(&k)
            && !self.w.with.contains(&k)
            && !self.w.place.contains(&k)
    }

    /// "at Mok", "@ Mok", "bij Mok", "in De Kroon", "chez Mok", "à La
    /// Couronne": the place runs to the next field or the end.
    fn place_at(&self, i: usize) -> Option<Hit> {
        let k = self.k(i);
        let (lead, first) = match k.strip_prefix('@') {
            Some(rest) if !rest.is_empty() && !rest.contains('@') => (0, i),
            _ if self.one_of(i, self.w.place) => (1, i + 1),
            _ => return None,
        };
        let mut j = first;
        while !self.k(j).is_empty() && !self.starts_part(j) && !self.one_of(j, self.w.with) && !is_address(self.k(j)) && (j == first || !self.one_of(j, self.w.place)) {
            j += 1;
        }
        if j == first {
            return None;
        }
        let mut place = self.toks[first..j].iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ");
        if lead == 0 {
            place.remove(0);
        }
        Hit::new(j - i, Effect::Place(place))
    }

    // -- The result

    fn finish(self, text: &str, now: DateTime<Tz>, prefs: &Prefs) -> Parsed {
        let here = now.timezone();
        let zone = self.zone.unwrap_or(here);
        let utc_now = now.with_timezone(&Utc);
        let passed = |d: NaiveDate, t: NaiveTime| tz::instant(d.and_time(t), zone) < utc_now;
        let length = self.lasts.unwrap_or(Duration::minutes(prefs.default_minutes));
        let day_rule = match self.day {
            Some(Day::Next(rule)) => Some(rule),
            _ => self.rule_day,
        };
        let day = match self.day {
            Some(Day::On(d)) => Some(d),
            _ => day_rule.map(|rule| {
                let today_ok = rule.matches(self.today) && self.start.is_none_or(|t| !passed(self.today, t));
                rule.next(self.today, today_ok)
            }),
        };
        let at = |wall| Local { wall, zone: Zone::Tz(zone) };
        let when = if let Some(after) = self.after {
            let wall = (utc_now + after).with_timezone(&zone).naive_local();
            let wall = wall.with_second(0).and_then(|w| w.with_nanosecond(0)).unwrap_or(wall);
            Some(Span::Timed { start: at(wall), end: at(wall + length) })
        } else {
            match (day, self.start) {
                (None, None) if !self.all_day => None,
                (d, None) => {
                    let start = d.unwrap_or(self.today);
                    Some(Span::AllDay { start, end: self.last_day.unwrap_or(start) + Duration::days(1) })
                }
                (d, Some(t)) => {
                    let date = d.unwrap_or(if passed(self.today, t) { self.today + Duration::days(1) } else { self.today });
                    let start = date.and_time(t);
                    let end = match self.end {
                        Some(e) => {
                            let e = date.and_time(e);
                            if e <= start { e + Duration::days(1) } else { e }
                        }
                        None => start + length,
                    };
                    Some(Span::Timed { start: at(start), end: at(end) })
                }
            }
        };
        // Without a day or time the editor starts at the next half hour, so
        // the rule's UNTIL is placed from there.
        let next = || {
            let start = super::draft::next_half_hour(utc_now, here);
            let local = |wall| Local { wall, zone: Zone::Tz(here) };
            Span::Timed { start: local(start), end: local(start + length) }
        };
        let repeat = self.rule.as_ref().map(|rule| {
            let span = when.unwrap_or_else(next);
            let mut rule = rule.clone();
            if let (Freq::Weekly, By::Nothing, Some(DayRule::Weekday(wd))) = (rule.freq, &rule.by, day_rule) {
                rule.by = By::Days(vec![wd]);
            }
            let first = match span {
                Span::AllDay { start, .. } => start,
                Span::Timed { start, .. } => start.wall.date(),
            };
            let last = self.until.or(self.for_weeks.map(|n| first + Duration::days(7 * i64::from(n) - 1)));
            rrule(&rule, last.map(|d| repeat::until(d, &span)), self.count)
        });
        let mut title = Vec::new();
        let mut leftover = Vec::new();
        for (n, tok) in self.toks.iter().enumerate() {
            if self.used[n] {
                continue;
            }
            title.push(tok.text.as_str());
            if !self.kept[n] && datey(&tok.key, self.w) {
                leftover.push(tok.text.clone());
            }
        }
        let mut parts: Vec<(Range<usize>, Part)> = self
            .hits
            .iter()
            .map(|(r, part)| match part {
                Part::Person => (r.clone(), *part),
                _ => (self.toks[r.start].range.start..self.toks[r.end - 1].range.end, *part),
            })
            .collect();
        parts.sort_by_key(|(r, _)| r.start);
        debug_assert!(parts.iter().all(|(r, _)| r.end <= text.len()));
        Parsed {
            title: title.join(" "),
            when,
            people: self.people,
            location: self.place,
            repeat,
            parts,
            leftover,
            language: self.lang,
        }
    }
}

/// Two ends of a time range as typed: "3" takes the pm of "4pm". Whether
/// either had a mark.
fn pair(a: &str, b: &str) -> Option<(NaiveTime, NaiveTime, bool)> {
    let suffix = ["am", "pm", "a.m.", "p.m."].into_iter().find(|s| b.ends_with(s));
    let a_bare = number(a).is_some();
    let marked = !a_bare || number(b).is_none();
    let end = time(b)?;
    let start = match suffix {
        Some(s) if a_bare => {
            let other = if s.starts_with('a') { "pm" } else { "am" };
            time(&format!("{a}{s}")).filter(|t| *t < end).or_else(|| time(&format!("{a}{other}"))).or_else(|| time(a))?
        }
        _ => time(a)?,
    };
    (start != end).then_some((start, end, marked))
}

/// "45m", "90min", "1h", "1h30", "2u", "1u30" as a length.
fn length_word(k: &str) -> Option<Duration> {
    for (suffix, per) in [("min", 1), ("mn", 1), ("m", 1)] {
        if let Some(n) = k.strip_suffix(suffix).and_then(number) {
            return (n > 0 && n <= 24 * 60).then(|| Duration::minutes(i64::from(n) * per));
        }
    }
    let (h, m) = k.split_once(['h', 'u'])?;
    let h = number(h).filter(|h| *h <= 24)?;
    let m = if m.is_empty() { 0 } else { number(m).filter(|m| *m < 60 && k.len() - k.find(['h', 'u']).unwrap_or(0) == 3)? };
    (h + m > 0).then(|| Duration::minutes(i64::from(h) * 60 + i64::from(m)))
}

fn is_address(k: &str) -> bool {
    let Some((local, domain)) = k.split_once('@') else { return false };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !domain.contains('@')
}

fn city_zone(name: &str) -> Option<Tz> {
    if let Some((_, z)) = CITIES.iter().find(|(c, _)| *c == name) {
        return Some(*z);
    }
    chrono_tz::TZ_VARIANTS.iter().copied().find(|z| {
        let city = z.name().rsplit('/').next().unwrap_or_default();
        z.name().contains('/') && fold(&city.replace('_', " ")) == name
    })
}

/// A word that names a day or time the reading didn't place.
fn datey(key: &str, w: &Words) -> bool {
    if w.datey.contains(&key) || MONTHS.iter().any(|m| m[0] == key) || w.weekdays.iter().any(|names| names.contains(&key)) {
        return true;
    }
    if !key.bytes().any(|b| b.is_ascii_digit()) || number(key).is_some() {
        return false;
    }
    // "25:00" and "31/02", not "1:1".
    let broken_clock = key.split_once(':').is_some_and(|(h, m)| number(h).is_some() && m.len() == 2 && number(m).is_some());
    broken_clock || key.contains('/') || ["am", "pm", "h", "u"].iter().any(|s| key.strip_suffix(s).is_some_and(|n| number(n).is_some()))
}

fn rrule(rule: &Rule, until: Option<String>, count: Option<u32>) -> String {
    let freq = match rule.freq {
        Freq::Daily => "DAILY",
        Freq::Weekly => "WEEKLY",
        Freq::Monthly => "MONTHLY",
        Freq::Yearly => "YEARLY",
    };
    let mut out = format!("FREQ={freq}");
    if rule.interval > 1 {
        out.push_str(&format!(";INTERVAL={}", rule.interval));
    }
    let code = |wd: &Weekday| ["MO", "TU", "WE", "TH", "FR", "SA", "SU"][wd.num_days_from_monday() as usize];
    match &rule.by {
        By::Nothing => {}
        By::Days(days) => out.push_str(&format!(";BYDAY={}", days.iter().map(code).collect::<Vec<_>>().join(","))),
        By::MonthDay(d) => out.push_str(&format!(";BYMONTHDAY={d}")),
        By::Last(wd) => out.push_str(&format!(";BYDAY=-1{}", code(wd))),
    }
    if let Some(u) = until {
        out.push_str(&format!(";UNTIL={u}"));
    } else if let Some(n) = count {
        out.push_str(&format!(";COUNT={n}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;

    fn at(h: u32, m: u32) -> Option<NaiveTime> {
        NaiveTime::from_hms_opt(h, m, 0)
    }

    #[test]
    fn reads_the_ways_people_type_a_time() {
        for (text, want) in [
            ("9", at(9, 0)),
            ("930", at(9, 30)),
            ("0930", at(9, 30)),
            ("14:30", at(14, 30)),
            ("1430", at(14, 30)),
            ("9.30", at(9, 30)),
            ("2pm", at(14, 0)),
            ("2 PM", at(14, 0)),
            ("12pm", at(12, 0)),
            ("12am", at(0, 0)),
            ("9:30am", at(9, 30)),
            ("9u30", at(9, 30)),
            ("9u", at(9, 0)),
            ("9h30", at(9, 30)),
            ("21h", at(21, 0)),
            ("noon", at(12, 0)),
            ("midi", at(12, 0)),
            ("midnight", at(0, 0)),
            ("23:59", at(23, 59)),
            (" 10 ", at(10, 0)),
        ] {
            assert_eq!(time(text), want, "{text:?}");
        }
    }

    /// Decision 7 in the C3 spec: "lunch at 1" is 13:00.
    #[test]
    fn a_bare_hour_up_to_seven_is_the_afternoon() {
        for (text, want) in [("1", at(13, 0)), ("4", at(16, 0)), ("7", at(19, 0)), ("130", at(13, 30)), ("8", at(8, 0)), ("12", at(12, 0))] {
            assert_eq!(time(text), want, "{text:?}");
        }
        for (text, want) in [("7u", at(7, 0)), ("7h", at(7, 0)), ("7:00", at(7, 0)), ("07", at(7, 0)), ("0130", at(1, 30)), ("7am", at(7, 0))] {
            assert_eq!(time(text), want, "{text:?} stays in the morning");
        }
    }

    #[test]
    fn refuses_what_is_not_a_time() {
        for text in ["", "abc", "24:00", "9:60", "13pm", "0am", "12345", "9:5", "tomorrow", "9-10", ":30", "９", "1😀"] {
            assert_eq!(time(text), None, "{text:?}");
        }
    }

    /// Reference A: Saturday 7 November 2026, 15:00 in Brussels.
    fn ref_a() -> DateTime<Tz> {
        Brussels.with_ymd_and_hms(2026, 11, 7, 15, 0, 0).unwrap()
    }

    /// Reference B: Wednesday 11 November 2026, 10:00.
    fn ref_b() -> DateTime<Tz> {
        Brussels.with_ymd_and_hms(2026, 11, 11, 10, 0, 0).unwrap()
    }

    fn prefs() -> Prefs {
        Prefs::new(30, &[Lang::En])
    }

    /// The draft in the spec table's words: "Fri 13 Nov 12:30–13:00",
    /// "All day Tue 22 Dec (end 24 Dec)", a zone after the time when it
    /// isn't Brussels.
    fn when(p: &Parsed) -> String {
        match p.when {
            None => "none".into(),
            Some(Span::Timed { start, end }) => {
                let zone = match start.zone {
                    Zone::Tz(z) if z != Brussels => format!(" {}", z.name()),
                    _ => String::new(),
                };
                let day = if start.wall.year() == 2026 { start.wall.format("%a %-d %b") } else { start.wall.format("%a %-d %b %Y") };
                format!("{day} {}–{}{zone}", start.wall.format("%H:%M"), end.format_end(start.wall.date()))
            }
            Some(Span::AllDay { start, end }) => {
                let day = |d: NaiveDate| if d.year() == 2026 { d.format("%a %-d %b").to_string() } else { d.format("%a %-d %b %Y").to_string() };
                format!("All day {} (end {})", day(start), day(end))
            }
        }
    }

    trait End {
        fn format_end(&self, day: NaiveDate) -> String;
    }

    impl End for Local {
        fn format_end(&self, day: NaiveDate) -> String {
            if self.wall.date() == day { self.wall.format("%H:%M").to_string() } else { self.wall.format("%a %-d %b %H:%M").to_string() }
        }
    }

    struct Row {
        id: &'static str,
        text: &'static str,
        title: &'static str,
        when: &'static str,
        people: &'static [&'static str],
        place: Option<&'static str>,
        repeat: Option<&'static str>,
    }

    const fn row(id: &'static str, text: &'static str, title: &'static str, when: &'static str) -> Row {
        Row { id, text, title, when, people: &[], place: None, repeat: None }
    }

    impl Row {
        const fn people(mut self, people: &'static [&'static str]) -> Row {
            self.people = people;
            self
        }
        const fn place(mut self, place: &'static str) -> Row {
            self.place = Some(place);
            self
        }
        const fn repeat(mut self, rule: &'static str) -> Row {
            self.repeat = Some(rule);
            self
        }
    }

    fn check(rows: &[Row], now: DateTime<Tz>) {
        let mut failures = Vec::new();
        for r in rows {
            let p = parse(r.text, now, &prefs());
            let got = (p.title.as_str(), when(&p), p.people.clone(), p.location.as_deref(), p.repeat.as_deref());
            let want = (r.title, r.when.to_string(), r.people.iter().map(|s| s.to_string()).collect::<Vec<_>>(), r.place, r.repeat);
            if got != want {
                failures.push(format!("{} {:?}\n   got {got:?}\n  want {want:?}", r.id, r.text));
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    /// The spec's English table, reference A.
    #[test]
    fn english() {
        check(
            &[
                row("E1", "Lunch with Jan fri 12:30 at Mok", "Lunch with Jan", "Fri 13 Nov 12:30–13:00").people(&["Jan"]).place("Mok"),
                row("E2", "Standup tomorrow 9:30 15m", "Standup", "Sun 8 Nov 09:30–09:45"),
                row("E3", "Call with Els at 4", "Call with Els", "Sat 7 Nov 16:00–16:30").people(&["Els"]),
                row("E4", "Dentist 14 dec 8am", "Dentist", "Mon 14 Dec 08:00–08:30"),
                row("E5", "Review 3-4pm wed", "Review", "Wed 11 Nov 15:00–16:00"),
                row("E6", "Workshop 10:00-12:30 on 2026-11-24", "Workshop", "Tue 24 Nov 10:00–12:30"),
                row("E7", "Offsite dec 22-23", "Offsite", "All day Tue 22 Dec (end Thu 24 Dec)"),
                row("E8", "Holiday from 24 dec to 2 jan", "Holiday", "All day Thu 24 Dec (end Sun 3 Jan 2027)"),
                row("E9", "Focus block monday 9 for 2 hours", "Focus block", "Mon 9 Nov 09:00–11:00"),
                row("E10", "1:1 with sofie@example.com next tue 11", "1:1", "Tue 10 Nov 11:00–11:30").people(&["sofie@example.com"]),
                row("E11", "Team sync every monday 10am", "Team sync", "Mon 9 Nov 10:00–10:30").repeat("FREQ=WEEKLY;BYDAY=MO"),
                row("E12", "Payroll check monthly on the 25th", "Payroll check", "All day Wed 25 Nov (end Thu 26 Nov)").repeat("FREQ=MONTHLY;BYMONTHDAY=25"),
                row("E13", "Retro every other friday 16:00 until 18 dec", "Retro", "Fri 13 Nov 16:00–16:30")
                    .repeat("FREQ=WEEKLY;INTERVAL=2;BYDAY=FR;UNTIL=20261218T225959Z"),
                row("E14", "Gym every weekday 7am for 10 weeks", "Gym", "Mon 9 Nov 07:00–07:30")
                    .repeat("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;UNTIL=20270117T225959Z"),
                row("E15", "Board meeting last friday of the month 14:00", "Board meeting", "Fri 27 Nov 14:00–14:30").repeat("FREQ=MONTHLY;BYDAY=-1FR"),
                row("E16", "Birthday Els 3 march every year", "Birthday Els", "All day Wed 3 Mar 2027 (end Thu 4 Mar 2027)").repeat("FREQ=YEARLY"),
                row("E17", "Flight sat 7:15 Lisbon time", "Flight", "Sat 14 Nov 07:15–07:45 Europe/Lisbon"),
                row("E18", "Coffee in 2 hours", "Coffee", "Sat 7 Nov 17:00–17:30"),
                row("E19", "Call noon", "Call", "Sun 8 Nov 12:00–12:30"),
                row("E20", "Townhall tomorrow all day", "Townhall", "All day Sun 8 Nov (end Mon 9 Nov)"),
                row("E21", "Lunch @ Mok with Jan and Els", "Lunch with Jan and Els", "none").people(&["Jan", "Els"]).place("Mok"),
                row("E22", "Meeting 5/12", "Meeting", "All day Sat 5 Dec (end Sun 6 Dec)"),
                row("E23", "Sprint planning 9-10:30 thursday", "Sprint planning", "Thu 12 Nov 09:00–10:30"),
                row("E24", "Read chapter 12", "Read chapter 12", "none"),
                row("E25", "Dinner mar 3", "Dinner", "All day Wed 3 Mar 2027 (end Thu 4 Mar 2027)"),
            ],
            ref_a(),
        );
    }

    /// The spec's weekday table, reference B: "fri" is the coming Friday,
    /// "next fri" the Friday of next week (decision 6).
    #[test]
    fn weekdays() {
        check(
            &[
                row("W1", "Demo fri 14:00", "Demo", "Fri 13 Nov 14:00–14:30"),
                row("W2", "Demo next fri 14:00", "Demo", "Fri 20 Nov 14:00–14:30"),
                row("W3", "Demo this fri 14:00", "Demo", "Fri 13 Nov 14:00–14:30"),
                row("W4", "Demo wed 9:00", "Demo", "Wed 18 Nov 09:00–09:30"),
                row("W5", "Demo wed 11:00", "Demo", "Wed 11 Nov 11:00–11:30"),
                row("W6", "Demo vrijdag 14u", "Demo", "Fri 13 Nov 14:00–14:30"),
                row("W7", "Démo vendredi prochain 14h", "Démo", "Fri 20 Nov 14:00–14:30"),
            ],
            ref_b(),
        );
    }

    #[test]
    fn dutch() {
        check(
            &[
                row("N1", "Lunch met Jan vrijdag 12u30 bij Mok", "Lunch met Jan", "Fri 13 Nov 12:30–13:00").people(&["Jan"]).place("Mok"),
                row("N2", "Standup morgen 9.30 15 min", "Standup", "Sun 8 Nov 09:30–09:45"),
                row("N3", "Tandarts 14 dec om 8 uur", "Tandarts", "Mon 14 Dec 08:00–08:30"),
                row("N4", "Overleg woensdag van 15 tot 16 uur", "Overleg", "Wed 11 Nov 15:00–16:00"),
                row("N5", "Workshop 24/11 10u-12u30", "Workshop", "Tue 24 Nov 10:00–12:30"),
                row("N6", "Teamoverleg elke maandag om 10u", "Teamoverleg", "Mon 9 Nov 10:00–10:30").repeat("FREQ=WEEKLY;BYDAY=MO"),
                row("N7", "Retro om de twee weken vrijdag 16u tot 18 december", "Retro", "Fri 13 Nov 16:00–16:30")
                    .repeat("FREQ=WEEKLY;INTERVAL=2;BYDAY=FR;UNTIL=20261218T225959Z"),
                row("N8", "Verlof van 24 dec tot 2 jan", "Verlof", "All day Thu 24 Dec (end Sun 3 Jan 2027)"),
                row("N9", "Sporten elke werkdag 7u", "Sporten", "Mon 9 Nov 07:00–07:30").repeat("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"),
                row("N10", "Koffie over 2 uur", "Koffie", "Sat 7 Nov 17:00–17:30"),
                row("N11", "Bestuur laatste vrijdag van de maand 14u", "Bestuur", "Fri 27 Nov 14:00–14:30").repeat("FREQ=MONTHLY;BYDAY=-1FR"),
                row("N12", "Opleiding overmorgen hele dag", "Opleiding", "All day Mon 9 Nov (end Tue 10 Nov)"),
                row("N13", "Gesprek volgende dinsdag 11u anderhalf uur", "Gesprek", "Tue 10 Nov 11:00–12:30"),
                row("N14", "Etentje zaterdag 19u in De Kroon", "Etentje", "Sat 7 Nov 19:00–19:30").place("De Kroon"),
                row("N15", "Belafspraak half tien", "Belafspraak", "Sun 8 Nov 09:30–10:00"),
            ],
            ref_a(),
        );
    }

    #[test]
    fn french() {
        check(
            &[
                row("F1", "Déjeuner avec Jan vendredi 12h30 chez Mok", "Déjeuner avec Jan", "Fri 13 Nov 12:30–13:00").people(&["Jan"]).place("Mok"),
                row("F2", "Standup demain 9h30 15 min", "Standup", "Sun 8 Nov 09:30–09:45"),
                row("F3", "Dentiste le 14 déc à 8h", "Dentiste", "Mon 14 Dec 08:00–08:30"),
                row("F4", "Réunion mercredi de 15h à 16h", "Réunion", "Wed 11 Nov 15:00–16:00"),
                row("F5", "Atelier 24/11 10h-12h30", "Atelier", "Tue 24 Nov 10:00–12:30"),
                row("F6", "Point d'équipe tous les lundis à 10h", "Point d'équipe", "Mon 9 Nov 10:00–10:30").repeat("FREQ=WEEKLY;BYDAY=MO"),
                row("F7", "Rétro un vendredi sur deux à 16h jusqu'au 18 décembre", "Rétro", "Fri 13 Nov 16:00–16:30")
                    .repeat("FREQ=WEEKLY;INTERVAL=2;BYDAY=FR;UNTIL=20261218T225959Z"),
                row("F8", "Congé du 24 déc au 2 janv", "Congé", "All day Thu 24 Dec (end Sun 3 Jan 2027)"),
                row("F9", "Sport chaque jour ouvrable à 7h", "Sport", "Mon 9 Nov 07:00–07:30").repeat("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"),
                row("F10", "Café dans 2 heures", "Café", "Sat 7 Nov 17:00–17:30"),
                row("F11", "Conseil le dernier vendredi du mois à 14h", "Conseil", "Fri 27 Nov 14:00–14:30").repeat("FREQ=MONTHLY;BYDAY=-1FR"),
                row("F12", "Formation après-demain toute la journée", "Formation", "All day Mon 9 Nov (end Tue 10 Nov)"),
                row("F13", "Entretien mardi prochain 11h pendant 1h30", "Entretien", "Tue 10 Nov 11:00–12:30"),
                row("F14", "Dîner samedi 19h à La Couronne", "Dîner", "Sat 7 Nov 19:00–19:30").place("La Couronne"),
                row("F15", "Atelier lundi 10h 1h30", "Atelier", "Mon 9 Nov 10:00–11:30"),
            ],
            ref_a(),
        );
    }

    #[test]
    fn says_which_language_won() {
        let lang = |text| parse(text, ref_a(), &prefs()).language;
        assert_eq!(lang("Lunch met Jan vrijdag 12u30 bij Mok"), Lang::Nl);
        assert_eq!(lang("Déjeuner avec Jan vendredi 12h30 chez Mok"), Lang::Fr);
        assert_eq!(lang("Lunch with Jan fri 12:30 at Mok"), Lang::En);
        assert_eq!(parse("Meeting 5/12", ref_a(), &Prefs::new(30, &[Lang::Nl])).language, Lang::Nl, "a tie goes to the first language");
    }

    #[test]
    fn leaves_what_it_cant_place() {
        let p = parse("1:1 with sofie@example.com next tue 11", ref_a(), &prefs());
        assert!(p.leftover.is_empty(), "1:1 isn't a time: {:?}", p.leftover);
        assert!(parse("Read chapter 12", ref_a(), &prefs()).leftover.is_empty());
        let p = parse("drinks the friday after next around six", ref_a(), &prefs());
        assert_eq!(p.leftover, ["after", "next", "around", "six"]);
        let p = parse("Call tomorrow fri", ref_a(), &prefs());
        assert_eq!(p.leftover, ["fri"], "a second day isn't taken");
        assert_eq!(p.title, "Call fri");
    }

    #[test]
    fn dutch_short_weekdays_need_a_time_or_date() {
        assert_eq!(when(&parse("Overleg do 14u", ref_a(), &prefs())), "Thu 12 Nov 14:00–14:30");
        let p = parse("Wat moet ik do", ref_a(), &prefs());
        assert_eq!(p.title, "Wat moet ik do");
        assert_eq!(p.when, None);
    }

    #[test]
    fn people_names_stay_and_addresses_leave() {
        let p = parse("Lunch with Jan and sofie@example.com", ref_a(), &prefs());
        assert_eq!(p.title, "Lunch with Jan");
        assert_eq!(p.people, ["Jan", "sofie@example.com"]);
        let p = parse("Kickoff with Jan Peeters tomorrow 9am", ref_a(), &prefs());
        assert_eq!(p.people, ["Jan Peeters"]);
        assert_eq!(p.title, "Kickoff with Jan Peeters");
        let p = parse("Sync with the team tomorrow 10am", ref_a(), &prefs());
        assert!(p.people.is_empty(), "{:?}", p.people);
    }

    #[test]
    fn more_ways_to_say_it() {
        let w = |text| when(&parse(text, ref_a(), &prefs()));
        assert_eq!(w("Call from 3 to 5"), "Sat 7 Nov 15:00–17:00");
        assert_eq!(w("Call 11-1pm tomorrow"), "Sun 8 Nov 11:00–13:00");
        assert_eq!(w("Trip 22 to 26 dec"), "All day Tue 22 Dec (end Sun 27 Dec)");
        assert_eq!(w("Call tomorrow 930"), "Sun 8 Nov 09:30–10:00");
        assert_eq!(w("Call tomorrow at 9 for half an hour"), "Sun 8 Nov 09:00–09:30");
        assert_eq!(w("Planning 14 dec 2027 10am"), "Tue 14 Dec 2027 10:00–10:30");
        assert_eq!(w("Call 4 pm tomorrow"), "Sun 8 Nov 16:00–16:30");
        assert_eq!(w("Koffie morgen kwart over 9"), "Sun 8 Nov 09:15–09:45");
        assert_eq!(w("Call midnight"), "Sun 8 Nov 00:00–00:30");
        assert_eq!(w("Trip in 3 days"), "All day Tue 10 Nov (end Wed 11 Nov)");
        let p = parse("Standup daily 9:15 10 times", ref_a(), &prefs());
        assert_eq!(p.repeat.as_deref(), Some("FREQ=DAILY;COUNT=10"));
        assert_eq!(when(&p), "Sun 8 Nov 09:15–09:45");
        let p = parse("Review every 2 weeks", ref_a(), &prefs());
        assert_eq!(p.repeat.as_deref(), Some("FREQ=WEEKLY;INTERVAL=2"));
        let p = parse("Call New York time 9am tomorrow", ref_a(), &prefs());
        assert_eq!(when(&p), "Sun 8 Nov 09:00–09:30 America/New_York");
        let p = parse("Appel demain 9h heure de Lisbonne", ref_a(), &prefs());
        assert_eq!(when(&p), "Sun 8 Nov 09:00–09:30 Europe/Lisbon");
        let p = parse("Call tomorrow 9am Europe/Lisbon", ref_a(), &prefs());
        assert_eq!(when(&p), "Sun 8 Nov 09:00–09:30 Europe/Lisbon");
    }

    #[test]
    fn the_draft_takes_people_only_on_a_clear_match() {
        use crate::store::Store;
        let dir = std::env::temp_dir().join(format!("jolta-quick-draft-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        let calendar = s.write(|tx| super::super::ops::new_local_calendar(tx, "Test", None)).unwrap().id;
        s.write(|tx| {
            tx.execute_batch(
                "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'imap', 'me@home.test', 'me', 'a1', 0);
                 INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, date)
                 VALUES (1, 1, '1', 1, 'me@home.test', 'Jan Peeters <jan@example.com>, Els Maes <els@example.com>, Els Claes <claes@example.com>', 1794000000);",
            )?;
            Ok(())
        })
        .unwrap();
        let now = ref_a().with_timezone(&Utc);
        let people = s.read(|c| People::build(c, now.timestamp(), &[])).unwrap();
        let parsed = parse("Lunch with Jan and Els fri 12:30 at Mok every week", ref_a(), &prefs());
        let d = s.read(|c| draft(c, &people, &parsed, now, Brussels)).unwrap();
        assert_eq!(d.calendar, calendar);
        assert_eq!(d.title, "Lunch with Jan and Els");
        assert_eq!(d.location, "Mok");
        assert_eq!(d.people.iter().map(|a| a.person.address.as_str()).collect::<Vec<_>>(), ["jan@example.com"]);
        assert_eq!(d.unresolved.len(), 1, "two people called Els");
        assert_eq!(d.unresolved[0].typed, "Els");
        assert_eq!(d.unresolved[0].choices.len(), 2);
        assert_eq!(d.rule.as_ref().map(|r| r.rules.clone()), Some(vec!["FREQ=WEEKLY;BYDAY=FR".to_string()]));
        assert_eq!(d.repeat.as_deref(), Some("Weekly on Friday"));
        let problems = s.read(|c| draft::check_draft(c, &d, Brussels)).unwrap();
        assert!(problems.iter().any(|p| p.blocking && p.text == "Choose who Els is, or remove the name."), "{problems:?}");

        let parsed = parse("Read chapter 12", ref_a(), &prefs());
        let d = s.read(|c| draft(c, &people, &parsed, now, Brussels)).unwrap();
        assert_eq!(d.span, draft::timed_span(now.timestamp() + 1800, now.timestamp() + 3600, Zone::Tz(Brussels), Brussels), "the next half hour");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn names_the_one_weekday_a_line_has() {
        assert_eq!(weekday_named("drinks the friday after next around six"), Some(Weekday::Fri));
        assert_eq!(weekday_named("Rétro tous les vendredis"), Some(Weekday::Fri));
        assert_eq!(weekday_named("borrel vrijdag of zaterdag"), None, "two weekdays");
        assert_eq!(weekday_named("Read chapter 12"), None);
        assert_eq!(weekday_named("Picnic in the sun on friday"), Some(Weekday::Fri));
    }

    #[test]
    fn go_to_date_reads_a_day() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 7).unwrap();
        let day = |y, m, d| NaiveDate::from_ymd_opt(y, m, d);
        for (text, want) in [
            ("fri", day(2026, 11, 13)),
            ("next fri", day(2026, 11, 13)),
            ("sat", day(2026, 11, 7)),
            ("14 dec", day(2026, 12, 14)),
            ("3 nov", day(2026, 11, 3)),
            ("3 march", day(2027, 3, 3)),
            ("tomorrow", day(2026, 11, 8)),
            ("morgen", day(2026, 11, 8)),
            ("après-demain", day(2026, 11, 9)),
            ("2026-12-14", day(2026, 12, 14)),
            ("14/12", day(2026, 12, 14)),
            ("volgende dinsdag", day(2026, 11, 10)),
            ("mardi prochain", day(2026, 11, 10)),
            ("in 3 weeks", day(2026, 11, 28)),
            ("december", day(2026, 12, 1)),
            ("january", day(2027, 1, 1)),
            ("14", day(2026, 11, 14)),
            ("", None),
            ("lunch", None),
            ("fri 9am", None),
        ] {
            assert_eq!(go_to_date(text, today, &prefs()), want, "{text:?}");
        }
    }

    /// Words from the tables and some that aren't, in random lines.
    #[test]
    fn parts_lie_in_the_text_apart_and_rebuild_it() {
        let words = [
            "Lunch", "with", "Jan", "and", "Els", "fri", "12:30", "at", "Mok", "tomorrow", "9:30", "15m", "every", "monday", "10am", "until",
            "18", "dec", "3-4pm", "in", "2", "hours", "met", "vrijdag", "12u30", "bij", "om", "8", "uur", "van", "tot", "avec", "vendredi",
            "12h30", "chez", "le", "14", "déc", "à", "8h", "sofie@example.com", "all", "day", "1:1", "half", "tien", "Lisbon", "time",
            "for", "weeks", "next", "@", "toute", "la", "journée", "2026-11-24", "5/12", "noon", "élan", "chapter", ",", "(x)", "é",
        ];
        let mut seed: u64 = 0x5eed;
        for _ in 0..3000 {
            let mut text = String::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = 1 + (seed >> 59) as usize;
            for _ in 0..n {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(words[(seed >> 33) as usize % words.len()]);
            }
            let p = parse(&text, ref_a(), &prefs());
            let mut last_end = 0;
            for (r, _) in &p.parts {
                assert!(r.start < r.end && r.end <= text.len() && text.is_char_boundary(r.start) && text.is_char_boundary(r.end), "{text:?}: {r:?}");
                assert!(r.start >= last_end, "{text:?}: parts overlap {:?}", p.parts);
                last_end = r.end;
            }
            for tok in tokens(&text) {
                let in_title = p.title.split(' ').any(|w| w == tok.text);
                let in_part = p.parts.iter().any(|(r, _)| r.start <= tok.range.start && tok.range.end <= r.end);
                assert!(in_title || in_part, "{text:?}: {:?} is lost; {p:?}", tok.text);
            }
        }
    }

    #[test]
    fn reads_eighty_characters_in_under_a_millisecond() {
        let text = "Retro om de twee weken vrijdag 16u tot 18 december met Jan en Els bij De Kroon Gent";
        assert!(text.len() >= 80);
        let runs = 200;
        let started = std::time::Instant::now();
        for _ in 0..runs {
            std::hint::black_box(parse(std::hint::black_box(text), ref_a(), &prefs()));
        }
        let each = started.elapsed() / runs;
        assert!(each < std::time::Duration::from_millis(1), "{each:?} a parse");
    }
}
