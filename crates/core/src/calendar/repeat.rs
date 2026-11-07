//! The repeat editor (C3 spec, section 5): presets taken from the event's
//! own date, a custom rule, the next dates a rule makes, and why a calendar
//! can't hold a rule, so the editor greys it before anything fails at save.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use chrono_tz::Tz;

use super::recur::{self, Window, day_name, month_name, weekday_code};
use super::{Calendar, CalendarSource, EventData, Recurrence, Span, Zone, caps, tz};
use crate::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Never,
    Hourly,
    Daily,
    Weekly,
    Weekdays,
    MonthDay,
    MonthNth,
    MonthLast,
    Yearly,
}

/// One line of the repeat editor's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub preset: Preset,
    /// "Every month on the fourth Tuesday".
    pub label: String,
    /// The RRULE, None for Does not repeat.
    pub rule: Option<String>,
    /// Why the calendar can't hold it: the editor greys it with this.
    pub refusal: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Day,
    Week,
    Month,
    Year,
}

/// How a monthly custom rule picks its day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonthBy {
    /// The start's day of the month.
    Day,
    /// The start's weekday at its place in the month ("the fourth Tuesday").
    Weekday,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ends {
    Never,
    /// The last day an occurrence may start on.
    On(NaiveDate),
    After(u32),
}

/// The Custom part of the editor: "Every [n] [unit]", the days of a weekly
/// rule, the day of a monthly one, and the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Custom {
    pub every: u32,
    pub unit: Unit,
    /// For a weekly rule; empty means the start's weekday.
    pub days: Vec<Weekday>,
    pub month_by: MonthBy,
    pub ends: Ends,
}

fn first_day(start: &Span) -> NaiveDate {
    match start {
        Span::AllDay { start, .. } => *start,
        Span::Timed { start, .. } => start.wall.date(),
    }
}

/// The start's weekday counted from the month's start: 4 for the fourth Tuesday.
fn nth_in_month(day: NaiveDate) -> u32 {
    (day.day() - 1) / 7 + 1
}

fn is_last_in_month(day: NaiveDate) -> bool {
    (day + Duration::days(7)).month() != day.month()
}

fn ordinal(n: u32) -> &'static str {
    ["first", "second", "third", "fourth", "fifth"].get(n as usize - 1).copied().unwrap_or("fifth")
}

/// The presets for an event starting `start`, each with the calendar's
/// refusal when it can't hold it. Every hour is offered for timed events
/// only. The month presets both appear when the start is both the fourth
/// and the last of its weekday.
pub fn presets(start: &Span, calendar: &Calendar, here: Tz) -> Vec<Choice> {
    let day = first_day(start);
    let weekday = day.weekday();
    let code = weekday_code(weekday);
    let month = month_name(day.month()).unwrap_or_default();
    let mut list = vec![(Preset::Never, "Does not repeat".to_string(), None)];
    if matches!(start, Span::Timed { .. }) {
        list.push((Preset::Hourly, "Every hour".into(), Some("FREQ=HOURLY".to_string())));
    }
    list.extend([
        (Preset::Daily, "Every day".into(), Some("FREQ=DAILY".to_string())),
        (Preset::Weekly, format!("Every week on {}", day_name(weekday)), Some(format!("FREQ=WEEKLY;BYDAY={code}"))),
        (Preset::Weekdays, "Every weekday (Monday to Friday)".into(), Some("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR".into())),
        (Preset::MonthDay, format!("Every month on day {}", day.day()), Some(format!("FREQ=MONTHLY;BYMONTHDAY={}", day.day()))),
    ]);
    let nth = nth_in_month(day);
    if nth <= 4 {
        list.push((Preset::MonthNth, format!("Every month on the {} {}", ordinal(nth), day_name(weekday)), Some(format!("FREQ=MONTHLY;BYDAY={nth}{code}"))));
    }
    if is_last_in_month(day) {
        list.push((Preset::MonthLast, format!("Every month on the last {}", day_name(weekday)), Some(format!("FREQ=MONTHLY;BYDAY=-1{code}"))));
    }
    list.push((Preset::Yearly, format!("Every year on {} {month}", day.day()), Some(format!("FREQ=YEARLY;BYMONTH={};BYMONTHDAY={}", day.month(), day.day()))));
    list.into_iter()
        .map(|(preset, label, rule)| {
            let refusal = rule.as_ref().and_then(|r| refusal(&one_rule(r), start, calendar, here));
            Choice { preset, label, rule, refusal }
        })
        .collect()
}

