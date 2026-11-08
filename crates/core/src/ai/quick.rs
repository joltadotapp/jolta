//! Quick create with AI (C3 spec, section 7). When the parser leaves words
//! that look like a day or time, or finds no day or time in a line with
//! digits, the user's model reads the line too. The user typed it, so this
//! is a foreground request, allowed with a cloud provider. The model never
//! works out a weekday: the next 21 days go with the line, as AI search
//! hands over its dates. Every field the model gives back is checked here,
//! and one that fails is dropped and listed.

use std::time::Duration as Wait;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Utc, Weekday};
use chrono_tz::Tz;
use serde_json::Value;

use super::{Ai, Cancel, Finish, Request};
use crate::calendar::draft::{self, EventDraft};
use crate::calendar::quick::{self, Lang, Parsed, Prefs};
use crate::calendar::{Calendar, Local, Recurrence, Span, Zone, recur, repeat, tz};
use crate::people::People;
use crate::{Jolta, Result};

/// How long the editor waits for the model before it opens with the parser's reading.
pub const LIMIT: Wait = Wait::from_secs(5);

/// The editor's draft for a quick create line.
#[derive(Debug)]
pub struct QuickDraft {
    pub draft: EventDraft,
    /// The model filled in a field: the editor says "Read by AI".
    pub by_ai: bool,
    /// What the model said that failed a check, in words.
    pub dropped: Vec<String>,
    /// No day or time was found: the editor puts the focus on the time.
    pub focus_time: bool,
}

/// Whether the model should read a line the parser read as `parsed`.
pub fn wanted(text: &str, parsed: &Parsed) -> bool {
    !parsed.leftover.is_empty() || (parsed.when.is_none() && text.bytes().any(|b| b.is_ascii_digit()))
}

/// The draft for `text`: the parser's reading, with the model's where the
/// parser left something and AI is on. Never saved here.
#[allow(clippy::too_many_arguments)]
pub async fn draft(jolta: &Jolta, ai: &Ai, people: &People, text: &str, now: DateTime<Utc>, here: Tz, prefs: &Prefs, cancel: &Cancel) -> Result<QuickDraft> {
    let local_now = now.with_timezone(&here);
    let mut parsed = quick::parse(text, local_now, prefs);
    let settings = jolta.store.read_async(super::settings).await?;
    let (mut by_ai, mut dropped) = (false, Vec::new());
    if settings.provider.is_some() && wanted(text, &parsed) {
        let calendar = jolta.store.read(draft::default_calendar)?;
        let weekday = quick::weekday_named(text);
        // One retry, inside the same time limit, when the day was refused:
        // small models miss the weekday even with the days listed.
        let asked = async {
            let first = request(text, local_now, prefs);
            let mut ask = first.clone();
            let mut last = None;
            for attempt in 0..2 {
                let Ok(Finish::Done(answer)) = ai.generate(&settings, &ask, cancel, &mut |_| {}).await else { break };
                let reading = read_answer(&answer, local_now, prefs, calendar.as_ref(), weekday);
                let again = attempt == 0 && reading.when.is_none() && !reading.dropped.is_empty();
                if again {
                    ask.user = format!("{}\n\n(Your answer was refused: {} Answer again.)", first.user, reading.dropped.join(" "));
                }
                last = Some(reading);
                if !again {
                    break;
                }
            }
            last
        };
        if let Ok(Some(reading)) = tokio::time::timeout(LIMIT, asked).await {
            dropped.clone_from(&reading.dropped);
            by_ai = merge(&mut parsed, reading);
        }
    }
    let focus_time = parsed.when.is_none();
    let draft = jolta.store.read(|c| quick::draft(c, people, &parsed, now, here))?;
    Ok(QuickDraft { draft, by_ai, dropped, focus_time })
}

const FORMAT: &str = r#"{"title": "...", "all_day": false, "start": "YYYY-MM-DDTHH:MM", "end": null, "duration_minutes": null, "zone": null, "location": null, "people": [], "repeat": null}"#;

