//! Snooze times typed in plain words: "tomorrow", "mon 9am", "in 2h",
//! "tonight", "next week", "3 dec", "2026-11-03 14:00".
//!
//! Times are local: the caller passes its UTC offset in seconds. Times
//! without a day mean the next time that clock time comes round.

const MORNING: i64 = 8 * 3600;
const EVENING: i64 = 19 * 3600;

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Seconds after midnight for "9", "9am", "9:30", "21:15", "9.30pm", "noon".
fn clock(word: &str) -> Option<i64> {
    match word {
        "noon" | "midday" => return Some(12 * 3600),
        "midnight" => return Some(0),
        _ => {}
    }
    let (digits, pm, am) = if let Some(d) = word.strip_suffix("pm") {
        (d, true, false)
    } else if let Some(d) = word.strip_suffix("am") {
        (d, false, true)
    } else {
        (word, false, false)
    };
    let (h, m) = match digits.split_once([':', '.', 'h']) {
        Some((h, m)) => (h.parse::<i64>().ok()?, if m.is_empty() { 0 } else { m.parse::<i64>().ok()? }),
        None => (digits.parse::<i64>().ok()?, 0),
    };
    if !(0..60).contains(&m) || h > 23 || ((am || pm) && !(1..=12).contains(&h)) {
        return None;
    }
    let h = match (am, pm) {
        (true, _) if h == 12 => 0,
        (_, true) if h < 12 => h + 12,
        _ => h,
    };
    Some(h * 3600 + m * 60)
}

/// Parses `text` relative to `now` (unix seconds). Returns the wake time in
/// unix seconds, always in the future, or `None` if the words make no sense.
pub fn parse(text: &str, now: i64, utc_offset: i32) -> Option<i64> {
    let offset = utc_offset as i64;
    let local_now = now + offset;
    let today = local_now.div_euclid(86_400);
    let now_secs = local_now.rem_euclid(86_400);
    let text = text.trim().to_lowercase();
    let words: Vec<&str> = text.split_whitespace().filter(|w| !matches!(*w, "at" | "on" | "the")).collect();
    if words.is_empty() {
        return None;
    }

    // "in 2 hours", "in 3d", "2h"
    let rest: &[&str] = if words[0] == "in" { &words[1..] } else { &words };
    if let Some(secs) = duration(rest) {
        return Some(now + secs);
    }

    let mut day: Option<i64> = None;
    let mut time: Option<i64> = None;
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        match w {
            "today" => day = Some(today),
            "tonight" => {
                day = Some(today);
                time.get_or_insert(EVENING);
            }
            "tomorrow" | "tmrw" | "tmr" => day = Some(today + 1),
            "morning" => {
                time = Some(MORNING);
            }
            "evening" => {
                time = Some(EVENING);
            }
            "weekend" => day = Some(next_weekday(today, 5, false)),
            "next" if words.get(i + 1) == Some(&"week") => {
                day = Some(next_weekday(today, 0, false));
                i += 1;
            }
            "next" if words.get(i + 1).is_some_and(|w| weekday(w).is_some()) => {
                let wd = weekday(words[i + 1]).expect("checked above");
                day = Some(next_weekday(today, wd, false) + 7 * i64::from(next_weekday(today, wd, false) - today < 7));
                i += 1;
            }
            "this" if words.get(i + 1) == Some(&"weekend") => {
                day = Some(next_weekday(today, 5, true));
                i += 1;
            }
            _ => {
                if let Some(wd) = weekday(w) {
                    day = Some(next_weekday(today, wd, false));
                } else if let Some((d, consumed)) = date(&words[i..], today) {
                    day = Some(d);
                    i += consumed - 1;
                } else if let Some(t) = clock(w) {
                    time = Some(t);
                } else {
                    return None;
                }
            }
        }
        i += 1;
    }

    let (day, time) = match (day, time) {
        (Some(d), Some(t)) => (d, t),
        (Some(d), None) => (d, MORNING),
        (None, Some(t)) => (if t > now_secs { today } else { today + 1 }, t),
        (None, None) => return None,
    };
    let at = day * 86_400 + time - offset;
    (at > now).then_some(at)
}

fn weekday(word: &str) -> Option<i64> {
    let w = word.get(..3)?;
    WEEKDAYS.iter().position(|d| *d == w).map(|i| i as i64)
}

/// The next day with weekday `wd` (0 = Monday) after `today`, or today itself
/// when `allow_today` and it matches.
fn next_weekday(today: i64, wd: i64, allow_today: bool) -> i64 {
    let current = (today + 3).rem_euclid(7);
    let mut ahead = (wd - current).rem_euclid(7);
    if ahead == 0 && !allow_today {
        ahead = 7;
    }
    today + ahead
}

fn duration(words: &[&str]) -> Option<i64> {
    let joined: String = words.concat();
    let split = joined.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = joined.split_at(split);
    let n: i64 = n.parse().ok()?;
    let secs = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "week" | "weeks" => 7 * 86_400,
        _ => return None,
    };
    Some(n * secs)
}