fn one_rule(rule: &str) -> Recurrence {
    Recurrence { rules: vec![rule.to_string()], ..Recurrence::default() }
}

/// The preset `rec` is, read from `start`: None for a rule only Custom
/// describes, Never for none.
pub fn preset_of(rec: Option<&Recurrence>, start: &Span, calendar: &Calendar, here: Tz) -> Option<Preset> {
    let Some(rec) = rec.filter(|r| !r.rules.is_empty() || !r.rdates.is_empty()) else { return Some(Preset::Never) };
    if rec.rules.len() != 1 || !rec.rdates.is_empty() {
        return None;
    }
    presets(start, calendar, here).into_iter().find_map(|c| {
        let rule = c.rule.as_deref()?;
        recur::same_repeat(Some(&one_rule(rule)), start, Some(&Recurrence { exdates: Vec::new(), ..rec.clone() }), start, here).then_some(c.preset)
    })
}

/// The RRULE the Custom part makes for an event starting `start`. An end
/// date is the last day an occurrence may start, on the event's own clock,
/// so a timed rule ends at that day's last second, in UTC as RFC 5545 asks
/// of a start in a zone.
pub fn build(custom: &Custom, start: &Span) -> Result<String> {
    if custom.every == 0 {
        return Err(Error::Other("Repeat at least every 1.".into()));
    }
    let day = first_day(start);
    let mut parts = vec![format!(
        "FREQ={}",
        match custom.unit {
            Unit::Day => "DAILY",
            Unit::Week => "WEEKLY",
            Unit::Month => "MONTHLY",
            Unit::Year => "YEARLY",
        }
    )];
    if custom.every > 1 {
        parts.push(format!("INTERVAL={}", custom.every));
    }
    match custom.unit {
        Unit::Day => {}
        Unit::Week => {
            let mut days = if custom.days.is_empty() { vec![day.weekday()] } else { custom.days.clone() };
            days.sort_by_key(Weekday::num_days_from_monday);
            days.dedup();
            parts.push(format!("BYDAY={}", days.iter().map(|d| weekday_code(*d)).collect::<Vec<_>>().join(",")));
        }
        Unit::Month => parts.push(match custom.month_by {
            MonthBy::Day => format!("BYMONTHDAY={}", day.day()),
            MonthBy::Weekday if is_last_in_month(day) && nth_in_month(day) == 5 => format!("BYDAY=-1{}", weekday_code(day.weekday())),
            MonthBy::Weekday => format!("BYDAY={}{}", nth_in_month(day), weekday_code(day.weekday())),
        }),
        Unit::Year => parts.push(format!("BYMONTH={};BYMONTHDAY={}", day.month(), day.day())),
    }
    match custom.ends {
        Ends::Never => {}
        Ends::After(0) => return Err(Error::Other("Repeat at least once.".into())),
        Ends::After(n) => parts.push(format!("COUNT={n}")),
        Ends::On(last) if last < day => return Err(Error::Other("The repeat ends before the event starts.".into())),
        Ends::On(last) => parts.push(format!("UNTIL={}", until(last, start))),
    }
    Ok(parts.join(";"))
}

/// An UNTIL that lets occurrences start up to the end of `last`.
pub fn until(last: NaiveDate, start: &Span) -> String {
    match start {
        Span::AllDay { .. } => last.format("%Y%m%d").to_string(),
        Span::Timed { start, .. } => {
            let end = last.and_hms_opt(23, 59, 59).unwrap_or_default();
            match start.zone {
                Zone::Floating => end.format("%Y%m%dT%H%M%S").to_string(),
                Zone::Tz(zone) => tz::instant(end, zone).format("%Y%m%dT%H%M%SZ").to_string(),
            }
        }
    }
}

