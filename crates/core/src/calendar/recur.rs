//! Repeating events: expand a series into the occurrences inside a window.

use std::str::FromStr;

use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday,
};
use chrono_tz::Tz;
use rrule::{Frequency, NWeekday, RRule, Unvalidated};

use super::tz::{self, MomentKey};
use super::{EventData, EventStatus, Local, Moment, Recurrence, Span, Zone};

/// Each series stops here, so a `FREQ=MINUTELY` rule from a stranger's
/// invitation can't fill the database.
pub const SERIES_CAP: usize = 5_000;

/// Rule steps one expansion may take, so a `FREQ=SECONDLY` series that
/// started years ago returns quickly instead of walking to the window.
const STEP_BUDGET: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Master,
    /// Index into the `overrides` passed to `expand`.
    Override(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub source: Source,
    pub span: Span,
}

/// The occurrences of `master` that overlap `window`, ordered by start. A
/// single event yields at most one. Rules, RDATEs and EXDATEs run on the
/// master's wall clock in its own zone; each override replaces the
/// occurrence its recurrence id names, and a cancelled override removes it.
/// Overrides moved into the window from outside it count, as do overrides
/// whose recurrence id matches no occurrence. At most `SERIES_CAP`.
pub fn expand(
    master: &EventData,
    overrides: &[EventData],
    window: Window,
    here: Tz,
) -> Vec<Occurrence> {
    let Some(rec) = &master.recurrence else {
        return overlaps(&master.span, window, here)
            .then_some(Occurrence {
                source: Source::Master,
                span: master.span,
            })
            .into_iter()
            .collect();
    };
    let clock = Clock::new(&master.span, here);

    let ids: Vec<Option<MomentKey>> = overrides
        .iter()
        .map(|o| o.recurrence_id.map(|id| id.key(here)))
        .collect();
    // Two overrides for one occurrence: the higher SEQUENCE wins, then the later one.
    let replacing: Vec<(usize, MomentKey)> = ids
        .iter()
        .enumerate()
        .filter_map(|(i, key)| Some((i, (*key)?)))
        .filter(|&(i, key)| {
            !ids.iter().enumerate().any(|(j, other)| {
                j != i
                    && other.is_some_and(|o| o.matches(&key))
                    && (overrides[j].sequence, j) > (overrides[i].sequence, i)
            })
        })
        .collect();
    let mut following: Vec<Following> = replacing
        .iter()
        .filter(|&&(i, _)| overrides[i].this_and_future)
        .filter_map(|&(i, key)| {
            let event = &overrides[i];
            let id = event.recurrence_id?;
            Some(Following {
                index: i,
                key,
                shift: clock.wall(&event.span.start()) - clock.wall(&id),
            })
        })
        .collect();
    following.sort_by(|a, b| compare(&a.key, &b.key));

    let reach = following
        .iter()
        .map(|f| f.shift.abs() + length(&overrides[f.index].span, here))
        .max()
        .unwrap_or_default();
    let margin = length(&master.span, here) + reach + Duration::days(1);
    let (Some(lo), Some(hi)) = (
        window.start.naive_utc().checked_sub_signed(margin),
        window.end.naive_utc().checked_add_signed(margin),
    ) else {
        return Vec::new();
    };
    let range = Range {
        lo,
        hi,
        sure: window.start.naive_utc() + Duration::days(1),
    };
    let limit = SERIES_CAP + rec.exdates.len() + overrides.len();

    let mut candidates: Vec<Candidate> = clock
        .starts(rec, range, limit)
        .into_iter()
        .map(|wall| {
            let key = clock.moment(wall).key(here);
            Candidate {
                wall,
                key,
                at: key.at.unwrap_or_else(|| midnight(key.day, here)),
            }
        })
        .collect();
    candidates.sort_by_key(|c| c.at);
    candidates.dedup_by(|a, b| a.key.matches(&b.key));
    let exdates: Vec<MomentKey> = rec.exdates.iter().map(|x| x.key(here)).collect();
    candidates.retain(|c| {
        !exdates.iter().any(|x| x.matches(&c.key))
            && !replacing.iter().any(|(_, id)| id.matches(&c.key))
    });

    let mut out: Vec<Occurrence> = replacing
        .iter()
        .map(|&(i, _)| (i, &overrides[i]))
        .filter(|(_, o)| o.status != EventStatus::Cancelled)
        .map(|(i, o)| Occurrence {
            source: Source::Override(i),
            span: o.span,
        })
        .collect();
    for candidate in candidates {
        let governing = following
            .iter()
            .rev()
            .find(|f| compare(&candidate.key, &f.key).is_ge());
        let occurrence = match governing {
            None => Occurrence {
                source: Source::Master,
                span: clock.place(&master.span, candidate.wall),
            },
            Some(f) => {
                let event = &overrides[f.index];
                if event.status == EventStatus::Cancelled {
                    continue;
                }
                let Some(wall) = candidate.wall.checked_add_signed(f.shift) else {
                    continue;
                };
                Occurrence {
                    source: Source::Override(f.index),
                    span: clock.place(&event.span, wall),
                }
            }
        };
        out.push(occurrence);
    }

    out.retain(|o| overlaps(&o.span, window, here));
    out.sort_by_cached_key(|o| placed(&o.span, here).0);
    out.truncate(SERIES_CAP);
    out
}

struct Candidate {
    wall: NaiveDateTime,
    key: MomentKey,
    at: DateTime<Utc>,
}

/// A `RANGE=THISANDFUTURE` override: from its recurrence id on, every
/// occurrence moves by `shift` on the series' wall clock and takes the
/// override's length and details.
struct Following {
    index: usize,
    key: MomentKey,
    shift: Duration,
}

#[derive(Clone, Copy)]
struct Range {
    lo: NaiveDateTime,
    hi: NaiveDateTime,
    /// Starts from here on are inside the window whatever the zone, so
    /// only they count towards the cap.
    sure: NaiveDateTime,
}

/// The wall clock a series repeats on: dates for all-day series, the
/// master's start zone otherwise.
struct Clock {
    all_day: bool,
    zone: Zone,
    time: NaiveTime,
    first: NaiveDateTime,
    here: Tz,
}

impl Clock {
    fn new(span: &Span, here: Tz) -> Clock {
        match span {
            Span::AllDay { start, .. } => Clock {
                all_day: true,
                zone: Zone::Floating,
                time: NaiveTime::MIN,
                first: start.and_time(NaiveTime::MIN),
                here,
            },
            Span::Timed { start, .. } => Clock {
                all_day: false,
                zone: start.zone,
                time: start.wall.time(),
                first: start.wall,
                here,
            },
        }
    }

    fn wall(&self, moment: &Moment) -> NaiveDateTime {
        match moment {
            Moment::Date(day) => day.and_time(self.time),
            Moment::Time(local) if self.all_day || local.zone == self.zone => local.wall,
            Moment::Time(local) => tz::wall(local.instant(self.here), self.zone, self.here),
        }
    }

    fn moment(&self, wall: NaiveDateTime) -> Moment {
        if self.all_day {
            Moment::Date(wall.date())
        } else {
            Moment::Time(Local {
                wall,
                zone: self.zone,
            })
        }
    }

    /// `template` moved to start at `wall`, keeping its length: whole days
    /// for all-day spans, the exact duration for timed ones (RFC 5545
    /// section 3.8.5.3).
    fn place(&self, template: &Span, wall: NaiveDateTime) -> Span {
        match template {
            Span::AllDay { start, end } => {
                let day = wall.date();
                Span::AllDay {
                    start: day,
                    end: day.checked_add_signed(*end - *start).unwrap_or(day),
                }
            }
            Span::Timed { start, end } => {
                let zone = if self.all_day { start.zone } else { self.zone };
                let begin = Local { wall, zone };
                let finish =
                    begin.instant(self.here) + (end.instant(self.here) - start.instant(self.here));
                Span::Timed {
                    start: begin,
                    end: Local {
                        wall: tz::wall(finish, end.zone, self.here),
                        zone: end.zone,
                    },
                }
            }
        }
    }

