//! Time zones: names from every source to IANA zones, and wall-clock times
//! to instants.

use std::str::FromStr;

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;

use super::{Bounds, Local, Moment, Span, Zone};

/// The IANA zone for a TZID: an IANA name, a Windows name as Graph and
/// Outlook use (`Romance Standard Time`), or a known alias.
pub fn zone(name: &str) -> Option<Tz> {
    let name = name.trim().trim_matches('"').trim();
    if name.eq_ignore_ascii_case("Z")
        || name.eq_ignore_ascii_case("UTC")
        || name.eq_ignore_ascii_case("GMT")
    {
        return Some(chrono_tz::UTC);
    }
    match calcard::common::timezone::Tz::from_str(name) {
        Ok(calcard::common::timezone::Tz::Tz(tz)) => Some(tz),
        _ => None,
    }
}

/// The zone the Mac is in: `TZ` when set, else the zone `/etc/localtime`
/// links to, else UTC.
pub fn here() -> Tz {
    let from_env = std::env::var("TZ").ok().and_then(|name| zone(name.trim_start_matches(':')));
    from_env
        .or_else(|| {
            let target = std::fs::read_link("/etc/localtime").ok()?;
            let target = target.to_str()?;
            zone(target.rsplit_once("zoneinfo/").map_or(target, |(_, name)| name))
        })
        .unwrap_or(chrono_tz::UTC)
}

/// The instant a wall-clock time names in `tz`. A time the clocks skip moves
/// forward by the gap (RFC 5545 section 3.3.5); a time that happens twice
/// means the first.
pub fn instant(wall: NaiveDateTime, tz: Tz) -> DateTime<Utc> {
    match tz.from_local_datetime(&wall) {
        LocalResult::Single(t) => t.with_timezone(&Utc),
        LocalResult::Ambiguous(a, b) => a.min(b).with_timezone(&Utc),
        LocalResult::None => {
            // RFC 5545 reads a skipped time with the offset in force before the gap.
            let before = tz
                .offset_from_utc_datetime(&(wall - Duration::days(1)))
                .fix();
            (wall - Duration::seconds(i64::from(before.local_minus_utc()))).and_utc()
        }
    }
}

/// The wall-clock time `t` shows in `zone`.
pub fn wall(t: DateTime<Utc>, zone: Zone, here: Tz) -> NaiveDateTime {
    t.with_timezone(&zone.or(here)).naive_local()
}

/// Days since 1970-01-01, as `Bounds::Days` counts them.
pub fn day_number(date: NaiveDate) -> i32 {
    (date - NaiveDate::default()).num_days() as i32
}

impl Zone {
    pub fn or(self, here: Tz) -> Tz {
        match self {
            Zone::Tz(tz) => tz,
            Zone::Floating => here,
        }
    }
}

impl Local {
    /// `here` is the zone the Mac is in, used for floating times.
    pub fn instant(&self, here: Tz) -> DateTime<Utc> {
        instant(self.wall, self.zone.or(here))
    }
}

impl Span {
    pub fn bounds(&self, here: Tz) -> Bounds {
        match self {
            Span::AllDay { start, end } => Bounds::Days {
                start: day_number(*start),
                end: day_number(*end),
            },
            Span::Timed { start, end } => Bounds::Utc {
                start: start.instant(here),
                end: end.instant(here),
            },
        }
    }

    pub fn start(&self) -> Moment {
        match self {
            Span::AllDay { start, .. } => Moment::Date(*start),
            Span::Timed { start, .. } => Moment::Time(*start),
        }
    }
}

impl Moment {
    /// Whether a RECURRENCE-ID or EXDATE names the occurrence starting at
    /// `start`: the same instant when both are times, even in different
    /// zones, and otherwise the same date on each one's own wall clock.
    pub fn names(&self, start: &Moment, here: Tz) -> bool {
        self.key(here).matches(&start.key(here))
    }

    pub(super) fn key(&self, here: Tz) -> MomentKey {
        match self {
            Moment::Date(day) => MomentKey {
                at: None,
                day: *day,
            },
            Moment::Time(local) => MomentKey {
                at: Some(local.instant(here)),
                day: local.wall.date(),
            },
        }
    }
}

/// A `Moment` reduced to what `Moment::names` compares, so a series can
/// match many ids without converting each start again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MomentKey {
    pub at: Option<DateTime<Utc>>,
    pub day: NaiveDate,
}

