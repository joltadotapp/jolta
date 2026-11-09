//! Find a time: busy times of the user and other people, working hours,
//! and the free slots they leave. Everything here is pure; `freebusy`
//! fetches the busy times and the working hours.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Offset, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::{tz, zones};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BusyKind {
    Busy,
    Tentative,
    OutOfOffice,
    /// Working, somewhere else. Outlook shows the person as available.
    Elsewhere,
}

impl BusyKind {
    /// Whether a slot over this time is taken.
    pub fn blocks(self) -> bool {
        self != BusyKind::Elsewhere
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Busy {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub kind: BusyKind,
}

/// Why a person's busy times aren't known. Unknown is never free.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Unknown {
    /// The person doesn't share free/busy with the user.
    NotShared,
    /// The provider can't see the person's calendar: someone outside the
    /// organization who shares nothing, or a mistyped address.
    NotFound,
    /// The account's provider has no way to ask (CalDAV, for now).
    NotSupported,
    Failed(String),
}

impl Unknown {
    /// For the panel's hover and the CLI.
    pub fn reason(&self) -> String {
        match self {
            Unknown::NotShared => "Doesn't share free/busy with you".into(),
            Unknown::NotFound => "Outside your organization, or doesn't share free/busy with you".into(),
            Unknown::NotSupported => "This account's calendar can't look up other people yet".into(),
            Unknown::Failed(why) => format!("Couldn't be looked up: {why}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Availability {
    Known(Vec<Busy>),
    Unknown(Unknown),
}

/// One person's answer, by address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub person: String,
    pub availability: Availability,
    /// The person's working hours, when the provider says: Microsoft 365
    /// gives them with the busy times, Google never does.
    #[serde(default)]
    pub hours: Option<WorkingHours>,
}

impl Schedule {
    pub fn new(person: impl Into<String>, availability: Availability) -> Schedule {
        Schedule { person: person.into(), availability, hours: None }
    }
}

/// Days of the week, Monday as bit 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Weekdays(pub u8);

impl Weekdays {
    pub const MONDAY_TO_FRIDAY: Weekdays = Weekdays(0b0001_1111);
    pub const EVERY_DAY: Weekdays = Weekdays(0b0111_1111);

    pub fn of(days: impl IntoIterator<Item = Weekday>) -> Weekdays {
        Weekdays(days.into_iter().fold(0, |bits, d| bits | 1 << d.num_days_from_monday()))
    }

    pub fn has(self, day: Weekday) -> bool {
        self.0 & 1 << day.num_days_from_monday() != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingHours {
    pub days: Weekdays,
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub zone: Tz,
}

/// One working day placed on the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub day: NaiveDate,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl WorkingHours {
    /// Monday to Friday, 09:00 to 17:00 in `zone`.
    pub fn standard(zone: Tz) -> WorkingHours {
        let at = |h| NaiveTime::from_hms_opt(h, 0, 0).expect("a valid hour");
        WorkingHours { days: Weekdays::MONDAY_TO_FRIDAY, start: at(9), end: at(17), zone }
    }

    /// The next `days` working days that still have working time after
    /// `from`, each from its start to its end on the zone's clock.
    pub fn windows(&self, from: DateTime<Utc>, days: u32) -> Vec<Window> {
        let mut out = Vec::new();
        if self.end <= self.start || self.days.0 & Weekdays::EVERY_DAY.0 == 0 {
            return out;
        }
        let mut day = from.with_timezone(&self.zone).date_naive();
        while out.len() < days as usize {
            if self.days.has(day.weekday()) {
                let (start, end) = (tz::instant(day.and_time(self.start), self.zone), tz::instant(day.and_time(self.end), self.zone));
                if end > from {
                    out.push(Window { day, start, end });
                }
            }
            day = day.succ_opt().expect("a day before the end of time");
        }
        out
    }

    /// The time from `start` to `end` outside these hours, on their clock.
    pub fn off(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        let mut out = Vec::new();
        let mut at = start;
        let mut day = start.with_timezone(&self.zone).date_naive().pred_opt().expect("a day after the start of time");
        while at < end {
            let Some(next) = day.succ_opt() else { break };
            if self.days.has(day.weekday()) && self.start < self.end {
                let (s, e) = (tz::instant(day.and_time(self.start), self.zone), tz::instant(day.and_time(self.end), self.zone));
                if e > at {
                    if s > at {
                        out.push((at, s.min(end)));
                    }
                    at = at.max(e);
                }
            }
            if tz::instant(next.and_time(NaiveTime::MIN), self.zone) >= end {
                break;
            }
            day = next;
        }
        if at < end {
            out.push((at, end));
        }
        out
    }
}

/// What `suggest` takes into account besides busy times.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuggestOptions {
    /// A slot must also fit inside each known person's working hours.
    pub respect_others_hours: bool,
}

/// A time everyone known is free. `missing` names the people whose busy
/// times aren't known, so the slot may not suit them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub missing: Vec<String>,
}

/// Suggested slots of `duration` in the next `days` working days after
/// `from`: the first start in each free stretch, then one every hour (or
/// every `duration`, when longer) while the stretch lasts. Starts fall on
/// the quarter hour for meetings under 30 minutes, else on the half hour,
/// on the working hours' clock. `hours` are the user's and set the days;
/// other people's hours narrow them when `options` says so.
pub fn suggest(own: &[Busy], others: &[Schedule], hours: &WorkingHours, duration: Duration, from: DateTime<Utc>, days: u32, options: SuggestOptions) -> Vec<Slot> {
    if duration <= Duration::zero() {
        return Vec::new();
    }
    let missing: Vec<String> = others
        .iter()
        .filter(|s| matches!(s.availability, Availability::Unknown(_)))
        .map(|s| s.person.clone())
        .collect();
    let known = others.iter().filter_map(|s| match &s.availability {
        Availability::Known(busy) => Some(busy.as_slice()),
        Availability::Unknown(_) => None,
    });
    let mut taken: Vec<Busy> = std::iter::once(own).chain(known).flatten().copied().collect();
    if options.respect_others_hours {
        let windows = hours.windows(from, days);
        if let (Some(first), Some(last)) = (windows.first(), windows.last()) {
            let theirs = others.iter().filter(|s| matches!(s.availability, Availability::Known(_))).filter_map(|s| s.hours);
            for (start, end) in theirs.flat_map(|h| h.off(first.start, last.end)) {
                taken.push(Busy { start, end, kind: BusyKind::Busy });
            }
        }
    }
    let taken = merged(&taken);
    let step = if duration < Duration::minutes(30) { 15 } else { 30 };
    let every = duration.max(Duration::hours(1));
    let mut out = Vec::new();
    for (gap_start, gap_end) in gaps(&taken, hours, from, days) {
        let mut start = round_up(gap_start, step, hours.zone);
        while start + duration <= gap_end {
            out.push(Slot { start, end: start + duration, missing: missing.clone() });
            start = round_up(start + every, step, hours.zone);
        }
    }
    out
}

/// The user's free stretches of at least `shortest` in the next `days`
/// working days after `from`, for Insert my availability. Stretch starts
/// fall on the half hour.
pub fn free_times(own: &[Busy], hours: &WorkingHours, from: DateTime<Utc>, days: u32, shortest: Duration) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    gaps(&merged(own), hours, from, days)
        .into_iter()
        .map(|(start, end)| (round_up(start, 30, hours.zone), end))
        .filter(|(start, end)| *end - *start >= shortest)
        .collect()
}

/// The user's free stretches as text for a mail, on `theirs`' clock: one
/// line per day, named with that day's abbreviation, as in "Tue 10 Nov
/// (EST): 10:00 to 11:30, 14:00 to 16:00". Where their clock differs from
/// the user's (`mine`) the heading names their zone and each stretch adds
/// its start on the user's clock: "10:00 to 10:30 (16:00 Brussels)".
pub fn availability_text(free: &[(DateTime<Utc>, DateTime<Utc>)], theirs: Tz, mine: Tz) -> String {
    let mut lines: Vec<(NaiveDate, String, Vec<String>)> = Vec::new();
    let mut differs = false;
    for (start, end) in free {
        let (s, e, m) = (start.with_timezone(&theirs), end.with_timezone(&theirs), start.with_timezone(&mine));
        let end_text = if e.date_naive() == s.date_naive() { e.format("%H:%M") } else { e.format("%a %H:%M") };
        let mut span = format!("{} to {end_text}", s.format("%H:%M"));
        if m.offset().fix() != s.offset().fix() {
            differs = true;
            let day = if m.date_naive() == s.date_naive() { String::new() } else { m.format("%a ").to_string() };
            span.push_str(&format!(" ({day}{} {})", m.format("%H:%M"), zones::city(mine)));
        }
        match lines.last_mut() {
            Some((day, _, spans)) if *day == s.date_naive() => spans.push(span),
            _ => lines.push((s.date_naive(), s.format("%Z").to_string(), vec![span])),
        }
    }
    let heading = if differs { format!("I'm free at these times ({} time):", zones::city(theirs)) } else { "I'm free at these times:".to_string() };
    let days = lines.iter().map(|(day, abbr, spans)| format!("{} ({abbr}): {}", day.format("%a %-d %b"), spans.join(", ")));
    std::iter::once(heading).chain(days).collect::<Vec<_>>().join("\n")
}

/// "Would Tuesday 10 November, 14:00 to 14:30 (CET) work for you?"
pub fn proposal_line(start: DateTime<Utc>, end: DateTime<Utc>, zone: Tz) -> String {
    let (s, e) = (start.with_timezone(&zone), end.with_timezone(&zone));
    let end_text = if e.date_naive() == s.date_naive() { e.format("%H:%M").to_string() } else { e.format("%A %-d %B, %H:%M").to_string() };
    format!("Would {}, {} to {end_text} ({}) work for you?", s.format("%A %-d %B"), s.format("%H:%M"), s.format("%Z"))
}

/// The line under a person's name in Find a time: the shown days they are
/// out of office ("Out of office Thu, Fri"), else their working hours on
/// their own clock when those differ from the user's ("Works 09:00 to
/// 15:00", with their city when their clock differs).
pub fn note(schedule: &Schedule, windows: &[Window], mine: &WorkingHours) -> Option<String> {
    if let Availability::Known(busy) = &schedule.availability {
        let away: Vec<usize> = windows
            .iter()
            .enumerate()
            .filter(|(_, w)| busy.iter().any(|b| b.kind == BusyKind::OutOfOffice && b.start < w.end && b.end > w.start))
            .map(|(i, _)| i)
            .collect();
        if !away.is_empty() {
            return Some(format!("Out of office {}", day_runs(&away, windows)));
        }
    }
    let theirs = schedule.hours?;
    let at = windows.first().map_or_else(Utc::now, |w| w.start).naive_utc();
    let offset = |zone: Tz| zone.offset_from_utc_datetime(&at).fix();
    let same_clock = offset(theirs.zone) == offset(mine.zone);
    if same_clock && (theirs.start, theirs.end) == (mine.start, mine.end) {
        return None;
    }
    let times = format!("Works {} to {}", theirs.start.format("%H:%M"), theirs.end.format("%H:%M"));
    Some(if same_clock { times } else { format!("{times} in {}", zones::city(theirs.zone)) })
}

/// "Mon", "Thu, Fri" or "Mon to Wed": runs of neighbouring shown days.
fn day_runs(indices: &[usize], windows: &[Window]) -> String {
    let name = |i: usize| windows[i].day.format("%a").to_string();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &i in indices {
        match runs.last_mut() {
            Some(run) if run.1 + 1 == i => run.1 = i,
            _ => runs.push((i, i)),
        }
    }
    let text = |&(a, b): &(usize, usize)| match b - a {
        0 => name(a),
        1 => format!("{}, {}", name(a), name(b)),
        _ => format!("{} to {}", name(a), name(b)),
    };
    runs.iter().map(text).collect::<Vec<_>>().join(", ")
}

/// The blocking times, sorted and joined where they touch or overlap.
fn merged<'a>(busy: impl IntoIterator<Item = &'a Busy>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut spans: Vec<_> = busy.into_iter().filter(|b| b.kind.blocks() && b.end > b.start).map(|b| (b.start, b.end)).collect();
    spans.sort();
    let mut out: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for (start, end) in spans {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// The working time after `from` that `taken` leaves.
fn gaps(taken: &[(DateTime<Utc>, DateTime<Utc>)], hours: &WorkingHours, from: DateTime<Utc>, days: u32) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    for w in hours.windows(from, days) {
        let mut at = w.start.max(from);
        for &(start, end) in taken.iter().filter(|(s, e)| *e > w.start && *s < w.end) {
            if start > at {
                out.push((at, start.min(w.end)));
            }
            at = at.max(end);
        }
        if at < w.end {
            out.push((at, w.end));
        }
    }
    out
}

/// The first time at or after `t` whose minute on `zone`'s clock is a
/// multiple of `step`.
fn round_up(t: DateTime<Utc>, step: u32, zone: Tz) -> DateTime<Utc> {
    let local = t.with_timezone(&zone).naive_local();
    let past = local.minute() % step;
    if past == 0 && local.second() == 0 && local.nanosecond() == 0 {
        return t;
    }
    let floor = local.with_second(0).and_then(|l| l.with_nanosecond(0)).expect("a valid time") - Duration::minutes(past.into());
    let wall = floor + Duration::minutes(step.into());
    tz::instant(wall, zone).max(t)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;

    /// A wall-clock time in Brussels.
    fn at(s: &str) -> DateTime<Utc> {
        let wall = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        Brussels.from_local_datetime(&wall).earliest().unwrap().with_timezone(&Utc)
    }

    fn busy(start: &str, end: &str, kind: BusyKind) -> Busy {
        Busy { start: at(start), end: at(end), kind }
    }

    fn known(person: &str, busy: Vec<Busy>) -> Schedule {
        Schedule::new(person, Availability::Known(busy))
    }

    /// Slot starts as Brussels wall-clock text.
    fn starts(slots: &[Slot]) -> Vec<String> {
        slots.iter().map(|s| s.start.with_timezone(&Brussels).format("%a %H:%M").to_string()).collect()
    }

    fn hours() -> WorkingHours {
        WorkingHours::standard(Brussels)
    }

    const ANY_HOURS: SuggestOptions = SuggestOptions { respect_others_hours: false };
    const THEIR_HOURS: SuggestOptions = SuggestOptions { respect_others_hours: true };

    fn clock(h: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, 0, 0).unwrap()
    }