    /// Series starts as wall times between `range.lo` and `range.hi`, plus
    /// DTSTART and every RDATE, unsorted and possibly repeated.
    fn starts(&self, rec: &Recurrence, range: Range, limit: usize) -> Vec<NaiveDateTime> {
        let mut out = vec![self.first];
        out.extend(rec.rdates.iter().map(|r| self.clock_wall(r)));
        let mut budget = STEP_BUDGET;
        for text in &rec.rules {
            self.rule_starts(text, range, limit, &mut budget, &mut out);
        }
        out
    }

    fn clock_wall(&self, moment: &Moment) -> NaiveDateTime {
        let wall = self.wall(moment);
        if self.all_day {
            wall.date().and_time(NaiveTime::MIN)
        } else {
            wall
        }
    }

    fn rule_starts(
        &self,
        text: &str,
        range: Range,
        limit: usize,
        budget: &mut usize,
        out: &mut Vec<NaiveDateTime>,
    ) {
        let Some(Rule { rrule, until }) = Rule::parse(text) else {
            tracing::warn!(rule = text, "unreadable repeat rule");
            return;
        };
        let until = until.map(|u| self.until(u));
        let first = fast_forward(&rrule, self.first, range.lo);
        if until.is_some_and(|u| u < first) {
            return;
        }
        let rrule = match until {
            Some(u) => rrule.until(pretend(u)),
            None => rrule,
        };
        let set = match rrule.build(pretend(first)) {
            Ok(set) => set.limit(),
            Err(error) => {
                tracing::warn!(rule = text, %error, "invalid repeat rule");
                return;
            }
        };
        let mut counted = 0;
        for start in &set {
            let wall = start.naive_utc();
            if wall > range.hi {
                break;
            }
            if *budget == 0 {
                tracing::warn!(rule = text, "repeat rule stopped after {STEP_BUDGET} steps");
                break;
            }
            *budget -= 1;
            if wall >= range.lo {
                out.push(wall);
                if wall >= range.sure {
                    counted += 1;
                    if counted >= limit {
                        break;
                    }
                }
            }
        }
    }

    /// UNTIL on the series' wall clock. A UTC value converts to the series'
    /// zone; a date includes that whole day.
    fn until(&self, until: Until) -> NaiveDateTime {
        let end_of_day = |day: NaiveDate| day.and_hms_opt(23, 59, 59).unwrap_or_default();
        match until {
            Until::Date(day) => end_of_day(day),
            Until::Wall(wall) if self.all_day => end_of_day(wall.date()),
            Until::Wall(wall) => wall,
            Until::Utc(at) => {
                let wall = tz::wall(at, self.zone, self.here);
                if self.all_day {
                    end_of_day(wall.date())
                } else {
                    wall
                }
            }
        }
    }
}

/// The rule's own start moved forward by whole periods to just before
/// `lo`, so short-period rules that began long ago don't walk the gap.
/// Only for rules whose phase is fixed by a whole number of periods from
/// DTSTART: without COUNT, weekly or shorter.
fn fast_forward(
    rule: &RRule<Unvalidated>,
    first: NaiveDateTime,
    lo: NaiveDateTime,
) -> NaiveDateTime {
    if rule.get_count().is_some() || lo <= first {
        return first;
    }
    let unit: i64 = match rule.get_freq() {
        Frequency::Secondly => 1,
        Frequency::Minutely => 60,
        Frequency::Hourly => 3_600,
        Frequency::Daily => 86_400,
        Frequency::Weekly => 604_800,
        Frequency::Monthly | Frequency::Yearly => return first,
    };
    let period = unit * i64::from(rule.get_interval().max(1));
    let gap = (lo - first).num_seconds();
    first + Duration::seconds(gap / period * period)
}

/// A wall time handed to `rrule` as UTC, so the rule never sees a clock
/// change; `tz::instant` applies the zone afterwards.
fn pretend(wall: NaiveDateTime) -> DateTime<rrule::Tz> {
    rrule::Tz::UTC.from_utc_datetime(&wall)
}

fn midnight(day: NaiveDate, here: Tz) -> DateTime<Utc> {
    tz::instant(day.and_time(NaiveTime::MIN), here)
}

/// Where a span sits on the timeline; all-day spans run from midnight to
/// midnight in `here`.
fn placed(span: &Span, here: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    match span {
        Span::AllDay { start, end } => (midnight(*start, here), midnight(*end, here)),
        Span::Timed { start, end } => (start.instant(here), end.instant(here)),
    }
}

fn length(span: &Span, here: Tz) -> Duration {
    let (start, end) = placed(span, here);
    (end - start).max(Duration::zero())
}

fn overlaps(span: &Span, window: Window, here: Tz) -> bool {
    let (start, end) = placed(span, here);
    start < window.end && (end > window.start || start == window.start)
}

fn compare(a: &MomentKey, b: &MomentKey) -> std::cmp::Ordering {
    match (a.at, b.at) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.day.cmp(&b.day),
    }
}

/// An RRULE with UNTIL taken out, since `rrule` can't place a UTC UNTIL on
/// a wall clock it treats as UTC.
struct Rule {
    rrule: RRule<Unvalidated>,
    until: Option<Until>,
}

#[derive(Clone, Copy)]
enum Until {
    Date(NaiveDate),
    Wall(NaiveDateTime),
    Utc(DateTime<Utc>),
}

impl Rule {
    fn parse(text: &str) -> Option<Rule> {
        let text = text.trim();
        let text = text.strip_prefix("RRULE:").unwrap_or(text);
        let mut until = None;
        let mut kept = Vec::new();
        for part in text.split(';') {
            let Some((name, value)) = part.split_once('=') else {
                continue;
            };
            let (name, value) = (name.trim().to_ascii_uppercase(), value.trim());
            match name.as_str() {
                "UNTIL" => until = Some(Until::parse(value)?),
                "FREQ" | "INTERVAL" | "COUNT" | "WKST" | "BYSETPOS" | "BYMONTH" | "BYMONTHDAY"
                | "BYYEARDAY" | "BYWEEKNO" | "BYDAY" | "BYHOUR" | "BYMINUTE" | "BYSECOND" => {
                    kept.push(format!("{name}={value}"))
                }
                _ => {}
            }
        }
        let rrule = RRule::from_str(&kept.join(";")).ok()?;
        Some(Rule { rrule, until })
    }
}

impl Until {
    fn parse(value: &str) -> Option<Until> {
        if let Some(utc) = value.strip_suffix('Z').or_else(|| value.strip_suffix('z')) {
            return NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S")
                .ok()
                .map(|t| Until::Utc(t.and_utc()));
        }
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
            .map(Until::Wall)
            .or_else(|_| NaiveDate::parse_from_str(value, "%Y%m%d").map(Until::Date))
            .ok()
    }
}

