//! Find a time: busy times of the user and other people, working hours,
//! and the free slots they leave. Everything here is pure; `freebusy`
//! fetches the busy times and the working hours.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::tz;

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
    /// The provider knows no such person or calendar.
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
            Unknown::NotFound => "Not found by your calendar provider".into(),
            Unknown::NotSupported => "This account's calendar can't look up other people yet".into(),
            Unknown::Failed(why) => format!("Couldn't be looked up: {why}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Availability {
    Known(Vec<Busy>),
    Unknown(Unknown),
}

/// One person's answer, by address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub person: String,
    pub availability: Availability,
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
/// on the working hours' clock.
pub fn suggest(own: &[Busy], others: &[Schedule], hours: &WorkingHours, duration: Duration, from: DateTime<Utc>, days: u32) -> Vec<Slot> {
    todo!()
}

/// The user's free stretches of at least `shortest` in the next `days`
/// working days after `from`, for Insert my availability. Stretch starts
/// fall on the half hour.
pub fn free_times(own: &[Busy], hours: &WorkingHours, from: DateTime<Utc>, days: u32, shortest: Duration) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    todo!()
}

/// "Tue 10 Nov: 10:00 to 11:30, 14:00 to 16:00", one line per day, on
/// `zone`'s clock.
pub fn availability_text(free: &[(DateTime<Utc>, DateTime<Utc>)], zone: Tz) -> String {
    todo!()
}

/// "Would Tuesday 10 November, 14:00 to 14:30 (CET) work for you?"
pub fn proposal_line(start: DateTime<Utc>, end: DateTime<Utc>, zone: Tz) -> String {
    todo!()
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
        Schedule { person: person.into(), availability: Availability::Known(busy) }
    }

    /// Slot starts as Brussels wall-clock text.
    fn starts(slots: &[Slot]) -> Vec<String> {
        slots.iter().map(|s| s.start.with_timezone(&Brussels).format("%a %H:%M").to_string()).collect()
    }

    fn hours() -> WorkingHours {
        WorkingHours::standard(Brussels)
    }

    // 2026-11-09 is a Monday.

    #[test]
    fn a_free_day_gives_one_slot_an_hour_inside_working_hours() {
        let slots = suggest(&[], &[], &hours(), Duration::minutes(30), at("2026-11-09 07:00"), 1);
        assert_eq!(starts(&slots), ["Mon 09:00", "Mon 10:00", "Mon 11:00", "Mon 12:00", "Mon 13:00", "Mon 14:00", "Mon 15:00", "Mon 16:00"]);
        assert_eq!(slots.last().unwrap().end, at("2026-11-09 16:30"));
        assert!(slots.iter().all(|s| s.missing.is_empty()));
    }

    #[test]
    fn past_times_today_are_skipped_and_starts_round_up() {
        let slots = suggest(&[], &[], &hours(), Duration::minutes(30), at("2026-11-09 10:07"), 1);
        assert_eq!(starts(&slots)[0], "Mon 10:30", "a half-hour meeting starts on the half hour");
        let slots = suggest(&[], &[], &hours(), Duration::minutes(15), at("2026-11-09 10:07"), 1);
        assert_eq!(starts(&slots)[0], "Mon 10:15", "a quarter-hour meeting starts on the quarter hour");
        let slots = suggest(&[], &[], &hours(), Duration::minutes(30), at("2026-11-09 16:45"), 1);
        assert_eq!(starts(&slots), ["Tue 09:00"], "a day whose working time is too short for the meeting gives nothing");
    }

    #[test]
    fn overlapping_blocks_of_several_people_leave_only_what_all_have_free() {
        let own = [busy("2026-11-09 09:00", "2026-11-09 10:00", BusyKind::Busy)];
        let others = [
            known("ann@example.com", vec![busy("2026-11-09 09:30", "2026-11-09 11:00", BusyKind::Busy)]),
            known("bo@example.com", vec![busy("2026-11-09 10:30", "2026-11-09 11:15", BusyKind::Tentative), busy("2026-11-09 12:00", "2026-11-09 17:00", BusyKind::OutOfOffice)]),
        ];
        let slots = suggest(&own, &others, &hours(), Duration::minutes(30), at("2026-11-09 07:00"), 1);
        assert_eq!(starts(&slots), ["Mon 11:30"], "11:15 rounds to 11:30, and 12:00 is taken");
    }

    #[test]
    fn working_elsewhere_leaves_the_time_free() {
        let others = [known("ann@example.com", vec![busy("2026-11-09 09:00", "2026-11-09 17:00", BusyKind::Elsewhere)])];
        let slots = suggest(&[], &others, &hours(), Duration::minutes(60), at("2026-11-09 07:00"), 1);
        assert_eq!(starts(&slots)[0], "Mon 09:00");
    }

    #[test]
    fn a_block_over_midnight_takes_the_next_morning() {
        let others = [known("ann@example.com", vec![busy("2026-11-09 16:00", "2026-11-10 10:00", BusyKind::Busy)])];
        let slots = suggest(&[], &others, &hours(), Duration::minutes(60), at("2026-11-09 14:00"), 2);
        assert_eq!(starts(&slots), ["Mon 14:00", "Tue 10:00", "Tue 11:00", "Tue 12:00", "Tue 13:00", "Tue 14:00", "Tue 15:00", "Tue 16:00"]);
    }

    #[test]
    fn people_not_known_are_listed_in_every_slot_and_never_block_or_free_it() {
        let others = [
            known("ann@example.com", vec![busy("2026-11-09 09:00", "2026-11-09 16:00", BusyKind::Busy)]),
            Schedule { person: "bo@example.com".into(), availability: Availability::Unknown(Unknown::NotShared) },
        ];
        let slots = suggest(&[], &others, &hours(), Duration::minutes(30), at("2026-11-09 07:00"), 1);
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
        let slots = suggest(&[], &[], &every_day, Duration::minutes(30), at("2026-10-25 08:10"), 1);
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
            availability_text(&free, Brussels),
            "Tue 10 Nov: 10:00 to 11:30, 14:00 to 16:00, 16:30 to 17:00\nThu 12 Nov: 09:00 to 17:00"
        );
    }

    #[test]
    fn the_proposal_names_the_day_the_time_and_the_zone() {
        assert_eq!(
            proposal_line(at("2026-11-10 14:00"), at("2026-11-10 14:30"), Brussels),
            "Would Tuesday 10 November, 14:00 to 14:30 (CET) work for you?"
        );
    }
}