    /// Ann works 09:00 to 15:00 in New York.
    fn ann_in_new_york(busy: Vec<Busy>) -> Schedule {
        let hours = WorkingHours { start: clock(9), end: clock(15), ..WorkingHours::standard(chrono_tz::America::New_York) };
        Schedule { hours: Some(hours), ..known("ann@example.com", busy) }
    }

    // 2026-11-09 is a Monday.

    #[test]
    fn a_free_day_gives_one_slot_an_hour_inside_working_hours() {
        let slots = suggest(&[], &[], &hours(), Duration::minutes(30), at("2026-11-09 07:00"), 1, ANY_HOURS);
        assert_eq!(starts(&slots), ["Mon 09:00", "Mon 10:00", "Mon 11:00", "Mon 12:00", "Mon 13:00", "Mon 14:00", "Mon 15:00", "Mon 16:00"]);
        assert_eq!(slots.last().unwrap().end, at("2026-11-09 16:30"));
        assert!(slots.iter().all(|s| s.missing.is_empty()));
    }

    #[test]
    fn past_times_today_are_skipped_and_starts_round_up() {
        let slots = suggest(&[], &[], &hours(), Duration::minutes(30), at("2026-11-09 10:07"), 1, ANY_HOURS);
        assert_eq!(starts(&slots)[0], "Mon 10:30", "a half-hour meeting starts on the half hour");
        let slots = suggest(&[], &[], &hours(), Duration::minutes(15), at("2026-11-09 10:07"), 1, ANY_HOURS);
        assert_eq!(starts(&slots)[0], "Mon 10:15", "a quarter-hour meeting starts on the quarter hour");
        let slots = suggest(&[], &[], &hours(), Duration::minutes(30), at("2026-11-09 16:45"), 2, ANY_HOURS);
        assert_eq!(starts(&slots)[0], "Tue 09:00", "the quarter hour left today is too short for the meeting");
    }

