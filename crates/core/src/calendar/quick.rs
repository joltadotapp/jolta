//! Times as people type them, for the editor's time fields now and quick
//! create later (C3 spec, section 7).

use chrono::NaiveTime;

/// A clock time typed in English, Dutch or French: "9", "930", "14:30",
/// "9.30", "2pm", "9u30", "9h", "noon", "midi". A bare hour from 1 to 7,
/// without minutes after a mark, am or pm, or a leading zero, is the
/// afternoon ("4" is 16:00, "130" is 13:30), as in quick create; "7u",
/// "7:00" and "07" stay in the morning.
pub fn time(text: &str) -> Option<NaiveTime> {
    let text: String = text.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
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

#[cfg(test)]
mod tests {
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
        for text in ["", "abc", "24:00", "9:60", "13pm", "0am", "12345", "9:5", "tomorrow", "9-10", ":30"] {
            assert_eq!(time(text), None, "{text:?}");
        }
    }
}