/// Whether two series repeat the same way from their starts: the same
/// rules and RDATEs, read with their defaults filled in. Graph writes no
/// `INTERVAL=1` where an invitation mail does, and either may leave out
/// the BYDAY a weekly rule takes from its start. EXDATEs don't count.
pub fn same_repeat(a: Option<&Recurrence>, a_start: &Span, b: Option<&Recurrence>, b_start: &Span, here: Tz) -> bool {
    let repeats = |r: Option<&Recurrence>| r.filter(|r| !r.rules.is_empty() || !r.rdates.is_empty()).cloned();
    match (repeats(a), repeats(b)) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            let rules = |r: &Recurrence, start: &Span| {
                let mut all: Vec<_> = r.rules.iter().map(|t| normal_rule(t, start, here)).collect();
                all.sort();
                all
            };
            a.rdates == b.rdates && rules(&a, a_start) == rules(&b, b_start)
        }
        _ => false,
    }
}

/// An RRULE as its parts, with what RFC 5545 leaves to the start written
/// out, lists sorted, and UNTIL as the start of the last occurrence it keeps.
fn normal_rule(text: &str, start: &Span, here: Tz) -> std::collections::BTreeMap<String, String> {
    let text = text.trim();
    let mut parts: std::collections::BTreeMap<String, String> = text
        .strip_prefix("RRULE:")
        .unwrap_or(text)
        .split(';')
        .filter_map(|p| p.split_once('='))
        .map(|(n, v)| (n.trim().to_ascii_uppercase(), v.trim().to_ascii_uppercase()))
        .filter(|(n, _)| !n.starts_with("X-"))
        .collect();
    for (_, value) in parts.iter_mut().filter(|(n, _)| n.starts_with("BY")) {
        let mut items: Vec<&str> = value.split(',').map(str::trim).collect();
        items.sort_unstable();
        items.dedup();
        *value = items.join(",");
    }
    if parts.get("INTERVAL").is_some_and(|i| i == "1") {
        parts.remove("INTERVAL");
    }
    let first = match start {
        Span::AllDay { start, .. } => *start,
        Span::Timed { start, .. } => start.wall.date(),
    };
    let has = |p: &std::collections::BTreeMap<String, String>, names: &[&str]| names.iter().any(|n| p.contains_key(*n));
    let day_parts = ["BYDAY", "BYMONTHDAY", "BYYEARDAY", "BYWEEKNO"];
    match parts.get("FREQ").map(String::as_str) {
        Some("WEEKLY") if !parts.contains_key("BYDAY") => {
            parts.insert("BYDAY".into(), weekday_code(first.weekday()).into());
        }
        Some("MONTHLY") if !has(&parts, &day_parts) => {
            parts.insert("BYMONTHDAY".into(), first.day().to_string());
        }
        Some("YEARLY") if !has(&parts, &day_parts) => {
            parts.entry("BYMONTH".into()).or_insert_with(|| first.month().to_string());
            parts.insert("BYMONTHDAY".into(), first.day().to_string());
        }
        _ => {}
    }
    // The week start changes nothing for a weekly rule every week.
    let weeks_matter = parts.contains_key("BYWEEKNO") || (parts.get("FREQ").is_some_and(|f| f == "WEEKLY") && parts.contains_key("INTERVAL"));
    if !weeks_matter || parts.get("WKST").is_some_and(|w| w == "MO") {
        parts.remove("WKST");
    }
    if let Some(last) = parts.get("UNTIL").and_then(|u| Until::parse(u)).and_then(|until| last_start(text, until, start, here)) {
        parts.insert("UNTIL".into(), last);
    }
    parts
}

/// The start of the last occurrence `rule` makes from `start` up to its
/// `until`, found by expanding back from `until` over ever longer windows.
/// None when even 30 years back hold no occurrence, or for a rule too dense
/// to expand, such as one every second.
fn last_start(rule: &str, until: Until, start: &Span, here: Tz) -> Option<String> {
    let clock = Clock::new(start, here);
    let end = match clock.moment(clock.until(until)) {
        Moment::Date(day) => midnight(day.succ_opt()?, here),
        Moment::Time(local) => local.instant(here) + Duration::seconds(1),
    };
    let master = EventData { recurrence: Some(Recurrence { rules: vec![rule.to_string()], ..Recurrence::default() }), ..EventData::new("", *start) };
    [1, 35, 800, 11_000].into_iter().find_map(|days| {
        let window = Window { start: end - Duration::days(days), end };
        expand(&master, &[], window, here).last().map(|o| match o.span.start() {
            Moment::Date(day) => day.format("%Y%m%d").to_string(),
            Moment::Time(local) => local.instant(here).format("%Y%m%dT%H%M%SZ").to_string(),
        })
    })
}

pub(crate) fn weekday_code(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

/// The rule in plain English ("Every 2 weeks on Tuesday and Thursday,
/// until 19 December 2026"), or None when it doesn't fit a sentence and the
/// UI shows "Custom repeat".
pub fn describe(rec: &Recurrence, start: &Span) -> Option<String> {
    let [text] = rec.rules.as_slice() else {
        return None;
    };
    if !rec.rdates.is_empty() {
        return None;
    }
    let Rule { rrule, until } = Rule::parse(text)?;
    if !(rrule.get_by_hour().is_empty()
        && rrule.get_by_minute().is_empty()
        && rrule.get_by_second().is_empty()
        && rrule.get_by_year_day().is_empty()
        && rrule.get_by_week_no().is_empty())
    {
        return None;
    }
    let first = match start {
        Span::AllDay { start, .. } => *start,
        Span::Timed { start, .. } => start.wall.date(),
    };
    let n = rrule.get_interval().max(1);
    let every = |one: &str, unit: &str| {
        if n == 1 {
            one.to_string()
        } else {
            format!("Every {n} {unit}s")
        }
    };
    let days = rrule.get_by_weekday();
    let month_days = rrule.get_by_month_day();
    let months = rrule.get_by_month();
    let set_pos = rrule.get_by_set_pos();
    let plain_days: Option<Vec<Weekday>> = days
        .iter()
        .map(|d| {
            if let NWeekday::Every(day) = d {
                Some(*day)
            } else {
                None
            }
        })
        .collect();
    let weekdays = plain_days.as_ref().is_some_and(|d| is_weekdays(d));

    let mut text = match rrule.get_freq() {
        Frequency::Secondly | Frequency::Minutely | Frequency::Hourly
            if days.is_empty() && month_days.is_empty() && months.is_empty() =>
        {
            match rrule.get_freq() {
                Frequency::Hourly => every("Hourly", "hour"),
                Frequency::Minutely => every("Every minute", "minute"),
                _ => every("Every second", "second"),
            }
        }
        Frequency::Daily if months.is_empty() && month_days.is_empty() && set_pos.is_empty() => {
            match (days.is_empty(), weekdays) {
                (true, _) => every("Daily", "day"),
                (false, true) if n == 1 => "Every weekday".to_string(),
                _ => return None,
            }
        }
        Frequency::Weekly if months.is_empty() && month_days.is_empty() && set_pos.is_empty() => {
            let days = match plain_days {
                Some(d) if d.is_empty() => vec![first.weekday()],
                Some(d) => d,
                None => return None,
            };
            if is_weekdays(&days) {
                if n == 1 {
                    "Every weekday".to_string()
                } else {
                    format!("Every {n} weeks on weekdays")
                }
            } else {
                format!(
                    "{} on {}",
                    every("Weekly", "week"),
                    join(days.iter().map(|d| day_name(*d).to_string()))
                )
            }
        }
        Frequency::Monthly if months.is_empty() => {
            format!(
                "{} on the {}",
                every("Monthly", "month"),
                day_in_period(days, month_days, set_pos, weekdays, first)?
            )
        }
        Frequency::Yearly => {
            let month = match months {
                [] => first.month(),
                [m] => u32::from(*m),
                _ => return None,
            };
            let when = if days.is_empty() && set_pos.is_empty() {
                match month_days {
                    [] => format!("{} {}", first.day(), month_name(month)?),
                    [d] if *d > 0 => format!("{d} {}", month_name(month)?),
                    _ => return None,
                }
            } else if month_days.is_empty() && !weekdays {
                format!(
                    "the {} of {}",
                    day_in_period(days, month_days, set_pos, false, first)?,
                    month_name(month)?
                )
            } else {
                return None;
            };
            format!(
                "{} on {when}",
                if n == 1 {
                    "Every year".to_string()
                } else {
                    format!("Every {n} years")
                }
            )
        }
        _ => return None,
    };

    match (rrule.get_count(), until) {
        (Some(_), Some(_)) => return None,
        (Some(1), None) => text.push_str(", once"),
        (Some(count), None) => text.push_str(&format!(", {count} times")),
        (None, Some(until)) => {
            let day = match until {
                Until::Date(day) => day,
                Until::Wall(wall) => wall.date(),
                Until::Utc(at) => match start {
                    Span::Timed {
                        start:
                            Local {
                                zone: Zone::Tz(tz), ..
                            },
                        ..
                    } => at.with_timezone(tz).date_naive(),
                    _ => at.date_naive(),
                },
            };
            text.push_str(&format!(
                ", until {} {} {}",
                day.day(),
                month_name(day.month())?,
                day.year()
            ));
        }
        (None, None) => {}
    }
    Some(text)
}

/// "15th", "1st and 15th", "last day", "last Friday", "second weekday".
fn day_in_period(
    days: &[NWeekday],
    month_days: &[i8],
    set_pos: &[i32],
    weekdays: bool,
    first: NaiveDate,
) -> Option<String> {
    match (days, month_days, set_pos) {
        ([], [], []) => month_day(first.day() as i32),
        ([], md, []) => md
            .iter()
            .map(|d| month_day(i32::from(*d)))
            .collect::<Option<Vec<_>>>()
            .map(join),
        ([NWeekday::Nth(n, day)], [], []) => {
            Some(format!("{} {}", position(i32::from(*n))?, day_name(*day)))
        }
        ([NWeekday::Every(day)], [], [n]) => Some(format!("{} {}", position(*n)?, day_name(*day))),
        (_, [], [n]) if weekdays => Some(format!("{} weekday", position(*n)?)),
        _ => None,
    }
}

fn is_weekdays(days: &[Weekday]) -> bool {
    use Weekday::*;
    let mut sorted: Vec<u32> = days.iter().map(|d| d.num_days_from_monday()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sorted == [Mon, Tue, Wed, Thu, Fri].map(|d| d.num_days_from_monday())
}

fn position(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "first",
        2 => "second",
        3 => "third",
        4 => "fourth",
        5 => "fifth",
        -1 => "last",
        -2 => "second to last",
        _ => return None,
    })
}