    #[test]
    fn overlapping_blocks_of_several_people_leave_only_what_all_have_free() {
        let own = [busy("2026-11-09 09:00", "2026-11-09 10:00", BusyKind::Busy)];
        let others = [
            known("ann@example.com", vec![busy("2026-11-09 09:30", "2026-11-09 11:00", BusyKind::Busy)]),
            known("bo@example.com", vec![busy("2026-11-09 10:30", "2026-11-09 11:15", BusyKind::Tentative), busy("2026-11-09 12:00", "2026-11-09 17:00", BusyKind::OutOfOffice)]),
        ];
        let slots = suggest(&own, &others, &hours(), Duration::minutes(30), at("2026-11-09 07:00"), 1, ANY_HOURS);
        assert_eq!(starts(&slots), ["Mon 11:30"], "11:15 rounds to 11:30, and 12:00 is taken");
    }

    #[test]
    fn working_elsewhere_leaves_the_time_free() {
        let others = [known("ann@example.com", vec![busy("2026-11-09 09:00", "2026-11-09 17:00", BusyKind::Elsewhere)])];
        let slots = suggest(&[], &others, &hours(), Duration::minutes(60), at("2026-11-09 07:00"), 1, ANY_HOURS);
        assert_eq!(starts(&slots)[0], "Mon 09:00");
    }