/// The Custom part showing `rule`, for a rule it can say: None for one only
/// "Custom repeat" names, which stays as it is unless the user picks a preset.
pub fn custom_of(rule: &str, start: &Span) -> Option<Custom> {
    let day = first_day(start);
    let mut parts = std::collections::BTreeMap::new();
    for part in rule.trim().trim_start_matches("RRULE:").split(';').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=')?;
        parts.insert(k.trim().to_ascii_uppercase(), v.trim().to_ascii_uppercase());
    }
    let unit = match parts.remove("FREQ")?.as_str() {
        "DAILY" => Unit::Day,
        "WEEKLY" => Unit::Week,
        "MONTHLY" => Unit::Month,
        "YEARLY" => Unit::Year,
        _ => return None,
    };
    let every = parts.remove("INTERVAL").map_or(Some(1), |n| n.parse().ok())?;
    let ends = match (parts.remove("COUNT"), parts.remove("UNTIL")) {
        (None, None) => Ends::Never,
        (Some(n), None) => Ends::After(n.parse().ok()?),
        (None, Some(u)) => Ends::On(NaiveDate::parse_from_str(u.get(..8)?, "%Y%m%d").ok().map(|d| match (start, u.ends_with('Z')) {
            // An UTC end read back on the event's clock.
            (Span::Timed { start, .. }, true) => match start.zone {
                Zone::Tz(zone) => chrono::NaiveDateTime::parse_from_str(&u, "%Y%m%dT%H%M%SZ").map_or(d, |t| t.and_utc().with_timezone(&zone).date_naive()),
                Zone::Floating => d,
            },
            _ => d,
        })?),
        _ => return None,
    };
    parts.remove("WKST").filter(|w| w == "MO");
    let mut custom = Custom { every, unit, days: Vec::new(), month_by: MonthBy::Day, ends };
    match unit {
        Unit::Day => {}
        Unit::Week => {
            if let Some(days) = parts.remove("BYDAY") {
                custom.days = days.split(',').map(code_weekday).collect::<Option<Vec<_>>>()?;
            }
        }
        Unit::Month => match (parts.remove("BYMONTHDAY"), parts.remove("BYDAY")) {
            (None, None) => {}
            (Some(d), None) if d.parse::<u32>().ok() == Some(day.day()) => {}
            (None, Some(by)) => {
                let code = weekday_code(day.weekday());
                let fits = by == format!("{}{code}", nth_in_month(day)) || (by == format!("-1{code}") && is_last_in_month(day));
                if !fits {
                    return None;
                }
                custom.month_by = MonthBy::Weekday;
            }
            _ => return None,
        },
        Unit::Year => {
            let month = parts.remove("BYMONTH").map_or(Some(day.month()), |m| m.parse().ok())?;
            let month_day = parts.remove("BYMONTHDAY").map_or(Some(day.day()), |d| d.parse().ok())?;
            if (month, month_day) != (day.month(), day.day()) {
                return None;
            }
        }
    }
    parts.is_empty().then_some(custom)
}

fn code_weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

/// The next `n` occurrences of a series starting `span` with `rec`, from
/// `after` on: the editor's "Next: Thu 26 Nov, Tue 1 Dec, Thu 3 Dec".
pub fn next(rec: &Recurrence, span: &Span, after: DateTime<Utc>, n: usize, here: Tz) -> Vec<Span> {
    let master = EventData { recurrence: Some(rec.clone()), ..EventData::new("", *span) };
    for days in [62, 800, 11_000] {
        let window = Window { start: after, end: after + Duration::days(days) };
        let found = recur::expand(&master, &[], window, here);
        if found.len() >= n || days == 11_000 {
            return found.into_iter().take(n).map(|o| o.span).collect();
        }
    }
    Vec::new()
}