impl MomentKey {
    pub fn matches(&self, other: &MomentKey) -> bool {
        match (self.at, other.at) {
            (Some(a), Some(b)) => a == b,
            _ => self.day == other.day,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::{America, Asia, Europe};

    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        at(s).and_utc()
    }

    #[test]
    fn iana_windows_and_aliases() {
        assert_eq!(zone("Europe/Brussels"), Some(Europe::Brussels));
        assert_eq!(zone("Romance Standard Time"), Some(Europe::Paris));
        assert_eq!(zone("W. Europe Standard Time"), Some(Europe::Berlin));
        assert_eq!(zone("Eastern Standard Time"), Some(America::New_York));
        assert_eq!(zone("Tokyo Standard Time"), Some(Asia::Tokyo));
        assert_eq!(zone("UTC"), Some(chrono_tz::UTC));
        assert_eq!(zone("Z"), Some(chrono_tz::UTC));
        assert_eq!(zone("GMT"), Some(chrono_tz::UTC));
        assert_eq!(zone("\"Europe/Brussels\""), Some(Europe::Brussels));
        assert_eq!(zone("/Europe/Brussels"), Some(Europe::Brussels));
        assert_eq!(
            zone("/freeassociation.sourceforge.net/Tzfile/Europe/Brussels"),
            Some(Europe::Brussels)
        );
        assert_eq!(zone("Customized Time Zone"), None);
        assert_eq!(zone(""), None);
    }

    #[test]
    fn skipped_time_moves_forward_by_the_gap() {
        assert_eq!(
            instant(at("2027-03-28 02:30"), Europe::Brussels),
            utc("2027-03-28 01:30")
        );
        assert_eq!(
            instant(at("2027-03-28 03:30"), Europe::Brussels),
            utc("2027-03-28 01:30")
        );
        assert_eq!(
            instant(at("2027-03-14 02:30"), America::New_York),
            utc("2027-03-14 07:30")
        );
    }

    #[test]
    fn repeated_time_means_the_first() {
        assert_eq!(
            instant(at("2027-10-31 02:30"), Europe::Brussels),
            utc("2027-10-31 00:30")
        );
        assert_eq!(
            instant(at("2027-10-31 03:30"), Europe::Brussels),
            utc("2027-10-31 02:30")
        );
    }

    #[test]
    fn floating_uses_here() {
        let nine = Local {
            wall: at("2026-11-04 09:00"),
            zone: Zone::Floating,
        };
        assert_eq!(nine.instant(Europe::Brussels), utc("2026-11-04 08:00"));
        assert_eq!(nine.instant(Asia::Tokyo), utc("2026-11-04 00:00"));
        let zoned = Local {
            wall: at("2026-11-04 09:00"),
            zone: Zone::Tz(Europe::Brussels),
        };
        assert_eq!(zoned.instant(Asia::Tokyo), utc("2026-11-04 08:00"));
    }

    #[test]
    fn all_day_is_the_same_days_everywhere() {
        let day = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let armistice = Span::AllDay {
            start: day("2026-11-11"),
            end: day("2026-11-12"),
        };
        let expected = Bounds::Days {
            start: 20_768,
            end: 20_769,
        };
        assert_eq!(armistice.bounds(Asia::Tokyo), expected);
        assert_eq!(armistice.bounds(Europe::Brussels), expected);
        assert_eq!(day_number(day("1970-01-02")), 1);
    }

    #[test]
    fn timed_bounds_are_utc() {
        let span = Span::Timed {
            start: Local {
                wall: at("2026-11-04 09:00"),
                zone: Zone::Tz(Europe::Brussels),
            },
            end: Local {
                wall: at("2026-11-04 10:00"),
                zone: Zone::Tz(America::New_York),
            },
        };
        assert_eq!(
            span.bounds(Asia::Tokyo),
            Bounds::Utc {
                start: utc("2026-11-04 08:00"),
                end: utc("2026-11-04 15:00")
            }
        );
    }

    #[test]
    fn recurrence_ids_match_by_instant_or_date() {
        let brussels = Moment::Time(Local {
            wall: at("2026-11-04 09:00"),
            zone: Zone::Tz(Europe::Brussels),
        });
        let utc_id = Moment::Time(Local {
            wall: at("2026-11-04 08:00"),
            zone: Zone::Tz(chrono_tz::UTC),
        });
        let wrong = Moment::Time(Local {
            wall: at("2026-11-04 09:00"),
            zone: Zone::Tz(chrono_tz::UTC),
        });
        let date = Moment::Date(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap());
        let other_date = Moment::Date(NaiveDate::from_ymd_opt(2026, 11, 5).unwrap());
        assert!(utc_id.names(&brussels, Asia::Tokyo));
        assert!(!wrong.names(&brussels, Asia::Tokyo));
        assert!(date.names(&brussels, Asia::Tokyo));
        assert!(brussels.names(&date, Asia::Tokyo));
        assert!(!other_date.names(&brussels, Asia::Tokyo));
    }
}