    #[test]
    fn a_block_over_midnight_takes_the_next_morning() {
        let others = [known("ann@example.com", vec![busy("2026-11-09 16:00", "2026-11-10 10:00", BusyKind::Busy)])];
        let slots = suggest(&[], &others, &hours(), Duration::minutes(60), at("2026-11-09 14:00"), 2, ANY_HOURS);
        assert_eq!(starts(&slots), ["Mon 14:00", "Mon 15:00", "Tue 10:00", "Tue 11:00", "Tue 12:00", "Tue 13:00", "Tue 14:00", "Tue 15:00", "Tue 16:00"]);
    }

    #[test]
    fn people_not_known_are_listed_in_every_slot_and_never_block_or_free_it() {
        let others = [
            known("ann@example.com", vec![busy("2026-11-09 09:00", "2026-11-09 16:00", BusyKind::Busy)]),
            Schedule::new("bo@example.com", Availability::Unknown(Unknown::NotShared)),
        ];
        let slots = suggest(&[], &others, &hours(), Duration::minutes(30), at("2026-11-09 07:00"), 1, ANY_HOURS);
        assert_eq!(starts(&slots), ["Mon 16:00"]);
        assert_eq!(slots[0].missing, ["bo@example.com"]);
    }

    #[test]
    fn working_hours_follow_the_clock_change() {
        // Brussels leaves summer time on Sunday 25 October 2026.
        let every_day = WorkingHours { days: Weekdays::EVERY_DAY, ..hours() };
        let windows = every_day.windows(at("2026-10-24 00:00"), 2);
        let utc = |w: &Window| w.start.format("%a %H:%M UTC").to_string();
        assert_eq!(windows.iter().map(utc).collect::<Vec<_>>(), ["Sat 07:00 UTC", "Sun 08:00 UTC"]);
        let slots = suggest(&[], &[], &every_day, Duration::minutes(30), at("2026-10-25 08:10"), 1, ANY_HOURS);
        assert_eq!(starts(&slots)[0], "Sun 09:00", "rounding uses the clock after the change");
    }