/// "3 dec", "dec 3", "2026-11-03", "3/12" (day/month). Returns the day and
/// how many words it used. Dates already past this year mean next year.
fn date(words: &[&str], today: i64) -> Option<(i64, usize)> {
    let (year, _, _) = civil_from_days(today);
    let month = |w: &str| w.get(..3).and_then(|m| MONTHS.iter().position(|x| *x == m)).map(|i| i as i64 + 1);
    let w0 = words[0];
    if let Some((y, rest)) = w0.split_once('-') {
        let (m, d) = rest.split_once('-')?;
        return Some((days_from_civil(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?), 1));
    }
    let pick = |m: i64, d: i64, used: usize| -> Option<(i64, usize)> {
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        let this_year = days_from_civil(year, m, d);
        Some((if this_year < today { days_from_civil(year + 1, m, d) } else { this_year }, used))
    };
    if let Some((d, m)) = w0.split_once('/') {
        return pick(m.parse().ok()?, d.parse().ok()?, 1);
    }
    if let (Ok(d), Some(m)) = (w0.trim_end_matches(['s', 't', 'n', 'd', 'r', 'h']).parse::<i64>(), words.get(1).and_then(|w| month(w))) {
        return pick(m, d, 2);
    }
    if let (Some(m), Some(Ok(d))) = (month(w0), words.get(1).map(|w| w.trim_end_matches(['s', 't', 'n', 'd', 'r', 'h']).parse::<i64>())) {
        return pick(m, d, 2);
    }
    None
}

/// The snooze presets, as (label, unix seconds), computed for `now`.
pub fn presets(now: i64, utc_offset: i32) -> Vec<(String, i64)> {
    let offset = utc_offset as i64;
    let local = now + offset;
    let today = local.div_euclid(86_400);
    let at = |day: i64, secs: i64| day * 86_400 + secs - offset;
    let mut out = vec![("Later today".to_string(), now + 3 * 3600)];
    if local.rem_euclid(86_400) < EVENING - 3600 {
        out.push(("This evening".into(), at(today, EVENING)));
    }
    out.push(("Tomorrow".into(), at(today + 1, MORNING)));
    out.push(("This weekend".into(), at(next_weekday(today, 5, false), MORNING)));
    out.push(("Next week".into(), at(next_weekday(today, 0, false), MORNING)));
    // On a Friday "This weekend" is tomorrow morning; list each time once.
    out.dedup_by(|later, earlier| later.1 == earlier.1);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Friday 30 October 2026, 14:00 in Brussels (UTC+1).
    const OFFSET: i32 = 3600;
    fn now() -> i64 {
        days_from_civil(2026, 10, 30) * 86_400 + 14 * 3600 - OFFSET as i64
    }
    fn local(y: i64, m: i64, d: i64, h: i64, min: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60 - OFFSET as i64
    }
    fn p(s: &str) -> Option<i64> {
        parse(s, now(), OFFSET)
    }

    #[test]
    fn days_and_times() {
        assert_eq!(p("tomorrow"), Some(local(2026, 10, 31, 8, 0)));
        assert_eq!(p("tomorrow 9am"), Some(local(2026, 10, 31, 9, 0)));
        assert_eq!(p("Tomorrow at 14:30"), Some(local(2026, 10, 31, 14, 30)));
        assert_eq!(p("tonight"), Some(local(2026, 10, 30, 19, 0)));
        assert_eq!(p("5pm"), Some(local(2026, 10, 30, 17, 0)));
        assert_eq!(p("9am"), Some(local(2026, 10, 31, 9, 0)));
        assert_eq!(p("mon"), Some(local(2026, 11, 2, 8, 0)));
        assert_eq!(p("Monday 10:15"), Some(local(2026, 11, 2, 10, 15)));
        assert_eq!(p("fri"), Some(local(2026, 11, 6, 8, 0)));
        assert_eq!(p("next week"), Some(local(2026, 11, 2, 8, 0)));
        assert_eq!(p("this weekend"), Some(local(2026, 10, 31, 8, 0)));
        assert_eq!(p("12pm"), Some(local(2026, 10, 31, 12, 0)));
    }

    #[test]
    fn durations_and_dates() {
        assert_eq!(p("in 2 hours"), Some(now() + 7200));
        assert_eq!(p("in 30m"), Some(now() + 1800));
        assert_eq!(p("3d"), Some(now() + 3 * 86_400));
        assert_eq!(p("3 dec"), Some(local(2026, 12, 3, 8, 0)));
        assert_eq!(p("dec 3rd 4pm"), Some(local(2026, 12, 3, 16, 0)));
        assert_eq!(p("2027-01-05 09:00"), Some(local(2027, 1, 5, 9, 0)));
        assert_eq!(p("1/2"), Some(local(2027, 2, 1, 8, 0)));
        assert_eq!(p("15 oct"), Some(local(2027, 10, 15, 8, 0)));
    }

    #[test]
    fn nonsense_and_the_past() {
        assert_eq!(p(""), None);
        assert_eq!(p("whenever"), None);
        assert_eq!(p("25:00"), None);
        assert_eq!(p("today 9am"), None);
        assert_eq!(p("13pm"), None);
    }

    #[test]
    fn presets_are_in_order_and_future() {
        let list = presets(now(), OFFSET);
        assert_eq!(list.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
                   ["Later today", "This evening", "Tomorrow", "Next week"]);
        assert!(list.windows(2).all(|w| w[0].1 < w[1].1));
    }
}