fn month_day(n: i32) -> Option<String> {
    match n {
        1..=31 => {
            let suffix = match (n % 10, n % 100) {
                (_, 11..=13) => "th",
                (1, _) => "st",
                (2, _) => "nd",
                (3, _) => "rd",
                _ => "th",
            };
            Some(format!("{n}{suffix}"))
        }
        -1 => Some("last day".to_string()),
        -2 => Some("second to last day".to_string()),
        _ => None,
    }
}

fn join(items: impl IntoIterator<Item = String>) -> String {
    let items: Vec<String> = items.into_iter().collect();
    match items.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

pub(crate) fn day_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

pub(crate) fn month_name(month: u32) -> Option<&'static str> {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    NAMES.get(month.checked_sub(1)? as usize).copied()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use chrono_tz::America::New_York;
    use chrono_tz::Asia::Tokyo;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Bounds, ShowAs, Visibility};

    fn wall(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        wall(s).and_utc()
    }

    fn local(s: &str, tz: Tz) -> Local {
        Local {
            wall: wall(s),
            zone: Zone::Tz(tz),
        }
    }

    fn at(s: &str, tz: Tz) -> Moment {
        Moment::Time(local(s, tz))
    }

    fn timed(start: &str, minutes: i64, tz: Tz) -> Span {
        let start = local(start, tz);
        let end = Local {
            wall: start.wall + Duration::minutes(minutes),
            ..start
        };
        Span::Timed { start, end }
    }

    fn all_day(start: &str, days: i64) -> Span {
        Span::AllDay {
            start: date(start),
            end: date(start) + Duration::days(days),
        }
    }

    fn event(span: Span, rules: &[&str]) -> EventData {
        EventData {
            uid: "series".into(),
            recurrence_id: None,
            this_and_future: false,
            span,
            recurrence: (!rules.is_empty()).then(|| Recurrence {
                rules: rules.iter().map(|r| r.to_string()).collect(),
                ..Recurrence::default()
            }),
            title: String::new(),
            location: String::new(),
            description: String::new(),
            description_html: false,
            url: None,
            status: EventStatus::Confirmed,
            show_as: ShowAs::Busy,
            visibility: Visibility::Default,
            organizer: None,
            attendees: Vec::new(),
            sequence: 0,
            alarms: Vec::new(),
            meeting: None,
            categories: Vec::new(),
            color: None,
            master_id: None,
            options: Default::default(),
        }
    }

    fn moved(id: Moment, span: Span) -> EventData {
        EventData {
            recurrence_id: Some(id),
            ..event(span, &[])
        }
    }

    fn window(from: &str, to: &str) -> Window {
        Window {
            start: utc(&format!("{from} 00:00")),
            end: utc(&format!("{to} 00:00")),
        }
    }

    /// Start walls as the event's own clock shows them.
    fn starts(occurrences: &[Occurrence]) -> Vec<String> {
        occurrences
            .iter()
            .map(|o| match o.span {
                Span::AllDay { start, .. } => start.to_string(),
                Span::Timed { start, .. } => start.wall.format("%Y-%m-%d %H:%M").to_string(),
            })
            .collect()
    }

    fn days(occurrences: &[Occurrence]) -> Vec<String> {
        starts(occurrences)
            .iter()
            .map(|s| s[..10].to_string())
            .collect()
    }

    fn utc_starts(occurrences: &[Occurrence], here: Tz) -> Vec<String> {
        occurrences
            .iter()
            .map(|o| match o.span.bounds(here) {
                Bounds::Utc { start, .. } => start.format("%Y-%m-%d %H:%M").to_string(),
                Bounds::Days { .. } => panic!("all-day occurrence"),
            })
            .collect()
    }

    /// An RFC 5545 section 3.8.5.3 example: DTSTART in America/New_York,
    /// one hour long, expanded over 1996 to 2010.
    fn rfc(dtstart: &str, rule: &str) -> Vec<Occurrence> {
        let master = event(timed(dtstart, 60, New_York), &[rule]);
        expand(&master, &[], window("1996-01-01", "2010-01-01"), Brussels)
    }

    fn rfc_days(dtstart: &str, rule: &str) -> Vec<String> {
        days(&rfc(dtstart, rule))
    }

    fn list(days: &str) -> Vec<String> {
        days.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn rfc_daily_for_10_occurrences() {
        assert_eq!(
            rfc_days("1997-09-02 09:00", "FREQ=DAILY;COUNT=10"),
            list(
                "1997-09-02 1997-09-03 1997-09-04 1997-09-05 1997-09-06 1997-09-07 1997-09-08 1997-09-09 1997-09-10 1997-09-11"
            )
        );
    }

    #[test]
    fn rfc_daily_until_utc_keeps_wall_time_across_the_clock_change() {
        let occurrences = rfc("1997-09-02 09:00", "FREQ=DAILY;UNTIL=19971224T000000Z");
        assert_eq!(occurrences.len(), 113);
        assert_eq!(days(&occurrences).last().unwrap(), "1997-12-23");
        assert!(starts(&occurrences).iter().all(|s| s.ends_with("09:00")));
        let utc = utc_starts(&occurrences, Brussels);
        assert_eq!(utc[0], "1997-09-02 13:00");
        assert_eq!(utc.last().unwrap(), "1997-12-23 14:00");
    }

    #[test]
    fn rfc_every_10_days_5_occurrences() {
        assert_eq!(
            rfc_days("1997-09-02 09:00", "FREQ=DAILY;INTERVAL=10;COUNT=5"),
            list("1997-09-02 1997-09-12 1997-09-22 1997-10-02 1997-10-12")
        );
    }

    #[test]
    fn rfc_every_day_in_january_for_3_years() {
        let days = rfc_days(
            "1998-01-01 09:00",
            "FREQ=YEARLY;UNTIL=20000131T140000Z;BYMONTH=1;BYDAY=SU,MO,TU,WE,TH,FR,SA",
        );
        assert_eq!(days.len(), 93);
        assert_eq!(days.last().unwrap(), "2000-01-31");
        assert!(days.iter().all(|d| &d[5..7] == "01"));
    }

    #[test]
    fn rfc_weekly_for_10_occurrences() {
        assert_eq!(
            rfc_days("1997-09-02 09:00", "FREQ=WEEKLY;COUNT=10"),
            list(
                "1997-09-02 1997-09-09 1997-09-16 1997-09-23 1997-09-30 1997-10-07 1997-10-14 1997-10-21 1997-10-28 1997-11-04"
            )
        );
    }

    #[test]
    fn rfc_every_other_week_monday_wednesday_friday_until() {
        assert_eq!(
            rfc_days(
                "1997-09-01 09:00",
                "FREQ=WEEKLY;INTERVAL=2;UNTIL=19971224T000000Z;WKST=SU;BYDAY=MO,WE,FR"
            ),
            list(
                "1997-09-01 1997-09-03 1997-09-05 1997-09-15 1997-09-17 1997-09-19 1997-09-29 1997-10-01 1997-10-03 \
                 1997-10-13 1997-10-15 1997-10-17 1997-10-27 1997-10-29 1997-10-31 1997-11-10 1997-11-12 1997-11-14 \
                 1997-11-24 1997-11-26 1997-11-28 1997-12-08 1997-12-10 1997-12-12 1997-12-22"
            )
        );
    }

    #[test]
    fn rfc_tuesday_and_thursday_for_five_weeks() {
        assert_eq!(
            rfc_days(
                "1997-09-02 09:00",
                "FREQ=WEEKLY;COUNT=10;WKST=SU;BYDAY=TU,TH"
            ),
            list(
                "1997-09-02 1997-09-04 1997-09-09 1997-09-11 1997-09-16 1997-09-18 1997-09-23 1997-09-25 1997-09-30 1997-10-02"
            )
        );
    }

    #[test]
    fn rfc_first_friday_for_10_occurrences() {
        assert_eq!(
            rfc_days("1997-09-05 09:00", "FREQ=MONTHLY;COUNT=10;BYDAY=1FR"),
            list(
                "1997-09-05 1997-10-03 1997-11-07 1997-12-05 1998-01-02 1998-02-06 1998-03-06 1998-04-03 1998-05-01 1998-06-05"
            )
        );
    }

    #[test]
    fn rfc_second_to_last_monday_for_6_months() {
        assert_eq!(
            rfc_days("1997-09-22 09:00", "FREQ=MONTHLY;COUNT=6;BYDAY=-2MO"),
            list("1997-09-22 1997-10-20 1997-11-17 1997-12-22 1998-01-19 1998-02-16")
        );
    }

    #[test]
    fn rfc_third_to_last_day_of_the_month() {
        assert_eq!(
            rfc_days("1997-09-28 09:00", "FREQ=MONTHLY;BYMONTHDAY=-3")[..6],
            list("1997-09-28 1997-10-29 1997-11-28 1997-12-29 1998-01-29 1998-02-26")
        );
    }

    #[test]
    fn rfc_2nd_and_15th_for_10_occurrences() {
        assert_eq!(
            rfc_days("1997-09-02 09:00", "FREQ=MONTHLY;COUNT=10;BYMONTHDAY=2,15"),
            list(
                "1997-09-02 1997-09-15 1997-10-02 1997-10-15 1997-11-02 1997-11-15 1997-12-02 1997-12-15 1998-01-02 1998-01-15"
            )
        );
    }

    #[test]
    fn rfc_every_friday_the_13th_with_dtstart_excluded() {
        let mut master = event(
            timed("1997-09-02 09:00", 60, New_York),
            &["FREQ=MONTHLY;BYDAY=FR;BYMONTHDAY=13"],
        );
        master
            .recurrence
            .as_mut()
            .unwrap()
            .exdates
            .push(at("1997-09-02 09:00", New_York));
        let occurrences = expand(&master, &[], window("1996-01-01", "2001-01-01"), Brussels);
        assert_eq!(
            days(&occurrences),
            list("1998-02-13 1998-03-13 1998-11-13 1999-08-13 2000-10-13")
        );
    }

    #[test]
    fn rfc_us_presidential_election_day() {
        assert_eq!(
            rfc_days(
                "1996-11-05 09:00",
                "FREQ=YEARLY;INTERVAL=4;BYMONTH=11;BYDAY=TU;BYMONTHDAY=2,3,4,5,6,7,8"
            )[..3],
            list("1996-11-05 2000-11-07 2004-11-02")
        );
    }

    #[test]
    fn rfc_third_tuesday_wednesday_or_thursday() {
        assert_eq!(
            rfc_days(
                "1997-09-04 09:00",
                "FREQ=MONTHLY;COUNT=3;BYDAY=TU,WE,TH;BYSETPOS=3"
            ),
            list("1997-09-04 1997-10-07 1997-11-06")
        );
    }

    #[test]
    fn rfc_second_to_last_weekday() {
        assert_eq!(
            rfc_days(
                "1997-09-29 09:00",
                "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-2"
            )[..7],
            list("1997-09-29 1997-10-30 1997-11-27 1997-12-30 1998-01-29 1998-02-26 1998-03-30")
        );
    }

    #[test]
    fn rfc_every_20_minutes_from_9_to_16_40() {
        let first_day: Vec<String> = starts(&rfc(
            "1997-09-02 09:00",
            "FREQ=DAILY;BYHOUR=9,10,11,12,13,14,15,16;BYMINUTE=0,20,40",
        ))
        .into_iter()
        .filter(|s| s.starts_with("1997-09-02"))
        .collect();
        assert_eq!(first_day.len(), 24);
        assert_eq!(
            first_day[..3],
            list("1997-09-02_09:00 1997-09-02_09:20 1997-09-02_09:40")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );
        assert_eq!(first_day.last().unwrap(), "1997-09-02 16:40");
    }

    #[test]
    fn rfc_week_start_changes_the_days() {
        assert_eq!(
            rfc_days(
                "1997-08-05 09:00",
                "FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=MO"
            ),
            list("1997-08-05 1997-08-10 1997-08-19 1997-08-24")
        );
        assert_eq!(
            rfc_days(
                "1997-08-05 09:00",
                "FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=TU,SU;WKST=SU"
            ),
            list("1997-08-05 1997-08-17 1997-08-19 1997-08-31")
        );
    }

    #[test]
    fn rfc_february_30th_is_ignored() {
        assert_eq!(
            rfc_days("2007-01-15 09:00", "FREQ=MONTHLY;BYMONTHDAY=15,30;COUNT=5"),
            list("2007-01-15 2007-01-30 2007-02-15 2007-03-15 2007-03-30")
        );
    }

    #[test]
    fn rfc_every_thursday_in_march() {
        assert_eq!(
            rfc_days("1997-03-13 09:00", "FREQ=YEARLY;BYMONTH=3;BYDAY=TH")[..11],
            list(
                "1997-03-13 1997-03-20 1997-03-27 1998-03-05 1998-03-12 1998-03-19 1998-03-26 1999-03-04 1999-03-11 1999-03-18 1999-03-25"
            )
        );
    }

    #[test]
    fn the_31st_skips_short_months() {
        let master = event(
            timed("2027-01-31 10:00", 30, Brussels),
            &["FREQ=MONTHLY;COUNT=7"],
        );
        assert_eq!(
            days(&expand(
                &master,
                &[],
                window("2027-01-01", "2028-06-01"),
                Brussels
            )),
            list("2027-01-31 2027-03-31 2027-05-31 2027-07-31 2027-08-31 2027-10-31 2027-12-31")
        );
    }

    #[test]
    fn february_29th_yearly_only_in_leap_years() {
        let master = event(all_day("2024-02-29", 1), &["FREQ=YEARLY"]);
        assert_eq!(
            days(&expand(
                &master,
                &[],
                window("2024-01-01", "2034-01-01"),
                Brussels
            )),
            list("2024-02-29 2028-02-29 2032-02-29")
        );
    }

    #[test]
    fn last_friday_of_the_month() {
        let master = event(
            timed("2026-11-27 10:00", 60, Brussels),
            &["FREQ=MONTHLY;BYDAY=-1FR;COUNT=4"],
        );
        assert_eq!(
            days(&expand(
                &master,
                &[],
                window("2026-11-01", "2027-06-01"),
                Brussels
            )),
            list("2026-11-27 2026-12-25 2027-01-29 2027-02-26")
        );
    }

    #[test]
    fn last_weekday_of_the_month() {
        let master = event(
            timed("2026-10-30 17:00", 30, Brussels),
            &["FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1;COUNT=4"],
        );
        assert_eq!(
            days(&expand(
                &master,
                &[],
                window("2026-10-01", "2027-06-01"),
                Brussels
            )),
            list("2026-10-30 2026-11-30 2026-12-31 2027-01-29")
        );
    }

    #[test]
    fn utc_until_for_a_brussels_series() {
        let w = window("2026-11-01", "2027-01-01");
        let including = event(
            timed("2026-11-03 09:00", 60, Brussels),
            &["FREQ=WEEKLY;UNTIL=20261124T080000Z"],
        );
        assert_eq!(
            days(&expand(&including, &[], w, Tokyo)),
            list("2026-11-03 2026-11-10 2026-11-17 2026-11-24")
        );
        let excluding = event(
            timed("2026-11-03 09:00", 60, Brussels),
            &["FREQ=WEEKLY;UNTIL=20261124T075959Z"],
        );
        assert_eq!(
            days(&expand(&excluding, &[], w, Tokyo)),
            list("2026-11-03 2026-11-10 2026-11-17")
        );
    }

    #[test]
    fn date_until_for_an_all_day_series() {
        let master = event(all_day("2026-11-01", 1), &["FREQ=DAILY;UNTIL=20261105"]);
        assert_eq!(
            days(&expand(
                &master,
                &[],
                window("2026-10-01", "2027-01-01"),
                Brussels
            )),
            list("2026-11-01 2026-11-02 2026-11-03 2026-11-04 2026-11-05")
        );
    }

    #[test]
    fn exdates_in_other_zones_match_by_instant() {
        let mut master = event(
            timed("2026-11-04 09:00", 60, Brussels),
            &["FREQ=DAILY;COUNT=5"],
        );
        let rec = master.recurrence.as_mut().unwrap();
        rec.exdates.push(at("2026-11-05 08:00", chrono_tz::UTC));
        rec.exdates.push(at("2026-11-06 03:00", New_York));
        rec.exdates.push(at("2026-11-07 09:00", chrono_tz::UTC));
        assert_eq!(
            days(&expand(
                &master,
                &[],
                window("2026-11-01", "2026-12-01"),
                Brussels
            )),
            list("2026-11-04 2026-11-07 2026-11-08")
        );
    }

    #[test]
    fn rdates_add_and_duplicates_collapse() {
        let mut master = event(
            timed("2026-11-02 09:00", 60, Brussels),
            &["FREQ=WEEKLY;COUNT=2"],
        );
        let rec = master.recurrence.as_mut().unwrap();
        rec.rdates.push(at("2026-11-04 15:00", Brussels));
        rec.rdates.push(at("2026-11-09 08:00", chrono_tz::UTC));
        assert_eq!(
            starts(&expand(
                &master,
                &[],
                window("2026-11-01", "2026-12-01"),
                Brussels
            )),
            list("2026-11-02_09:00 2026-11-04_15:00 2026-11-09_09:00")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );
    }

    fn weekly_monday() -> EventData {
        event(timed("2026-11-02 09:00", 60, Brussels), &["FREQ=WEEKLY"])
    }

    #[test]
    fn override_moves_an_occurrence_onto_another_day() {
        let overrides = [moved(
            at("2026-11-09 09:00", Brussels),
            timed("2026-11-11 14:00", 60, Brussels),
        )];
        let occurrences = expand(
            &weekly_monday(),
            &overrides,
            window("2026-11-01", "2026-11-24"),
            Brussels,
        );
        assert_eq!(
            starts(&occurrences),
            list("2026-11-02_09:00 2026-11-11_14:00 2026-11-16_09:00 2026-11-23_09:00")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );
        assert_eq!(occurrences[1].source, Source::Override(0));
        assert_eq!(occurrences[0].source, Source::Master);
    }

    #[test]
    fn override_id_in_utc_matches_a_zoned_occurrence() {
        let overrides = [moved(
            at("2026-11-09 08:00", chrono_tz::UTC),
            timed("2026-11-09 11:00", 60, Brussels),
        )];
        let occurrences = expand(
            &weekly_monday(),
            &overrides,
            window("2026-11-08", "2026-11-10"),
            Brussels,
        );
        assert_eq!(starts(&occurrences), vec!["2026-11-09 11:00"]);
    }

    #[test]
    fn cancelled_override_removes_the_occurrence() {
        let mut cancelled = moved(
            at("2026-11-09 09:00", Brussels),
            timed("2026-11-09 09:00", 60, Brussels),
        );
        cancelled.status = EventStatus::Cancelled;
        let occurrences = expand(
            &weekly_monday(),
            &[cancelled],
            window("2026-11-01", "2026-11-17"),
            Brussels,
        );
        assert_eq!(days(&occurrences), list("2026-11-02 2026-11-16"));
    }

    #[test]
    fn overrides_moved_across_the_window_edge() {
        let overrides = [
            moved(
                at("2026-12-07 09:00", Brussels),
                timed("2026-11-20 10:00", 60, Brussels),
            ),
            moved(
                at("2026-11-16 09:00", Brussels),
                timed("2026-12-14 10:00", 60, Brussels),
            ),
        ];
        let occurrences = expand(
            &weekly_monday(),
            &overrides,
            window("2026-11-01", "2026-12-01"),
            Brussels,
        );
        assert_eq!(
            days(&occurrences),
            list("2026-11-02 2026-11-09 2026-11-20 2026-11-23 2026-11-30")
        );
        assert_eq!(occurrences[2].source, Source::Override(0));
    }

    #[test]
    fn unmatched_override_still_shows_and_higher_sequence_wins() {
        let orphan = moved(
            at("2026-11-04 12:00", Brussels),
            timed("2026-11-04 12:00", 60, Brussels),
        );
        let old = moved(
            at("2026-11-09 09:00", Brussels),
            timed("2026-11-10 09:00", 60, Brussels),
        );
        let new = EventData {
            sequence: 2,
            ..moved(
                at("2026-11-09 09:00", Brussels),
                timed("2026-11-11 09:00", 60, Brussels),
            )
        };
        let occurrences = expand(
            &weekly_monday(),
            &[new, orphan, old],
            window("2026-11-01", "2026-11-15"),
            Brussels,
        );
        assert_eq!(days(&occurrences), list("2026-11-02 2026-11-04 2026-11-11"));
        assert_eq!(occurrences[2].source, Source::Override(0));
    }

    #[test]
    fn this_and_future_shifts_later_occurrences_on_the_wall_clock() {
        let master = event(timed("2027-03-08 09:00", 60, Brussels), &["FREQ=WEEKLY"]);
        let following = EventData {
            this_and_future: true,
            title: "Moved".into(),
            ..moved(
                at("2027-03-22 09:00", Brussels),
                timed("2027-03-22 10:00", 90, Brussels),
            )
        };
        let occurrences = expand(
            &master,
            &[following],
            window("2027-03-01", "2027-04-10"),
            Brussels,
        );
        assert_eq!(
            starts(&occurrences),
            list("2027-03-08_09:00 2027-03-15_09:00 2027-03-22_10:00 2027-03-29_10:00 2027-04-05_10:00")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );
        assert!(
            occurrences[2..]
                .iter()
                .all(|o| o.source == Source::Override(0))
        );
        assert_eq!(
            occurrences[3].span.bounds(Brussels),
            Bounds::Utc {
                start: utc("2027-03-29 08:00"),
                end: utc("2027-03-29 09:30")
            }
        );
    }

    #[test]
    fn cancelled_this_and_future_ends_the_series() {
        let mut ending = EventData {
            this_and_future: true,
            ..moved(
                at("2026-11-16 09:00", Brussels),
                timed("2026-11-16 09:00", 60, Brussels),
            )
        };
        ending.status = EventStatus::Cancelled;
        let occurrences = expand(
            &weekly_monday(),
            &[ending],
            window("2026-11-01", "2026-12-31"),
            Brussels,
        );
        assert_eq!(days(&occurrences), list("2026-11-02 2026-11-09"));
    }

    #[test]
    fn nonexistent_and_repeated_times_in_a_daily_series() {
        let master = event(
            timed("2027-03-27 02:30", 30, Brussels),
            &["FREQ=DAILY;COUNT=3"],
        );
        let occurrences = expand(&master, &[], window("2027-03-01", "2027-04-01"), Brussels);
        assert_eq!(
            utc_starts(&occurrences, Brussels),
            list("2027-03-27_01:30 2027-03-28_01:30 2027-03-29_00:30")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );

        let autumn = event(
            timed("2027-10-30 02:30", 30, Brussels),
            &["FREQ=DAILY;COUNT=3"],
        );
        let occurrences = expand(&autumn, &[], window("2027-10-01", "2027-11-30"), Brussels);
        assert_eq!(
            utc_starts(&occurrences, Brussels),
            list("2027-10-30_00:30 2027-10-31_00:30 2027-11-01_01:30")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn all_day_series_gives_the_same_days_in_tokyo_and_brussels() {
        let master = event(all_day("2026-11-11", 2), &["FREQ=YEARLY"]);
        let w = window("2026-01-01", "2029-01-01");
        let tokyo = expand(&master, &[], w, Tokyo);
        let brussels = expand(&master, &[], w, Brussels);
        assert_eq!(tokyo, brussels);
        assert_eq!(
            tokyo.iter().map(|o| o.span).collect::<Vec<_>>(),
            vec![
                all_day("2026-11-11", 2),
                all_day("2027-11-11", 2),
                all_day("2028-11-11", 2)
            ]
        );
    }

    #[test]
    fn new_york_weekly_seen_as_utc_across_march_2027() {
        let new_york = event(timed("2027-03-01 09:00", 60, New_York), &["FREQ=WEEKLY"]);
        let brussels = event(timed("2027-03-01 09:00", 60, Brussels), &["FREQ=WEEKLY"]);
        let w = window("2027-03-01", "2027-04-01");
        assert_eq!(
            utc_starts(&expand(&new_york, &[], w, Brussels), Brussels),
            list("2027-03-01_14:00 2027-03-08_14:00 2027-03-15_13:00 2027-03-22_13:00 2027-03-29_13:00").iter().map(|s| s.replace('_', " ")).collect::<Vec<_>>()
        );
        assert_eq!(
            utc_starts(&expand(&brussels, &[], w, Brussels), Brussels),
            list("2027-03-01_08:00 2027-03-08_08:00 2027-03-15_08:00 2027-03-22_08:00 2027-03-29_07:00").iter().map(|s| s.replace('_', " ")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn floating_series_follows_here() {
        let mut master = event(
            timed("2026-11-02 09:00", 60, Brussels),
            &["FREQ=DAILY;COUNT=2"],
        );
        if let Span::Timed { start, end } = &mut master.span {
            start.zone = Zone::Floating;
            end.zone = Zone::Floating;
        }
        let w = window("2026-11-01", "2026-12-01");
        assert_eq!(
            utc_starts(&expand(&master, &[], w, Tokyo), Tokyo),
            list("2026-11-02_00:00 2026-11-03_00:00")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn single_events_and_window_edges() {
        let w = Window {
            start: utc("2026-11-04 09:00"),
            end: utc("2026-11-04 10:00"),
        };
        let one = |start: &str, minutes| {
            expand(
                &event(timed(start, minutes, chrono_tz::UTC), &[]),
                &[],
                w,
                Brussels,
            )
            .len()
        };
        assert_eq!(one("2026-11-04 08:00", 60), 0);
        assert_eq!(one("2026-11-04 08:00", 61), 1);
        assert_eq!(one("2026-11-04 09:00", 0), 1);
        assert_eq!(one("2026-11-04 09:59", 0), 1);
        assert_eq!(one("2026-11-04 10:00", 0), 0);
        assert_eq!(one("2026-11-04 10:00", 60), 0);
    }

    #[test]
    fn long_running_daily_series_reaches_today() {
        let master = event(timed("2010-01-01 09:00", 15, Brussels), &["FREQ=DAILY"]);
        let occurrences = expand(&master, &[], window("2026-11-01", "2026-12-01"), Brussels);
        assert_eq!(occurrences.len(), 30);
        assert_eq!(starts(&occurrences)[0], "2026-11-01 09:00");

        let hourly = event(timed("2015-06-01 09:00", 15, Brussels), &["FREQ=HOURLY"]);
        let occurrences = expand(
            &hourly,
            &[],
            Window {
                start: utc("2026-11-04 00:00"),
                end: utc("2026-11-04 04:00"),
            },
            Brussels,
        );
        assert_eq!(
            starts(&occurrences),
            list("2026-11-04_01:00 2026-11-04_02:00 2026-11-04_03:00 2026-11-04_04:00")
                .iter()
                .map(|s| s.replace('_', " "))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn series_stops_at_the_cap() {
        let master = event(
            timed("2026-11-01 00:00", 1, chrono_tz::UTC),
            &["FREQ=MINUTELY"],
        );
        let occurrences = expand(&master, &[], window("2026-11-01", "2027-01-01"), Brussels);
        assert_eq!(occurrences.len(), SERIES_CAP);
        assert!(
            occurrences
                .windows(2)
                .all(|p| p[0].span.bounds(Brussels) != p[1].span.bounds(Brussels))
        );
    }

    #[test]
    fn secondly_rules_from_long_ago_return_quickly() {
        let w = window("2026-11-01", "2027-11-01");
        for rule in [
            "FREQ=SECONDLY",
            "FREQ=SECONDLY;COUNT=1000000000",
            "FREQ=SECONDLY;BYMONTH=2;BYMONTHDAY=30",
            "FREQ=MINUTELY;INTERVAL=7",
        ] {
            let master = event(timed("2015-06-01 09:00", 1, Brussels), &[rule]);
            let started = Instant::now();
            let occurrences = expand(&master, &[], w, Brussels);
            let took = started.elapsed();
            assert!(took.as_millis() < 200, "{rule} took {took:?}");
            assert!(occurrences.len() <= SERIES_CAP);
        }
    }

    #[test]
    fn invalid_rules_keep_the_first_occurrence() {
        let w = window("2026-11-01", "2026-12-01");
        for rule in [
            "FREQ=FORTNIGHTLY",
            "garbage",
            "",
            "FREQ=DAILY;UNTIL=20200101T000000Z",
            "FREQ=DAILY;BYMONTHDAY=40",
        ] {
            let master = event(timed("2026-11-02 09:00", 60, Brussels), &[rule]);
            assert_eq!(
                days(&expand(&master, &[], w, Brussels)),
                list("2026-11-02"),
                "{rule}"
            );
        }
        let tolerant = event(
            timed("2026-11-02 09:00", 60, Brussels),
            &["RRULE:FREQ=DAILY;COUNT=2;X-NAME=1;"],
        );
        assert_eq!(
            days(&expand(&tolerant, &[], w, Brussels)),
            list("2026-11-02 2026-11-03")
        );
    }

    fn say(rule: &str, start: Span) -> Option<String> {
        describe(
            &Recurrence {
                rules: vec![rule.to_string()],
                ..Recurrence::default()
            },
            &start,
        )
    }

    #[test]
    fn describes_rules_in_plain_english() {
        let tuesday = timed("2026-11-03 09:00", 60, Brussels);
        let cases = [
            (
                "FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH;UNTIL=20261219T225959Z",
                "Every 2 weeks on Tuesday and Thursday, until 19 December 2026",
            ),
            ("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR", "Every weekday"),
            ("FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR", "Every weekday"),
            ("FREQ=WEEKLY", "Weekly on Tuesday"),
            (
                "FREQ=WEEKLY;BYDAY=MO,WE,FR",
                "Weekly on Monday, Wednesday and Friday",
            ),
            ("FREQ=MONTHLY;BYDAY=-1FR", "Monthly on the last Friday"),
            (
                "FREQ=MONTHLY;BYDAY=FR;BYSETPOS=-1",
                "Monthly on the last Friday",
            ),
            (
                "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
                "Monthly on the last weekday",
            ),
            ("FREQ=MONTHLY;BYMONTHDAY=31", "Monthly on the 31st"),
            (
                "FREQ=MONTHLY;INTERVAL=3;BYMONTHDAY=2,15",
                "Every 3 months on the 2nd and 15th",
            ),
            ("FREQ=MONTHLY", "Monthly on the 3rd"),
            ("FREQ=DAILY;COUNT=10", "Daily, 10 times"),
            ("FREQ=DAILY;INTERVAL=2", "Every 2 days"),
            (
                "FREQ=YEARLY;BYMONTH=11;BYDAY=4TH",
                "Every year on the fourth Thursday of November",
            ),
        ];
        for (rule, expected) in cases {
            assert_eq!(say(rule, tuesday).as_deref(), Some(expected), "{rule}");
        }
        assert_eq!(
            say("FREQ=YEARLY", all_day("2026-11-11", 1)).as_deref(),
            Some("Every year on 11 November")
        );
        assert_eq!(
            say("FREQ=YEARLY", all_day("2024-02-29", 1)).as_deref(),
            Some("Every year on 29 February")
        );
        assert_eq!(
            say("FREQ=DAILY;UNTIL=20261105", all_day("2026-11-01", 1)).as_deref(),
            Some("Daily, until 5 November 2026")
        );
    }

    #[test]
    fn complex_rules_have_no_sentence() {
        let start = timed("2026-11-03 09:00", 60, Brussels);
        for rule in [
            "FREQ=MONTHLY;BYDAY=FR;BYMONTHDAY=13",
            "FREQ=DAILY;BYHOUR=9,10",
            "FREQ=YEARLY;BYWEEKNO=20;BYDAY=MO",
            "FREQ=MONTHLY;BYDAY=TU,WE,TH;BYSETPOS=3",
            "FREQ=YEARLY;BYMONTH=6,7",
            "garbage",
        ] {
            assert_eq!(say(rule, start), None, "{rule}");
        }
        let two = Recurrence {
            rules: vec!["FREQ=DAILY".into(), "FREQ=WEEKLY".into()],
            ..Recurrence::default()
        };
        assert_eq!(describe(&two, &start), None);
    }

    #[test]
    fn the_same_repeat_written_two_ways() {
        let start = timed("2026-11-12 16:00", 60, Brussels);
        let rec = |rule: &str| Recurrence { rules: vec![rule.into()], ..Recurrence::default() };
        let same = |a: &str, b: &str| same_repeat(Some(&rec(a)), &start, Some(&rec(b)), &start, Brussels);
        assert!(same("FREQ=WEEKLY;INTERVAL=1;BYDAY=TH;WKST=SU", "FREQ=WEEKLY;BYDAY=TH;WKST=SU"), "Graph leaves out INTERVAL=1");
        assert!(same("FREQ=WEEKLY", "FREQ=WEEKLY;BYDAY=TH;WKST=SU"), "a weekly rule repeats on its start's day, whatever week start");
        assert!(same("FREQ=WEEKLY;BYDAY=TH,TU", "freq=weekly;byday=TU,TH"));
        assert!(same("FREQ=WEEKLY;BYDAY=TH;UNTIL=20261217T150000Z", "FREQ=WEEKLY;BYDAY=TH;UNTIL=20261217T225959Z"), "both keep the 17 December occurrence");
        assert!(!same("FREQ=WEEKLY;BYDAY=TH;UNTIL=20261217T145959Z", "FREQ=WEEKLY;BYDAY=TH;UNTIL=20261217T225959Z"), "one stops a minute before it");
        assert!(same("FREQ=WEEKLY;BYDAY=TH;UNTIL=20261217", "FREQ=WEEKLY;BYDAY=TH;UNTIL=20261219"), "no Thursday on the 18th or the 19th");
        assert!(!same("FREQ=WEEKLY;BYDAY=TH;UNTIL=20261216", "FREQ=WEEKLY;BYDAY=TH;UNTIL=20261217"));
        assert!(same("FREQ=MONTHLY;UNTIL=20270115", "FREQ=MONTHLY;UNTIL=20270211T000000Z"), "both end on 12 January");
        assert!(same("FREQ=SECONDLY;UNTIL=20261112T150500Z", "FREQ=SECONDLY;UNTIL=20261112T150500Z"));
        assert!(same("FREQ=MONTHLY", "FREQ=MONTHLY;BYMONTHDAY=12"));
        assert!(!same("FREQ=WEEKLY;BYDAY=TH", "FREQ=DAILY"));
        assert!(!same("FREQ=WEEKLY;INTERVAL=2;BYDAY=TH,SU;WKST=SU", "FREQ=WEEKLY;INTERVAL=2;BYDAY=TH,SU"), "every other week, the week start picks which Sunday");
        assert!(!same_repeat(Some(&rec("FREQ=DAILY")), &start, None, &start, Brussels));
    }
}