    #[test]
    fn days_count_only_working_days_with_time_left() {
        // Friday 13 November after work: Monday and Tuesday are next.
        let windows = hours().windows(at("2026-11-13 18:00"), 2);
        assert_eq!(windows.iter().map(|w| w.day.to_string()).collect::<Vec<_>>(), ["2026-11-16", "2026-11-17"]);
        assert!(WorkingHours { days: Weekdays(0), ..hours() }.windows(at("2026-11-13 18:00"), 2).is_empty());
    }

    #[test]
    fn availability_lists_free_stretches_by_day() {
        let own = [
            busy("2026-11-10 09:00", "2026-11-10 10:00", BusyKind::Busy),
            busy("2026-11-10 11:30", "2026-11-10 14:00", BusyKind::Tentative),
            busy("2026-11-10 16:00", "2026-11-10 16:20", BusyKind::Busy),
            busy("2026-11-11 09:00", "2026-11-11 17:00", BusyKind::OutOfOffice),
        ];
        let free = free_times(&own, &hours(), at("2026-11-10 07:00"), 3, Duration::minutes(30));
        assert_eq!(
            availability_text(&free, Brussels, Brussels),
            "I'm free at these times:\nTue 10 Nov (CET): 10:00 to 11:30, 14:00 to 16:00, 16:30 to 17:00\nThu 12 Nov (CET): 09:00 to 17:00"
        );
    }

    #[test]
    fn availability_names_each_days_own_zone() {
        // Brussels leaves summer time on Sunday 25 October 2026.
        let free = free_times(&[], &hours(), at("2026-10-23 07:00"), 2, Duration::minutes(30));
        assert_eq!(availability_text(&free, Brussels, Brussels), "I'm free at these times:\nFri 23 Oct (CEST): 09:00 to 17:00\nMon 26 Oct (CET): 09:00 to 17:00");
    }