/// The local model's answer held to the object's shape.
const GRAMMAR: &str = r#"root ::= "{" ws "\"title\":" ws str "," ws "\"all_day\":" ws bool "," ws "\"start\":" ws (str | "null") "," ws "\"end\":" ws (str | "null") "," ws "\"duration_minutes\":" ws (int | "null") "," ws "\"zone\":" ws (str | "null") "," ws "\"location\":" ws (str | "null") "," ws "\"people\":" ws "[" ws (str (ws "," ws str)*)? ws "]" "," ws "\"repeat\":" ws (str | "null") ws "}"
str ::= "\"" ([^"\\\n] | "\\" ["\\/bfnrt])* "\""
bool ::= "true" | "false"
int ::= [0-9] [0-9]? [0-9]? [0-9]?
ws ::= " "?"#;

/// The request for a line: the line, now with its offset, the zone, the
/// week's first day, the next 21 days with their weekdays, and the languages.
pub fn request(text: &str, now: DateTime<Tz>, prefs: &Prefs) -> Request {
    let today = now.date_naive();
    let days: Vec<String> = (0..21)
        .map(|n| {
            let d = today + Duration::days(n);
            format!(r#"{{"date": "{}", "weekday": "{}"}}"#, d.format("%Y-%m-%d"), d.format("%A"))
        })
        .collect();
    let languages: Vec<&str> = prefs
        .languages
        .iter()
        .map(|l| match l {
            Lang::En => "English",
            Lang::Nl => "Dutch",
            Lang::Fr => "French",
        })
        .collect();
    let system = format!(
        "You read a line someone typed to make a calendar event and answer with one JSON object, nothing else:\n{FORMAT}\n\n\
         - title: the line without the words for the day, time, length, place and repeat. Names of people stay in it.\n\
         - start: \"YYYY-MM-DD\" for an all-day event, else \"YYYY-MM-DDTHH:MM\" on a 24-hour clock.\n\
         - end: the same form, or null with duration_minutes for a length. An all-day event's end is its last day.\n\
         - zone: an IANA zone such as \"Europe/Lisbon\" when the line names one, else null.\n\
         - people: names or addresses the line invites. repeat: an RRULE value such as \"FREQ=WEEKLY;BYDAY=FR\", or null.\n\
         Take every date from the list of days you are given; never work out a weekday yourself. \"friday\" alone is the coming \
         Friday; \"next friday\" is the Friday of next week. An hour from 1 to 7 without am or pm is in the afternoon. Leave out \
         what the line doesn't say."
    );
    let user = format!(
        "Line: {text}\nNow: {} ({})\nWeeks start on {}.\nThe next 21 days: [{}]\nThe line may be in {}.",
        now.format("%Y-%m-%dT%H:%M:%S%:z"),
        now.timezone().name(),
        prefs.week_start,
        days.join(", "),
        languages.join(", "),
    );
    Request { system, user, max_tokens: 200, temperature: 0.0, grammar: Some(GRAMMAR.into()) }
}

/// The fields of the model's answer that passed the checks.
#[derive(Debug, Default, PartialEq)]
pub struct Reading {
    pub title: Option<String>,
    pub when: Option<Span>,
    pub location: Option<String>,
    pub people: Vec<String>,
    pub repeat: Option<String>,
    pub dropped: Vec<String>,
}

/// Checks the model's answer: a start from yesterday to two years ahead,
/// on `weekday` when the line names one, an end after the start, a timed
/// event at most 14 days long, a zone chrono-tz knows, an RRULE `rrule`
/// reads and `calendar` can hold.
pub fn read_answer(answer: &str, now: DateTime<Tz>, prefs: &Prefs, calendar: Option<&Calendar>, weekday: Option<Weekday>) -> Reading {
    let mut out = Reading::default();
    let object = match (answer.find('{'), answer.rfind('}')) {
        (Some(a), Some(b)) if a < b => &answer[a..=b],
        _ => answer,
    };
    let Ok(v) = serde_json::from_str::<Value>(object) else {
        out.dropped.push("The answer wasn't JSON.".into());
        return out;
    };
    let text = |key: &str| v[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let here = now.timezone();
    out.title = text("title");
    out.location = text("location");
    out.people = v["people"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).map(str::trim).filter(|p| !p.is_empty()).take(20).map(str::to_string).collect();
    let zone = match text("zone") {
        None => here,
        Some(name) => tz::zone(&name).unwrap_or_else(|| {
            out.dropped.push(format!("The zone {name} isn't one Jolta knows."));
            here
        }),
    };
    let today = now.date_naive();
    let in_range = |d: NaiveDate| d >= today - Duration::days(1) && d <= today + Duration::days(730);
    let start = text("start");
    let timed = start.as_deref().and_then(wall);
    let day = start.as_deref().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let all_day = v["all_day"].as_bool().unwrap_or(timed.is_none());
    match (timed, day) {
        (Some(t), _) if !all_day => {
            if !in_range(t.date()) {
                out.dropped.push(format!("The start {} is outside yesterday to two years ahead.", t.format("%Y-%m-%d %H:%M")));
            } else {
                let length = Duration::minutes(prefs.default_minutes);
                let end = match (text("end").as_deref().and_then(wall), v["duration_minutes"].as_i64()) {
                    (Some(e), _) => Some(e),
                    (None, Some(m)) => Some(t + Duration::minutes(m)),
                    (None, None) => None,
                };
                let end = match end {
                    Some(e) if e <= t => {
                        out.dropped.push("The end is before the start.".into());
                        t + length
                    }
                    Some(e) if e - t > Duration::days(14) => {
                        out.dropped.push("A timed event can't last more than 14 days.".into());
                        t + length
                    }
                    Some(e) => e,
                    None => t + length,
                };
                let at = |wall| Local { wall, zone: Zone::Tz(zone) };
                out.when = Some(Span::Timed { start: at(t), end: at(end) });
            }
        }
        (t, d) if d.is_some() || t.is_some() => {
            let first = d.or(t.map(|t| t.date())).expect("one of them");
            if !in_range(first) {
                out.dropped.push(format!("The start {first} is outside yesterday to two years ahead."));
            } else {
                let last = text("end").and_then(|e| NaiveDate::parse_from_str(e.get(..10).unwrap_or(&e), "%Y-%m-%d").ok());
                let last = match last {
                    Some(l) if l < first => {
                        out.dropped.push("The end is before the start.".into());
                        first
                    }
                    Some(l) => l,
                    None => first,
                };
                out.when = Some(Span::AllDay { start: first, end: last + Duration::days(1) });
            }
        }
        _ => {
            if let Some(s) = start {
                out.dropped.push(format!("{s} isn't a date."));
            }
        }
    }
    if let (Some(span), Some(named)) = (&out.when, weekday) {
        let first = match span {
            Span::AllDay { start, .. } => *start,
            Span::Timed { start, .. } => start.wall.date(),
        };
        if first.weekday() != named {
            out.dropped.push(format!("{} is a {}, but the line names {}.", first.format("%Y-%m-%d"), first.format("%A"), name_of(named)));
            out.when = None;
        }
    }
    if let Some(rule) = text("repeat") {
        let rule = rule.trim_start_matches("RRULE:").to_string();
        let rec = Recurrence { rules: vec![rule.clone()], ..Default::default() };
        let refused = match (&out.when, calendar) {
            _ if !recur::valid(&rule) => Some(format!("{rule} isn't a repeat rule.")),
            (Some(span), Some(c)) => repeat::refusal(&rec, span, c, here),
            _ => None,
        };
        match refused {
            Some(why) => out.dropped.push(why),
            None => out.repeat = Some(rule),
        }
    }
    out
}

fn name_of(wd: Weekday) -> &'static str {
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"][wd.num_days_from_monday() as usize]
}

fn wall(s: &str) -> Option<NaiveDateTime> {
    ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"].iter().find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
}

/// Puts the model's fields where the parser had none. The parser's day and
/// time give way when it left words over, since those words are why the
/// model was asked ("drinks the friday after next around six" isn't this
/// Friday); then the model's title, without those words, goes too. Returns
/// whether the model's reading was used.
pub fn merge(parsed: &mut Parsed, ai: Reading) -> bool {
    let mut used = false;
    if ai.when.is_some() && (parsed.when.is_none() || !parsed.leftover.is_empty()) {
        parsed.when = ai.when;
        if let Some(title) = ai.title {
            parsed.title = title;
        }
        parsed.leftover.clear();
        used = true;
    }
    if parsed.location.is_none() && ai.location.is_some() {
        parsed.location = ai.location;
        used = true;
    }
    if parsed.people.is_empty() && !ai.people.is_empty() {
        parsed.people = ai.people;
        used = true;
    }
    if parsed.repeat.is_none() && ai.repeat.is_some() {
        parsed.repeat = ai.repeat;
        used = true;
    }
    used
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::{Brussels, Lisbon};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::calendar::{CalendarSource, ops};

    fn now() -> DateTime<Tz> {
        Brussels.with_ymd_and_hms(2026, 11, 7, 15, 0, 0).unwrap()
    }

    fn prefs() -> Prefs {
        Prefs::new(30, &[Lang::En])
    }

    fn at(s: &str, zone: Tz) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(zone) }
    }

    #[test]
    fn asks_when_the_parser_leaves_words() {
        let p = |t| quick::parse(t, now(), &prefs());
        assert!(wanted("drinks the friday after next around six", &p("drinks the friday after next around six")));
        assert!(wanted("Read chapter 12", &p("Read chapter 12")), "digits and no day or time");
        assert!(!wanted("Lunch with Jan fri 12:30 at Mok", &p("Lunch with Jan fri 12:30 at Mok")));
        assert!(!wanted("Lunch with Jan", &p("Lunch with Jan")));
    }

    #[test]
    fn hands_over_the_next_21_days() {
        let r = request("drinks the friday after next around six", now(), &prefs());
        assert!(r.user.starts_with("Line: drinks the friday after next around six\nNow: 2026-11-07T15:00:00+01:00 (Europe/Brussels)\nWeeks start on Mon."));
        assert!(r.user.contains(r#"{"date": "2026-11-07", "weekday": "Saturday"}, {"date": "2026-11-08", "weekday": "Sunday"}"#));
        assert!(r.user.contains(r#"{"date": "2026-11-27", "weekday": "Friday"}]"#), "21 days, the last on 27 Nov");
        assert!(!r.user.contains("2026-11-28"));
        assert!(r.user.ends_with("The line may be in English, Dutch, French."));
        assert!(r.grammar.is_some());
    }

    /// Answers a model gave, and ones made to fail each check.
    #[test]
    fn checks_every_field() {
        let read = |answer: &str| read_answer(answer, now(), &prefs(), None, None);
        let good = read(
            r#"```json
{"title": "Drinks", "all_day": false, "start": "2026-11-20T18:00", "end": null, "duration_minutes": 90, "zone": null, "location": "Café Mok", "people": ["Jan"], "repeat": null}
```"#,
        );
        assert_eq!(
            good,
            Reading {
                title: Some("Drinks".into()),
                when: Some(Span::Timed { start: at("2026-11-20 18:00", Brussels), end: at("2026-11-20 19:30", Brussels) }),
                location: Some("Café Mok".into()),
                people: vec!["Jan".into()],
                repeat: None,
                dropped: vec![],
            }
        );
        let zoned = read(r#"{"title": "Flight", "all_day": false, "start": "2026-11-14T07:15", "end": "2026-11-14T09:00", "zone": "Europe/Lisbon"}"#);
        assert_eq!(zoned.when, Some(Span::Timed { start: at("2026-11-14 07:15", Lisbon), end: at("2026-11-14 09:00", Lisbon) }));
        let trip = read(r#"{"title": "Trip", "all_day": true, "start": "2026-12-22", "end": "2026-12-23"}"#);
        assert_eq!(trip.when, Some(Span::AllDay { start: NaiveDate::from_ymd_opt(2026, 12, 22).unwrap(), end: NaiveDate::from_ymd_opt(2026, 12, 24).unwrap() }));

        let far = read(r#"{"title": "x", "all_day": false, "start": "2029-01-01T10:00"}"#);
        assert_eq!((far.when, far.dropped.len()), (None, 1), "more than two years ahead");
        let past = read(r#"{"title": "x", "all_day": true, "start": "2026-11-05"}"#);
        assert_eq!((past.when, past.dropped.len()), (None, 1), "before yesterday");
        assert!(read(r#"{"title": "x", "all_day": true, "start": "2026-11-06"}"#).when.is_some(), "yesterday counts");
        let backwards = read(r#"{"title": "x", "all_day": false, "start": "2026-11-20T18:00", "end": "2026-11-20T17:00"}"#);
        assert_eq!(backwards.when, Some(Span::Timed { start: at("2026-11-20 18:00", Brussels), end: at("2026-11-20 18:30", Brussels) }));
        assert_eq!(backwards.dropped, ["The end is before the start."]);
        let long = read(r#"{"title": "x", "all_day": false, "start": "2026-11-20T18:00", "duration_minutes": 30000}"#);
        assert_eq!(long.dropped, ["A timed event can't last more than 14 days."]);
        let zone = read(r#"{"title": "x", "all_day": false, "start": "2026-11-20T18:00", "zone": "Mars/Olympus"}"#);
        assert_eq!(zone.dropped, ["The zone Mars/Olympus isn't one Jolta knows."]);
        assert_eq!(zone.when, Some(Span::Timed { start: at("2026-11-20 18:00", Brussels), end: at("2026-11-20 18:30", Brussels) }));
        let rule = read(r#"{"title": "x", "all_day": false, "start": "2026-11-20T18:00", "repeat": "FREQ=SOMETIMES"}"#);
        assert_eq!((rule.repeat, rule.dropped), (None, vec!["FREQ=SOMETIMES isn't a repeat rule.".to_string()]));
        let weekly = read(r#"{"title": "x", "all_day": false, "start": "2026-11-20T18:00", "repeat": "RRULE:FREQ=WEEKLY;BYDAY=FR"}"#);
        assert_eq!(weekly.repeat.as_deref(), Some("FREQ=WEEKLY;BYDAY=FR"));
        let words = read("I think it is next Friday.");
        assert_eq!(words.dropped, ["The answer wasn't JSON."]);
        assert_eq!(read(r#"{"title": "x", "start": "soon"}"#).dropped, ["soon isn't a date."]);
    }

    /// A small model put "the friday after next" on Sat 28 Nov in the live check.
    #[test]
    fn the_day_must_fall_on_the_weekday_the_line_names() {
        let answer = r#"{"title": "Drinks", "all_day": false, "start": "2026-11-28T18:00"}"#;
        let r = read_answer(answer, now(), &prefs(), None, Some(Weekday::Fri));
        assert_eq!((r.when, r.dropped), (None, vec!["2026-11-28 is a Saturday, but the line names Friday.".to_string()]));
        let r = read_answer(&answer.replace("28", "20"), now(), &prefs(), None, Some(Weekday::Fri));
        assert!(r.when.is_some());
    }

    #[test]
    fn a_rule_the_calendar_cant_hold_is_dropped() {
        let google = Calendar {
            id: crate::calendar::CalendarId(1),
            account_id: None,
            source: CalendarSource::Google,
            remote_id: "primary".into(),
            name: "Work".into(),
            color: None,
            zone: None,
            access: crate::calendar::Access::Owner,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: true,
            cursor: None,
            refresh: None,
            synced: None,
        };
        let r = read_answer(r#"{"title": "x", "all_day": false, "start": "2026-11-20T18:00", "repeat": "FREQ=HOURLY"}"#, now(), &prefs(), Some(&google), None);
        assert_eq!(r.repeat, None);
        assert_eq!(r.dropped.len(), 1, "{:?}", r.dropped);
    }

    #[test]
    fn the_parser_wins_unless_it_left_words() {
        let mut parsed = quick::parse("Lunch with Jan fri 12:30", now(), &prefs());
        let ai = Reading { title: Some("Lunch".into()), when: Some(Span::AllDay { start: NaiveDate::from_ymd_opt(2026, 11, 9).unwrap(), end: NaiveDate::from_ymd_opt(2026, 11, 10).unwrap() }), location: Some("Mok".into()), people: vec!["Els".into()], ..Default::default() };
        let before = parsed.when;
        assert!(merge(&mut parsed, ai), "the place was new");
        assert_eq!((parsed.when, parsed.title.as_str(), parsed.people.clone(), parsed.location.as_deref()), (before, "Lunch with Jan", vec!["Jan".to_string()], Some("Mok")));

        let mut parsed = quick::parse("drinks the friday after next around six", now(), &prefs());
        let when = Some(Span::Timed { start: at("2026-11-20 18:00", Brussels), end: at("2026-11-20 18:30", Brussels) });
        assert!(merge(&mut parsed, Reading { title: Some("Drinks".into()), when, ..Default::default() }));
        assert_eq!((parsed.when, parsed.title.as_str()), (when, "Drinks"));
        assert!(parsed.leftover.is_empty());

        let mut parsed = quick::parse("Read chapter 12", now(), &prefs());
        assert!(!merge(&mut parsed, Reading::default()));
        assert_eq!(parsed.title, "Read chapter 12");
    }

    /// An OpenAI-compatible server on this Mac that streams one answer, so
    /// the whole path runs as it does with a provider, offline.
    async fn fixture_server(answers: &'static [&'static str], delay: Wait) -> String {
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let n = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let answer = answers[n.min(answers.len() - 1)];
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = socket.read(&mut buf).await {
                        if n == 0 {
                            return;
                        }
                        seen.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&seen);
                        if let Some(head) = text.find("\r\n\r\n") {
                            let length = text[..head].lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                            if seen.len() >= head + 4 + length {
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(delay).await;
                    let delta = serde_json::json!({"type": "response.output_text.delta", "delta": answer}).to_string();
                    let body = format!("data: {delta}\n\ndata: {{\"type\":\"response.completed\"}}\n\n");
                    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
            }
        });
        format!("http://{address}")
    }

    fn jolta(name: &str, url: &str) -> (std::path::PathBuf, Jolta) {
        crate::secrets::use_memory_store();
        let dir = std::env::temp_dir().join(format!("jolta-ai-quick-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jolta = Jolta::open(&dir).unwrap();
        jolta
            .store
            .write(|tx| {
                ops::new_local_calendar(tx, "Personal", None)?;
                let mut s = super::super::settings(tx)?;
                s.provider = Some(super::super::Provider::OpenAi);
                s.openai = super::super::Remote { base_url: url.into(), model: "fixture".into() };
                super::super::save_settings(tx, &s)
            })
            .unwrap();
        (dir, jolta)
    }

    #[tokio::test]
    async fn reads_what_the_parser_cant_through_the_provider() {
        let url = fixture_server(
            &[
                r#"{"title": "Drinks", "all_day": false, "start": "2026-11-28T18:00", "end": null, "duration_minutes": null, "zone": null, "location": null, "people": [], "repeat": null}"#,
                r#"{"title": "Drinks", "all_day": false, "start": "2026-11-20T18:00", "end": null, "duration_minutes": null, "zone": null, "location": null, "people": [], "repeat": null}"#,
            ],
            Wait::ZERO,
        )
        .await;
        let (dir, jolta) = jolta("read", &url);
        let ai = Ai::new(&dir, dir.join("no-helper"));
        let people = jolta.store.read(|c| People::build(c, 0, &[])).unwrap();
        let now = now().with_timezone(&Utc);
        let q = draft(&jolta, &ai, &people, "drinks the friday after next around six", now, Brussels, &prefs(), &Cancel::default()).await.unwrap();
        assert!(q.by_ai, "the second answer, after the Saturday was refused");
        assert!(q.dropped.is_empty());
        assert_eq!(q.draft.title, "Drinks");
        assert_eq!(q.draft.span, Span::Timed { start: at("2026-11-20 18:00", Brussels), end: at("2026-11-20 18:30", Brussels) });
        assert!(!q.focus_time);

        let q = draft(&jolta, &ai, &people, "Lunch with Jan fri 12:30 at Mok", now, Brussels, &prefs(), &Cancel::default()).await.unwrap();
        assert!(!q.by_ai, "the parser read it all, so the model isn't asked");
        assert_eq!(q.draft.location, "Mok");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn opens_with_the_parsers_reading_after_five_seconds() {
        let url = fixture_server(&[r#"{"title": "Late"}"#], Wait::from_secs(8)).await;
        let (dir, jolta) = jolta("slow", &url);
        let ai = Ai::new(&dir, dir.join("no-helper"));
        let people = jolta.store.read(|c| People::build(c, 0, &[])).unwrap();
        let started = std::time::Instant::now();
        let q = draft(&jolta, &ai, &people, "drinks the friday after next around six", now().with_timezone(&Utc), Brussels, &prefs(), &Cancel::default()).await.unwrap();
        let waited = started.elapsed();
        assert!(waited >= LIMIT && waited < LIMIT + Wait::from_secs(2), "{waited:?}");
        assert!(!q.by_ai);
        assert_eq!(q.draft.title, "drinks the after next around six");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn stays_with_the_parser_while_ai_is_off() {
        let (dir, jolta) = jolta("off", "http://127.0.0.1:9");
        jolta
            .store
            .write(|tx| {
                let mut s = super::super::settings(tx)?;
                s.provider = None;
                super::super::save_settings(tx, &s)
            })
            .unwrap();
        let ai = Ai::new(&dir, dir.join("no-helper"));
        let people = jolta.store.read(|c| People::build(c, 0, &[])).unwrap();
        let q = draft(&jolta, &ai, &people, "Read chapter 12", now().with_timezone(&Utc), Brussels, &prefs(), &Cancel::default()).await.unwrap();
        assert!(!q.by_ai && q.dropped.is_empty() && q.focus_time);
        assert_eq!(q.draft.title, "Read chapter 12");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