/// Why `calendar` can't hold `rec` for a series starting `span`, if it
/// can't: its provider's limits (`caps.repeat`), on Microsoft 365 a rule
/// Graph's pattern can't repeat the same way. Every rule keeps its first
/// event: RFC 5545 counts the start, as Google does.
pub fn refusal(rec: &Recurrence, span: &Span, calendar: &Calendar, here: Tz) -> Option<String> {
    let limits = caps::of(calendar).repeat;
    let provider = super::draft::provider(calendar);
    for rule in &rec.rules {
        let parts: Vec<(String, String)> = rule
            .trim_start_matches("RRULE:")
            .split(';')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().to_ascii_uppercase()))
            .collect();
        let has = |name: &str| parts.iter().any(|(k, _)| k == name);
        let sub_daily = parts.iter().any(|(k, v)| k == "FREQ" && matches!(v.as_str(), "HOURLY" | "MINUTELY" | "SECONDLY")) || has("BYHOUR") || has("BYMINUTE") || has("BYSECOND");
        if sub_daily && !limits.hourly {
            return Some(format!("{provider} calendars can't repeat more often than daily"));
        }
        if has("BYSETPOS") && !limits.set_pos {
            return Some(format!("{provider} calendars can't repeat on a place in the month such as the last weekday"));
        }
        if parts.iter().any(|(k, v)| k == "BYMONTH" && v.contains(',')) && !limits.several_months {
            return Some(format!("{provider} calendars can't repeat in more than one month of the year"));
        }
    }
    (calendar.source == CalendarSource::Graph && super::sources::graph::pattern(rec, span, here).is_none()).then(|| "Microsoft 365 can't repeat an event this way".into())
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDateTime, TimeZone};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Access, CalendarId, Local};

    fn calendar(source: CalendarSource) -> Calendar {
        Calendar {
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
        }
    }

    fn timed(start: &str) -> Span {
        let wall = NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M").unwrap();
        let zone = Zone::Tz(Brussels);
        Span::Timed { start: Local { wall, zone }, end: Local { wall: wall + Duration::minutes(30), zone } }
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn labels(start: &Span, source: CalendarSource) -> Vec<(String, Option<String>)> {
        presets(start, &calendar(source), Brussels).into_iter().map(|c| (c.label, c.refusal)).collect()
    }

    #[test]
    fn presets_come_from_the_events_own_date_and_grey_what_the_calendar_cant_hold() {
        // Tue 24 Nov 2026 is both the fourth and the last Tuesday.
        let tuesday = timed("2026-11-24 14:00");
        let graph = labels(&tuesday, CalendarSource::Graph);
        assert_eq!(
            graph.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
            [
                "Does not repeat",
                "Every hour",
                "Every day",
                "Every week on Tuesday",
                "Every weekday (Monday to Friday)",
                "Every month on day 24",
                "Every month on the fourth Tuesday",
                "Every month on the last Tuesday",
                "Every year on 24 November",
            ]
        );
        assert_eq!(graph[1].1.as_deref(), Some("Microsoft 365 calendars can't repeat more often than daily"));
        assert!(graph.iter().filter(|(l, _)| l != "Every hour").all(|(_, r)| r.is_none()), "{graph:?}");
        assert_eq!(labels(&tuesday, CalendarSource::Google)[1].1.as_deref(), Some("Google calendars can't repeat more often than daily"));
        assert_eq!(labels(&tuesday, CalendarSource::CalDav)[1].1, None);

        // Thu 29 Oct 2026 is the fifth Thursday: only "last" names it.
        let fifth = labels(&timed("2026-10-29 09:00"), CalendarSource::Graph);
        assert!(fifth.iter().any(|(l, _)| l == "Every month on the last Thursday"));
        assert!(!fifth.iter().any(|(l, _)| l.contains("fifth")));
        // The 10th is no last Tuesday, and an all-day event has no hourly rule.
        let early = Span::AllDay { start: day("2026-11-10"), end: day("2026-11-11") };
        let list = labels(&early, CalendarSource::Google);
        assert!(!list.iter().any(|(l, _)| l == "Every hour" || l.contains("last")), "{list:?}");
        assert!(list.iter().any(|(l, _)| l == "Every month on the second Tuesday"));
    }

    #[test]
    fn a_rule_reads_back_as_its_preset() {
        let tuesday = timed("2026-11-24 14:00");
        let cal = calendar(CalendarSource::Google);
        let rec = |r: &str| Recurrence { rules: vec![r.into()], ..Recurrence::default() };
        assert_eq!(preset_of(None, &tuesday, &cal, Brussels), Some(Preset::Never));
        assert_eq!(preset_of(Some(&rec("FREQ=WEEKLY")), &tuesday, &cal, Brussels), Some(Preset::Weekly), "BYDAY from the start");
        assert_eq!(preset_of(Some(&rec("FREQ=WEEKLY;INTERVAL=1;BYDAY=TU")), &tuesday, &cal, Brussels), Some(Preset::Weekly));
        assert_eq!(preset_of(Some(&rec("FREQ=MONTHLY;BYDAY=-1TU")), &tuesday, &cal, Brussels), Some(Preset::MonthLast));
        assert_eq!(preset_of(Some(&rec("FREQ=WEEKLY;BYDAY=TU,TH")), &tuesday, &cal, Brussels), None, "custom");
        assert_eq!(preset_of(Some(&rec("FREQ=WEEKLY;BYDAY=TU;COUNT=4")), &tuesday, &cal, Brussels), None, "an end makes it custom");
    }

    #[test]
    fn custom_builds_the_rule_the_mockup_shows() {
        let tuesday = timed("2026-11-24 14:00");
        let custom = Custom { every: 1, unit: Unit::Week, days: vec![Weekday::Thu, Weekday::Tue], month_by: MonthBy::Day, ends: Ends::On(day("2026-12-22")) };
        let rule = build(&custom, &tuesday).unwrap();
        // 23:59:59 in Brussels on 22 December is 22:59:59 UTC.
        assert_eq!(rule, "FREQ=WEEKLY;BYDAY=TU,TH;UNTIL=20261222T225959Z");
        let rec = Recurrence { rules: vec![rule.clone()], ..Recurrence::default() };
        assert_eq!(recur::describe(&rec, &tuesday).as_deref(), Some("Weekly on Tuesday and Thursday, until 22 December 2026"));
        let after = Brussels.with_ymd_and_hms(2026, 11, 24, 15, 0, 0).unwrap().with_timezone(&Utc);
        let next3: Vec<String> = next(&rec, &tuesday, after, 3, Brussels).iter().map(|s| match s.start() {
            crate::calendar::Moment::Time(t) => t.wall.format("%a %-d %b").to_string(),
            crate::calendar::Moment::Date(d) => d.to_string(),
        }).collect();
        assert_eq!(next3, ["Thu 26 Nov", "Tue 1 Dec", "Thu 3 Dec"]);
        let last = next(&rec, &tuesday, after, 100, Brussels).last().map(|s| s.start());
        assert_eq!(last, Some(timed("2026-12-22 14:00").start()), "the end day itself is kept");
        assert_eq!(custom_of(&rule, &tuesday), Some(Custom { days: vec![Weekday::Tue, Weekday::Thu], ..custom }));

        let monthly = Custom { every: 2, unit: Unit::Month, days: Vec::new(), month_by: MonthBy::Weekday, ends: Ends::After(6) };
        assert_eq!(build(&monthly, &tuesday).unwrap(), "FREQ=MONTHLY;INTERVAL=2;BYDAY=4TU;COUNT=6");
        let fifth = Custom { unit: Unit::Month, month_by: MonthBy::Weekday, every: 1, days: Vec::new(), ends: Ends::Never };
        assert_eq!(build(&fifth, &timed("2026-10-29 09:00")).unwrap(), "FREQ=MONTHLY;BYDAY=-1TH");
        let all_day = Span::AllDay { start: day("2026-11-24"), end: day("2026-11-25") };
        assert_eq!(build(&Custom { unit: Unit::Year, ends: Ends::On(day("2030-11-24")), ..fifth.clone() }, &all_day).unwrap(), "FREQ=YEARLY;BYMONTH=11;BYMONTHDAY=24;UNTIL=20301124");
        assert!(build(&Custom { every: 0, ..fifth.clone() }, &tuesday).is_err());
        assert!(build(&Custom { ends: Ends::On(day("2026-11-01")), ..fifth }, &tuesday).is_err());
        assert_eq!(custom_of("FREQ=MONTHLY;BYDAY=1MO", &tuesday), None, "another weekday than the start's");
        assert_eq!(custom_of("FREQ=DAILY;BYHOUR=9", &tuesday), None);
    }

    #[test]
    fn a_calendar_refuses_what_its_provider_cant_hold() {
        let tuesday = timed("2026-11-24 14:00");
        let rec = |r: &str| Recurrence { rules: vec![r.into()], ..Recurrence::default() };
        let graph = calendar(CalendarSource::Graph);
        assert!(refusal(&rec("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1"), &tuesday, &graph, Brussels).is_some());
        assert!(refusal(&rec("FREQ=YEARLY;BYMONTH=3,9;BYMONTHDAY=1"), &tuesday, &graph, Brussels).is_some());
        assert_eq!(refusal(&rec("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1"), &tuesday, &calendar(CalendarSource::Google), Brussels), None);
        assert!(refusal(&rec("FREQ=WEEKLY;BYHOUR=9,15"), &tuesday, &calendar(CalendarSource::Google), Brussels).is_some(), "Google drops BYHOUR");
        assert_eq!(refusal(&rec("FREQ=WEEKLY;BYDAY=TU,TH"), &tuesday, &graph, Brussels), None);
    }
}