    /// Brussels wall-clock stretches as instants.
    fn stretches(spans: &[(&str, &str)]) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        spans.iter().map(|(s, e)| (at(s), at(e))).collect()
    }

    #[test]
    fn availability_on_their_clock_gives_mine_in_parentheses() {
        let free = stretches(&[("2026-11-10 16:00", "2026-11-10 16:30"), ("2026-11-10 16:45", "2026-11-10 17:00"), ("2026-11-12 16:30", "2026-11-12 17:00")]);
        assert_eq!(
            availability_text(&free, chrono_tz::America::New_York, Brussels),
            "I'm free at these times (New York time):\nTue 10 Nov (EST): 10:00 to 10:30 (16:00 Brussels), 10:45 to 11:00 (16:45 Brussels)\nThu 12 Nov (EST): 10:30 to 11:00 (16:30 Brussels)"
        );
    }

    #[test]
    fn availability_in_a_zone_on_my_clock_has_no_parentheses() {
        let free = stretches(&[("2026-11-10 10:00", "2026-11-10 11:00")]);
        let mine = availability_text(&free, Brussels, Brussels);
        assert_eq!(availability_text(&free, chrono_tz::Europe::Paris, Brussels), mine, "Paris keeps Brussels' clock");
        assert_eq!(mine, "I'm free at these times:\nTue 10 Nov (CET): 10:00 to 11:00");
    }

    #[test]
    fn availability_follows_each_clock_change_between_the_days() {
        // Brussels left summer time on 25 October 2026 and New York on 1
        // November, so they were 5 hours apart that week and 6 after it.
        let autumn = stretches(&[("2026-10-23 16:00", "2026-10-23 17:00"), ("2026-10-27 15:00", "2026-10-27 16:00"), ("2026-11-03 16:00", "2026-11-03 17:00")]);
        assert_eq!(
            availability_text(&autumn, chrono_tz::America::New_York, Brussels),
            "I'm free at these times (New York time):\nFri 23 Oct (EDT): 10:00 to 11:00 (16:00 Brussels)\nTue 27 Oct (EDT): 10:00 to 11:00 (15:00 Brussels)\nTue 3 Nov (EST): 10:00 to 11:00 (16:00 Brussels)"
        );
        // New York starts summer time on 14 March 2027 and Brussels on 28 March.
        let spring = stretches(&[("2027-03-12 16:00", "2027-03-12 17:00"), ("2027-03-16 15:00", "2027-03-16 16:00"), ("2027-03-30 16:00", "2027-03-30 17:00")]);
        assert_eq!(
            availability_text(&spring, chrono_tz::America::New_York, Brussels),
            "I'm free at these times (New York time):\nFri 12 Mar (EST): 10:00 to 11:00 (16:00 Brussels)\nTue 16 Mar (EDT): 10:00 to 11:00 (15:00 Brussels)\nTue 30 Mar (EDT): 10:00 to 11:00 (16:00 Brussels)"
        );
    }

    #[test]
    fn availability_south_of_the_equator_changes_clocks_the_other_way() {
        // Sydney is 10 hours ahead of Brussels until Brussels starts summer
        // time on 28 March 2027, 9 hours until Sydney ends its own on 4
        // April, and 8 after. Their morning is the user's evening before.
        let free = stretches(&[("2027-03-25 22:00", "2027-03-25 23:00"), ("2027-03-30 23:00", "2027-03-31 00:00"), ("2027-04-06 09:00", "2027-04-06 17:00")]);
        assert_eq!(
            availability_text(&free, chrono_tz::Australia::Sydney, Brussels),
            "I'm free at these times (Sydney time):\nFri 26 Mar (AEDT): 08:00 to 09:00 (Thu 22:00 Brussels)\nWed 31 Mar (AEDT): 08:00 to 09:00 (Tue 23:00 Brussels)\nTue 6 Apr (AEST): 17:00 to Wed 01:00 (09:00 Brussels)"
        );
    }

    #[test]
    fn the_proposal_names_the_day_the_time_and_the_zone() {
        assert_eq!(
            proposal_line(at("2026-11-10 14:00"), at("2026-11-10 14:30"), Brussels),
            "Would Tuesday 10 November, 14:00 to 14:30 (CET) work for you?"
        );
    }

    #[test]
    fn slots_fit_inside_each_known_persons_working_hours() {
        // New York is 6 hours behind Brussels on Tuesday 10 November, so Ann's
        // day is 15:00 to 21:00 in Brussels and overlaps the user's from 15:00 to 17:00.
        let others = [ann_in_new_york(Vec::new())];
        let slots = suggest(&[], &others, &hours(), Duration::minutes(30), at("2026-11-10 07:00"), 1, THEIR_HOURS);
        assert_eq!(starts(&slots), ["Tue 15:00", "Tue 16:00"]);
        let slots = suggest(&[], &others, &hours(), Duration::minutes(30), at("2026-11-10 07:00"), 1, ANY_HOURS);
        assert_eq!(starts(&slots)[0], "Tue 09:00", "turned off, their hours narrow nothing");
    }

    #[test]
    fn a_person_without_hours_or_not_known_constrains_nothing() {
        let without = known("bo@example.com", Vec::new());
        let unknown = Schedule { hours: ann_in_new_york(Vec::new()).hours, ..Schedule::new("cy@example.com", Availability::Unknown(Unknown::NotShared)) };
        let slots = suggest(&[], &[without, unknown], &hours(), Duration::minutes(30), at("2026-11-10 07:00"), 1, THEIR_HOURS);
        assert_eq!(starts(&slots).len(), 8, "every hour from 09:00 to 16:00");
        assert_eq!(slots[0].missing, ["cy@example.com"]);
    }

    #[test]
    fn time_off_their_hours_covers_nights_weekends_and_the_clock_change() {
        let fmt = |spans: Vec<(DateTime<Utc>, DateTime<Utc>)>| {
            spans.iter().map(|(s, e)| format!("{} to {}", s.with_timezone(&Brussels).format("%a %H:%M"), e.with_timezone(&Brussels).format("%a %H:%M"))).collect::<Vec<_>>()
        };
        assert_eq!(fmt(hours().off(at("2026-11-13 07:00"), at("2026-11-16 18:00"))), ["Fri 07:00 to Fri 09:00", "Fri 17:00 to Mon 09:00", "Mon 17:00 to Mon 18:00"]);
        assert_eq!(fmt(hours().off(at("2026-11-13 10:00"), at("2026-11-13 12:00"))), Vec::<String>::new(), "inside working hours");
        let ann = ann_in_new_york(Vec::new()).hours.unwrap();
        assert_eq!(fmt(ann.off(at("2026-11-10 09:00"), at("2026-11-10 17:00"))), ["Tue 09:00 to Tue 15:00"]);
        // New York leaves summer time on 1 November 2026, a week after Brussels.
        assert_eq!(fmt(ann.off(at("2026-10-27 09:00"), at("2026-10-27 17:00"))), ["Tue 09:00 to Tue 14:00"], "5 hours apart that week");
    }

    #[test]
    fn the_note_names_out_of_office_days_else_hours_that_differ() {
        let windows = hours().windows(at("2026-11-09 07:00"), 5);
        let away = |days: &[u32]| days.iter().map(|d| busy(&format!("2026-11-{d} 00:00"), &format!("2026-11-{} 00:00", d + 1), BusyKind::OutOfOffice)).collect::<Vec<_>>();
        let note_of = |s: Schedule| note(&s, &windows, &hours());
        assert_eq!(note_of(known("bo@example.com", away(&[13]))).as_deref(), Some("Out of office Fri"));
        assert_eq!(note_of(known("bo@example.com", away(&[9, 12, 13]))).as_deref(), Some("Out of office Mon, Thu, Fri"));
        assert_eq!(note_of(known("bo@example.com", away(&[10, 11, 12]))).as_deref(), Some("Out of office Tue to Thu"));
        assert_eq!(note_of(ann_in_new_york(away(&[11]))).as_deref(), Some("Out of office Wed"), "out of office before working hours");
        assert_eq!(note_of(ann_in_new_york(Vec::new())).as_deref(), Some("Works 09:00 to 15:00 in New York"));
        let short = WorkingHours { end: clock(15), ..hours() };
        assert_eq!(note_of(Schedule { hours: Some(short), ..known("jan@example.com", Vec::new()) }).as_deref(), Some("Works 09:00 to 15:00"));
        let same = WorkingHours { days: Weekdays::of([Weekday::Mon]), ..WorkingHours::standard(chrono_tz::Europe::Paris) };
        assert_eq!(note_of(Schedule { hours: Some(same), ..known("jan@example.com", Vec::new()) }), None, "the same clock and times; the shading shows the days");
        assert_eq!(note_of(known("bo@example.com", Vec::new())), None);
    }
}
