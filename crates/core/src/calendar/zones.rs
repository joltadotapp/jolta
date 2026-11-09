//! The zone a person's clock is in, for Insert my availability: a zone the
//! user picked for them, else their Microsoft 365 working hours' zone, else
//! a zone with the offset in the `Date` header of their newest mail.

use chrono::{DateTime, FixedOffset, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use mail_parser::MessageParser;
use rusqlite::Connection;

use super::availability::WorkingHours;
use super::freebusy;
use crate::blobs::Blobs;
use crate::{AccountId, Jolta, Result, store};

/// Where a person's zone comes from, first match wins.
#[derive(Clone, Debug, PartialEq)]
pub enum ZoneGuess {
    /// The user picked it for this person.
    Picked(Tz),
    /// Their working hours in Microsoft 365, when they are in the tenant.
    WorkingHours(WorkingHours),
    /// The offset of their newest mail, which several zones share on the
    /// day it was sent: `zone` is the likeliest, `others` the rest.
    LastMail { offset: FixedOffset, zone: Tz, others: Vec<Tz> },
}

impl ZoneGuess {
    pub fn zone(&self) -> Tz {
        match self {
            ZoneGuess::Picked(zone) | ZoneGuess::LastMail { zone, .. } => *zone,
            ZoneGuess::WorkingHours(h) => h.zone,
        }
    }

    /// Their working day on their clock: Outlook's when known, else Monday
    /// to Friday, 09:00 to 17:00.
    pub fn hours(&self) -> WorkingHours {
        match self {
            ZoneGuess::WorkingHours(h) => *h,
            _ => WorkingHours::standard(self.zone()),
        }
    }
}

/// The zones Insert my availability offers. For each offset the first
/// listed is the likeliest for someone writing mail, so it comes first
/// when an offset is all Jolta knows.
pub const ZONES: &[Tz] = &[
    Tz::America__New_York,
    Tz::America__Chicago,
    Tz::America__Denver,
    Tz::America__Los_Angeles,
    Tz::America__Phoenix,
    Tz::America__Anchorage,
    Tz::Pacific__Honolulu,
    Tz::America__Toronto,
    Tz::America__Mexico_City,
    Tz::America__Sao_Paulo,
    Tz::America__Argentina__Buenos_Aires,
    Tz::America__Bogota,
    Tz::America__Lima,
    Tz::America__Santiago,
    Tz::America__Halifax,
    Tz::America__St_Johns,
    Tz::Europe__London,
    Tz::Europe__Dublin,
    Tz::Europe__Lisbon,
    Tz::Europe__Paris,
    Tz::Europe__Berlin,
    Tz::Europe__Brussels,
    Tz::Europe__Amsterdam,
    Tz::Europe__Madrid,
    Tz::Europe__Stockholm,
    Tz::Europe__Warsaw,
    Tz::Europe__Athens,
    Tz::Europe__Helsinki,
    Tz::Europe__Kyiv,
    Tz::Europe__Istanbul,
    Tz::Europe__Moscow,
    Tz::Asia__Jerusalem,
    Tz::Africa__Cairo,
    Tz::Africa__Johannesburg,
    Tz::Africa__Lagos,
    Tz::Africa__Nairobi,
    Tz::Asia__Riyadh,
    Tz::Asia__Dubai,
    Tz::Asia__Karachi,
    Tz::Asia__Kolkata,
    Tz::Asia__Dhaka,
    Tz::Asia__Bangkok,
    Tz::Asia__Jakarta,
    Tz::Asia__Singapore,
    Tz::Asia__Shanghai,
    Tz::Asia__Hong_Kong,
    Tz::Asia__Tokyo,
    Tz::Asia__Seoul,
    Tz::Australia__Perth,
    Tz::Australia__Brisbane,
    Tz::Australia__Sydney,
    Tz::Australia__Melbourne,
    Tz::Pacific__Auckland,
];

/// Looks up `address`'s zone through `account`. None when nothing says,
/// and the text stays on the user's clock, `here`.
pub async fn guess(jolta: &Jolta, account: AccountId, address: &str, here: Tz) -> Result<Option<ZoneGuess>> {
    let (person, blobs) = (address.to_string(), jolta.blobs.clone());
    let (picked, mail) = jolta.store.read_async(move |c| Ok((store::person_zone(c, &person)?, last_mail_offset(c, &blobs, &person)?))).await?;
    if let Some(zone) = picked {
        return Ok(Some(ZoneGuess::Picked(zone)));
    }
    // Without an answer from Microsoft 365 the mail still says something.
    if let Ok(Some(hours)) = freebusy::person_hours(jolta, account, address).await {
        return Ok(Some(ZoneGuess::WorkingHours(hours)));
    }
    Ok(mail.and_then(|(offset, sent)| from_offset(offset, sent, here)))
}

/// The guess an offset seen at `sent` gives: None when the Mac's zone had
/// that offset then, as the person is most likely on the user's clock.
pub fn from_offset(offset: FixedOffset, sent: DateTime<Utc>, here: Tz) -> Option<ZoneGuess> {
    if offset_at(here, sent) == offset {
        return None;
    }
    let mut zones = zones_with_offset(offset, sent).into_iter();
    let zone = zones.next()?;
    Some(ZoneGuess::LastMail { offset, zone, others: zones.collect() })
}

/// The offset of the newest of `address`'s last five stored mails whose
/// `Date` header has one, with when it was sent.
fn last_mail_offset(conn: &Connection, blobs: &Blobs, address: &str) -> Result<Option<(FixedOffset, DateTime<Utc>)>> {
    let mut stmt = conn.prepare("SELECT date, blob FROM message WHERE from_addr = ?1 COLLATE NOCASE AND blob IS NOT NULL ORDER BY date DESC LIMIT 5")?;
    let rows = stmt.query_map([address], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (date, blob) in rows {
        let Ok(head) = blobs.head(&blob, 64 * 1024) else { continue };
        if let (Some(offset), Some(sent)) = (date_offset(&head), DateTime::from_timestamp(date, 0)) {
            return Ok(Some((offset, sent)));
        }
    }
    Ok(None)
}

/// The UTC offset in a message's `Date` header. None without one, and for
/// UTC itself: Jolta, Exchange Online and many servers write every `Date`
/// in UTC wherever the writer is, and `-0000` means the zone isn't known
/// (RFC 5322 section 3.3).
pub fn date_offset(raw: &[u8]) -> Option<FixedOffset> {
    let message = MessageParser::new().parse_headers(raw)?;
    let date = message.root().date()?;
    let minutes = i32::from(date.tz_hour) * 60 + i32::from(date.tz_minute);
    let seconds = if date.tz_before_gmt { -minutes } else { minutes } * 60;
    if seconds == 0 || minutes > 14 * 60 {
        return None;
    }
    FixedOffset::east_opt(seconds)
}

/// The zones with `offset` at `at`: from the list Jolta offers, the likeliest
/// first, else every zone in the world that has it.
pub fn zones_with_offset(offset: FixedOffset, at: DateTime<Utc>) -> Vec<Tz> {
    let listed: Vec<Tz> = ZONES.iter().copied().filter(|z| offset_at(*z, at) == offset).collect();
    if !listed.is_empty() {
        return listed;
    }
    const AREAS: &[&str] = &["Africa/", "America/", "Asia/", "Atlantic/", "Australia/", "Europe/", "Indian/", "Pacific/"];
    chrono_tz::TZ_VARIANTS
        .iter()
        .copied()
        .filter(|z| AREAS.iter().any(|a| z.name().starts_with(a)))
        .filter(|z| offset_at(*z, at) == offset)
        .collect()
}

/// The zones Jolta offers, west to east by their offset at `at`.
pub fn choices(at: DateTime<Utc>) -> Vec<Tz> {
    let mut zones = ZONES.to_vec();
    zones.sort_by_key(|z| offset_at(*z, at).local_minus_utc());
    zones
}

pub fn offset_at(zone: Tz, at: DateTime<Utc>) -> FixedOffset {
    zone.offset_from_utc_datetime(&at.naive_utc()).fix()
}

/// "UTC−05:00", "UTC+05:30".
pub fn offset_label(offset: FixedOffset) -> String {
    let seconds = offset.local_minus_utc();
    let sign = if seconds < 0 { '−' } else { '+' };
    let minutes = seconds.abs() / 60;
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// "New York" for America/New_York.
pub fn city(zone: Tz) -> String {
    zone.name().rsplit('/').next().unwrap_or(zone.name()).replace('_', " ")
}

#[cfg(test)]
mod tests {
    use chrono_tz::{America, Europe};

    use super::*;

    fn headers(date: &str) -> Vec<u8> {
        format!("From: Ann <ann@example.com>\r\n{date}Subject: Hi\r\n\r\nBody\r\n").into_bytes()
    }

    fn hours(h: i32) -> FixedOffset {
        FixedOffset::east_opt(h * 3600).unwrap()
    }

    fn day(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("{s}T12:00:00Z")).unwrap().to_utc()
    }

    #[test]
    fn the_date_header_gives_its_offset() {
        assert_eq!(date_offset(&headers("Date: Tue, 10 Nov 2026 09:12:00 -0500\r\n")), Some(hours(-5)));
        assert_eq!(date_offset(&headers("Date: Tue, 10 Nov 2026 19:42:00 +0530\r\n")), FixedOffset::east_opt(5 * 3600 + 30 * 60));
        assert_eq!(date_offset(&headers("Date: Tue,\r\n 10 Nov 2026 09:12:00 -0800 (PST)\r\n")), Some(hours(-8)), "folded, with a comment");
        assert_eq!(date_offset(&headers("")), None, "no Date header");
        assert_eq!(date_offset(&headers("Date: Tue, 10 Nov 2026 14:12:00 GMT\r\n")), None, "GMT says nothing about the writer's clock");
        assert_eq!(date_offset(&headers("Date: Tue, 10 Nov 2026 14:12:00 +0000\r\n")), None, "nor does +0000, Jolta's and Exchange Online's own");
        assert_eq!(date_offset(&headers("Date: Tue, 10 Nov 2026 14:12:00 -0000\r\n")), None);
        assert_eq!(date_offset(&headers("Date: soon\r\n")), None);
    }

    #[test]
    fn an_offset_matches_the_zones_that_have_it_that_day() {
        let zones = zones_with_offset(hours(-5), day("2026-11-10"));
        assert_eq!(zones.first(), Some(&America::New_York));
        assert!(zones.contains(&America::Bogota));
        assert!(!zones.contains(&America::Chicago), "Chicago is at −06:00 in November");
        let summer = zones_with_offset(hours(-5), day("2026-07-10"));
        assert_eq!(summer.first(), Some(&America::Chicago), "in July New York is at −04:00");
        assert!(!summer.contains(&America::New_York));
        let odd = zones_with_offset(FixedOffset::east_opt(5 * 3600 + 45 * 60).unwrap(), day("2026-11-10"));
        assert_eq!(odd.first(), Some(&Tz::Asia__Kathmandu), "an offset off the list still finds its zone");
    }

    #[test]
    fn the_users_own_offset_or_one_no_zone_has_guesses_nothing() {
        let sent = day("2026-11-10");
        assert_eq!(from_offset(hours(1), sent, Europe::Brussels), None);
        assert_eq!(from_offset(hours(2), day("2026-07-10"), Europe::Brussels), None, "Brussels in summer");
        assert_eq!(from_offset(FixedOffset::east_opt(3 * 3600 + 17 * 60).unwrap(), sent, Europe::Brussels), None);
        let Some(ZoneGuess::LastMail { offset, zone, others }) = from_offset(hours(-5), sent, Europe::Brussels) else { panic!("a guess") };
        assert_eq!((offset, zone), (hours(-5), America::New_York));
        assert!(others.contains(&America::Toronto) && !others.contains(&America::New_York));
    }

    #[test]
    fn offsets_read_as_utc_plus_or_minus() {
        assert_eq!(offset_label(hours(-5)), "UTC−05:00");
        assert_eq!(offset_label(FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap()), "UTC+05:30");
    }

    #[test]
    fn the_offered_zones_run_west_to_east() {
        let zones = choices(day("2026-11-10"));
        assert_eq!(zones.first(), Some(&Tz::Pacific__Honolulu));
        assert_eq!(zones.last(), Some(&Tz::Pacific__Auckland));
        assert_eq!(zones.len(), ZONES.len());
    }

    #[tokio::test]
    async fn a_picked_zone_wins_then_working_hours_then_the_last_mail() {
        let dir = std::env::temp_dir().join(format!("jolta-zones-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jolta = Jolta::open(&dir).unwrap();
        jolta.store.write(|tx| Ok(tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'imap', 'me@example.com', 'me', 'a1', 0);")?)).unwrap();
        let add = |id: i64, from: &str, date: i64, header: &str| {
            let blob = jolta.blobs.put(&headers(header)).unwrap();
            jolta
                .store
                .write(|tx| {
                    tx.execute("INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, date, blob) VALUES (?1, 1, ?1, ?1, ?2, ?3, ?4)", rusqlite::params![id, from, date, blob])?;
                    Ok(())
                })
                .unwrap();
        };
        let ann = "ann@example.com";
        let guess_of = |who: &'static str| guess(&jolta, AccountId(1), who, Europe::Brussels);
        assert_eq!(guess_of(ann).await.unwrap(), None, "nothing known");

        let sent = day("2026-11-10").timestamp();
        add(1, ann, sent, "Date: Tue, 10 Nov 2026 07:00:00 -0500\r\n");
        add(2, "Ann@Example.com", sent + 60, "Date: Tue, 10 Nov 2026 12:01:00 +0000\r\n");
        add(3, "bo@example.com", sent + 120, "Date: Tue, 10 Nov 2026 17:32:00 +0530\r\n");
        let Some(ZoneGuess::LastMail { offset, zone, .. }) = guess_of(ann).await.unwrap() else { panic!("a guess from the mail") };
        assert_eq!((offset, zone), (hours(-5), America::New_York), "the newest mail in UTC says nothing, so the one before it counts");

        let tokyo = WorkingHours::standard(Tz::Asia__Tokyo);
        std::fs::write(dir.join("free-busy.json"), format!(r#"{{"peopleHours": {{"{ann}": {}}}}}"#, serde_json::to_string(&tokyo).unwrap())).unwrap();
        assert_eq!(guess_of(ann).await.unwrap(), Some(ZoneGuess::WorkingHours(tokyo)));

        jolta.store.write(|tx| store::set_person_zone(tx, "ANN@example.com", Tz::Europe__Lisbon, 0)).unwrap();
        assert_eq!(guess_of(ann).await.unwrap(), Some(ZoneGuess::Picked(Tz::Europe__Lisbon)), "matched without regard to case");
        std::fs::remove_dir_all(dir).ok();
    }
}
