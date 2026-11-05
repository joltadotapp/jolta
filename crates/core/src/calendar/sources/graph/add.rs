//! Add to calendar and Update in your calendar on Microsoft 365. A Graph
//! event with guests makes Exchange email them, so the copy is the user's
//! own event without guests: the time, title, place and link. It keeps the
//! invitation's UID through the Global Object ID Exchange gives meetings
//! from outside Outlook ("vCal-Uid" and the UID, [MS-OXCICAL] 2.1.3.1.1.20.26),
//! set as an extended property; `iCalUId` itself is read-only, and Graph
//! ignores it on create (seen on the Microsoft 365 test account, 2026-11-04).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use chrono_tz::Tz;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::super::{NewCopy, Replaced, encode, occurrence_etag, with_join_link};
use super::{GraphCalendar, PatternedRecurrence, rrule};
use crate::calendar::rsvp::{Answer, Occurrence, Stored, Target};
use crate::calendar::{Calendar, EventData, EventId, EventStatus, Local, Moment, Recurrence, Span, Zone, recur, tz};
use crate::{Error, Result};

/// PidLidGlobalObjectId and PidLidCleanGlobalObjectId, which Graph reads
/// back as `iCalUId`.
const GLOBAL_OBJECT_IDS: [&str; 2] =
    ["Binary {6ED8DA90-450B-101B-98DA-00AA003F1305} Id 0x0003", "Binary {6ED8DA90-450B-101B-98DA-00AA003F1305} Id 0x0023"];

impl GraphCalendar {
    /// Creates the event with a `transactionId` made from the calendar and
    /// the UID, so a create retried after a lost answer makes no second
    /// one, then takes out the occurrences the series leaves out and moves
    /// its changed ones.
    pub async fn create(&self, calendar: &Calendar, copy: &NewCopy<'_>) -> Result<String> {
        let here = tz::here();
        let first = copy.master().or(copy.events.first()).ok_or_else(|| Error::Other("The invitation has no event to add.".into()))?;
        let mut body = event_body(first, copy.answer, here)?;
        body["transactionId"] = json!(transaction_id(&calendar.remote_id, copy.uid));
        body["singleValueExtendedProperties"] = json!(GLOBAL_OBJECT_IDS.iter().map(|id| json!({ "id": id, "value": global_object_id(copy.uid) })).collect::<Vec<_>>());
        let (status, created) = self.request("POST", &format!("/me/calendars/{}/events", encode(&calendar.remote_id)), Some(&body), &[]).await?;
        if !(200..300).contains(&status) {
            return Err(self.failure(status, &created));
        }
        let id = created["id"].as_str().ok_or_else(|| Error::Other("Microsoft 365 returned no id for the new event.".into()))?.to_string();
        if copy.master().is_some() && first.recurrence.is_some() {
            let master = Stored { id: EventId(0), remote_id: id.clone(), etag: None, raw: serde_json::to_vec(&created).ok(), data: first.clone() };
            let writes = occurrence_writes(first, copy, &[], here).expect("nothing stored to guard");
            self.occurrences(&master, writes, copy.answer, here).await?;
        }
        Ok(id)
    }

    /// Patches the copy with the newer version, guarded by its change key,
    /// and each changed occurrence stored with it in `occurrences` by its
    /// own. A copy no sync has read yet has none, and is left alone, as is
    /// one with a changed occurrence stored without its change key.
    pub async fn replace(&self, target: &Target, occurrences: &[Stored], copy: &NewCopy<'_>) -> Result<Replaced> {
        let here = tz::here();
        let etag = target.event.etag.as_deref().ok_or_else(|| Error::Other("Jolta hasn't read this copy back from Microsoft 365 yet. Try again in a minute.".into()))?;
        let repeats = target.event.data.recurrence.is_some();
        let writes = match repeats {
            true => occurrence_writes(copy.master().unwrap_or(&target.event.data), copy, occurrences, here),
            false => Some(Vec::new()),
        };
        let Some(writes) = writes else { return Ok(Replaced::Changed) };
        let path = format!("/me/events/{}", encode(&target.event.remote_id));
        let written = copy.master().or_else(|| copy.events.first().filter(|_| !repeats));
        if let Some(e) = written {
            let body = event_body(e, copy.answer, here)?;
            let (status, reply) = self.request("PATCH", &path, Some(&body), &[("If-Match", &if_match(etag))]).await?;
            match status {
                200..300 => {}
                412 => return Ok(Replaced::Changed),
                _ => return Err(self.failure(status, &reply)),
            }
        }
        self.occurrences(&target.event, writes, copy.answer, here).await
    }

    /// Shows the answer the user sent by mail as the copy's free, tentative
    /// or busy time: Graph can't answer the user's own event.
    pub async fn follow(&self, target: &Target, answer: Answer) -> Result<String> {
        let id = match &target.occurrence {
            None => target.event.remote_id.clone(),
            Some(Occurrence { stored: Some(s), .. }) => s.remote_id.clone(),
            Some(Occurrence { recurrence_id, stored: None }) => self.instance(&target.event, recurrence_id).await?,
        };
        let (status, reply) = self.request("PATCH", &format!("/me/events/{}", encode(&id)), Some(&json!({ "showAs": show_as(Some(answer)) })), &[]).await?;
        if !(200..300).contains(&status) {
            return Err(self.failure(status, &reply));
        }
        Ok(id)
    }

    /// Makes the occurrence `writes` on the series `master`. An occurrence
    /// the series doesn't make is passed over.
    async fn occurrences(&self, master: &Stored, writes: Vec<OccurrenceWrite<'_>>, answer: Option<Answer>, here: Tz) -> Result<Replaced> {
        for OccurrenceWrite { rid, change, guard } in writes {
            let id = match self.instance(master, rid).await {
                Ok(id) => id,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let headers: &[(&str, &str)] = match &guard {
                Some(g) => &[("If-Match", g.as_str())],
                None => &[],
            };
            let path = format!("/me/events/{}", encode(&id));
            let (status, reply) = match change {
                Some(o) => {
                    let mut body = event_body(o, answer, here)?;
                    body.as_object_mut().expect("an object").remove("recurrence");
                    self.request("PATCH", &path, Some(&body), headers).await?
                }
                None => self.request("DELETE", &path, None, headers).await?,
            };
            match status {
                200..300 => {}
                404 if change.is_none() => {}
                412 => return Ok(Replaced::Changed),
                _ => return Err(self.failure(status, &reply)),
            }
        }
        Ok(Replaced::Written(None))
    }
}

/// One write to an occurrence of the copy's series.
#[derive(Debug, PartialEq)]
struct OccurrenceWrite<'a> {
    rid: &'a Moment,
    /// Its changed version to patch in, None to delete it.
    change: Option<&'a EventData>,
    /// The `If-Match` of the changed occurrence stored for it.
    guard: Option<String>,
}

/// The occurrence writes that put `copy` over the series `sent`: first a
/// delete for each occurrence the series leaves out or the copy cancels,
/// then a patch for each changed one, each guarded by the change key of
/// the changed occurrence `stored` holds for it. None when one is stored
/// without its change key, so nothing may be written.
fn occurrence_writes<'a>(sent: &'a EventData, copy: &'a NewCopy<'a>, stored: &[Stored], here: Tz) -> Option<Vec<OccurrenceWrite<'a>>> {
    let excluded = sent.recurrence.iter().flat_map(|r| r.exdates.iter());
    let (cancelled, changed): (Vec<_>, Vec<_>) = copy.overrides().partition(|o| o.status == EventStatus::Cancelled);
    let gone = excluded.chain(cancelled.into_iter().filter_map(|o| o.recurrence_id.as_ref())).map(|rid| (rid, None));
    gone.chain(changed.into_iter().filter_map(|o| o.recurrence_id.as_ref().map(|rid| (rid, Some(o)))))
        .map(|(rid, change)| Some(OccurrenceWrite { rid, change, guard: occurrence_etag(stored, rid, here).ok()?.map(if_match) }))
        .collect()
}

/// The `If-Match` value for a stored change key: Graph's ETag is
/// `W/"<changeKey>"`.
fn if_match(etag: &str) -> String {
    if etag.starts_with("W/") { etag.to_string() } else { format!("W/\"{etag}\"") }
}

/// The event as Graph creates it: no guests, so Exchange emails nobody.
/// An unanswered invitation shows as tentative, as Exchange places one.
pub(crate) fn event_body(e: &EventData, answer: Option<Answer>, here: Tz) -> Result<Value> {
    let all_day = matches!(e.span, Span::AllDay { .. });
    let mut v = json!({
        "subject": e.title,
        "body": { "contentType": if e.description_html { "html" } else { "text" }, "content": with_join_link(e) },
        "start": when(&e.span, true, here),
        "end": when(&e.span, false, here),
        "isAllDay": all_day,
        "location": { "displayName": e.location },
        "showAs": show_as(answer),
    });
    if let Some(r) = &e.recurrence {
        v["recurrence"] = pattern(r, &e.span, here)
            .ok_or_else(|| Error::Other("Microsoft 365 can't repeat this meeting the same way, so pick another calendar for it.".into()))?;
    }
    Ok(v)
}

fn show_as(answer: Option<Answer>) -> &'static str {
    match answer {
        Some(Answer::Accept) => "busy",
        Some(Answer::Maybe) | None => "tentative",
        Some(Answer::Decline) => "free",
    }
}

/// A start or end as Graph takes it: the wall-clock time and its zone,
/// midnight for an all-day event, a floating time in the Mac's zone.
fn when(span: &Span, start: bool, here: Tz) -> Value {
    let (wall, zone) = match span {
        Span::AllDay { start: s, end: e } => ((if start { s } else { e }).and_time(NaiveTime::MIN), here),
        Span::Timed { start: s, end: e } => {
            let Local { wall, zone } = if start { s } else { e };
            (*wall, match zone {
                Zone::Tz(tz) => *tz,
                Zone::Floating => here,
            })
        }
    };
    json!({ "dateTime": wall.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": zone.name() })
}

/// The same id for the same calendar and UID.
fn transaction_id(calendar: &str, uid: &str) -> String {
    let hash = Sha256::digest(format!("{calendar}\n{uid}").as_bytes());
    hex::encode(&hash[..16])
}

/// A Global Object ID holding `uid` as Exchange writes one for a meeting
/// from outside Outlook: the class id, no date, and "vCal-Uid", version 1,
/// the UID and a zero byte as its data ([MS-OXOCAL] 2.2.1.27).
fn global_object_id(uid: &str) -> String {
    let mut data = b"vCal-Uid".to_vec();
    data.extend_from_slice(&1u32.to_le_bytes());
    data.extend_from_slice(uid.as_bytes());
    data.push(0);
    let mut id = hex::decode("040000008200E00074C5B7101A82E008").expect("the class id is hex");
    id.extend_from_slice(&[0; 20]);
    id.extend_from_slice(&(data.len() as u32).to_le_bytes());
    id.extend_from_slice(&data);
    STANDARD.encode(id)
}

/// An RRULE as Graph's `patternedRecurrence`. None for what Graph can't
/// hold: more than one rule, RDATEs, frequencies below a day, BY parts
/// beyond the weekdays, one day of the month, one month and one position,
/// and a pattern that would repeat on other days (`same_occurrences`).
pub(crate) fn pattern(r: &Recurrence, span: &Span, here: Tz) -> Option<Value> {
    let [rule] = r.rules.as_slice() else { return None };
    if !r.rdates.is_empty() {
        return None;
    }
    let mut parts = std::collections::BTreeMap::new();
    for part in rule.split(';').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=')?;
        parts.insert(k.to_ascii_uppercase(), v.to_ascii_uppercase());
    }
    if parts.keys().any(|k| !["FREQ", "INTERVAL", "COUNT", "UNTIL", "BYDAY", "BYMONTHDAY", "BYMONTH", "BYSETPOS", "WKST"].contains(&k.as_str())) {
        return None;
    }
    let (start, zone) = match span {
        Span::AllDay { start, .. } => (*start, here),
        Span::Timed { start, .. } => (start.wall.date(), match start.zone {
            Zone::Tz(tz) => tz,
            Zone::Floating => here,
        }),
    };
    let interval: u32 = parts.get("INTERVAL").map_or(Some(1), |i| i.parse().ok())?;
    let day_names = |list: &str| list.split(',').map(day_name).collect::<Option<Vec<_>>>();
    // "2TU" or "-1FR": a position and a weekday.
    let ordinal = |v: &str| -> Option<(i32, &'static str)> {
        let split = v.find(|c: char| c.is_ascii_alphabetic())?;
        Some((v[..split].parse().ok()?, day_name(&v[split..])?))
    };
    let index = |n: i32| -> Option<&'static str> {
        Some(match n {
            1 => "first",
            2 => "second",
            3 => "third",
            4 => "fourth",
            -1 => "last",
            _ => return None,
        })
    };
    let weekday = day_name(&start.format("%a").to_string()[..2].to_ascii_uppercase())?;
    let by = |k: &str| parts.get(k).map(String::as_str);
    let relative = || -> Option<(&'static str, Vec<&'static str>)> {
        match (by("BYDAY"), by("BYSETPOS")) {
            (Some(days), Some(pos)) => Some((index(pos.parse().ok()?)?, day_names(days)?)),
            (Some(day), None) if !day.contains(',') => {
                let (n, d) = ordinal(day)?;
                Some((index(n)?, vec![d]))
            }
            _ => None,
        }
    };
    let pattern = match by("FREQ")? {
        "DAILY" if by("BYDAY").is_none() && by("BYMONTHDAY").is_none() && by("BYMONTH").is_none() && by("BYSETPOS").is_none() => {
            json!({ "type": "daily", "interval": interval })
        }
        "WEEKLY" if by("BYMONTHDAY").is_none() && by("BYMONTH").is_none() && by("BYSETPOS").is_none() => {
            let days = match by("BYDAY") {
                Some(days) => day_names(days)?,
                None => vec![weekday],
            };
            json!({ "type": "weekly", "interval": interval, "daysOfWeek": days, "firstDayOfWeek": by("WKST").map_or(Some("monday"), day_name)? })
        }
        "MONTHLY" if by("BYMONTH").is_none() => match (by("BYMONTHDAY"), by("BYDAY")) {
            (Some(day), None) if by("BYSETPOS").is_none() => {
                let day: u32 = day.parse().ok().filter(|d| (1..=31).contains(d))?;
                json!({ "type": "absoluteMonthly", "interval": interval, "dayOfMonth": day })
            }
            (None, None) if by("BYSETPOS").is_none() => json!({ "type": "absoluteMonthly", "interval": interval, "dayOfMonth": start.format("%-d").to_string().parse::<u32>().ok()? }),
            (None, Some(_)) => {
                let (index, days) = relative()?;
                json!({ "type": "relativeMonthly", "interval": interval, "index": index, "daysOfWeek": days })
            }
            _ => return None,
        },
        "YEARLY" => {
            let month: u32 = by("BYMONTH").map_or(Some(start.format("%-m").to_string()), |m| Some(m.to_string()))?.parse().ok().filter(|m| (1..=12).contains(m))?;
            match (by("BYMONTHDAY"), by("BYDAY")) {
                (day, None) if by("BYSETPOS").is_none() => {
                    let day: u32 = day.map_or(Some(start.format("%-d").to_string()), |d| Some(d.to_string()))?.parse().ok().filter(|d| (1..=31).contains(d))?;
                    json!({ "type": "absoluteYearly", "interval": interval, "month": month, "dayOfMonth": day })
                }
                (None, Some(_)) => {
                    let (index, days) = relative()?;
                    json!({ "type": "relativeYearly", "interval": interval, "month": month, "index": index, "daysOfWeek": days })
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let start_date = start.format("%Y-%m-%d").to_string();
    let range = match (by("COUNT"), by("UNTIL")) {
        (Some(n), None) => json!({ "type": "numbered", "numberOfOccurrences": n.parse::<u32>().ok()?, "startDate": start_date, "recurrenceTimeZone": zone.name() }),
        (None, Some(until)) => {
            let end = until_date(until, zone)?;
            json!({ "type": "endDate", "startDate": start_date, "endDate": end.format("%Y-%m-%d").to_string(), "recurrenceTimeZone": zone.name() })
        }
        (None, None) => json!({ "type": "noEnd", "startDate": start_date, "recurrenceTimeZone": zone.name() }),
        (Some(_), Some(_)) => return None,
    };
    let value = json!({ "pattern": pattern, "range": range });
    same_occurrences(rule, &value, span, start, zone, here).then_some(value)
}

/// Whether Graph's pattern repeats on the days the rule does, as Jolta
/// reads each back, over four years from the series' start: Exchange moves
/// a day of the month that a month lacks to its last day, where RFC 5545
/// skips the month.
fn same_occurrences(rule: &str, value: &Value, span: &Span, start: NaiveDate, zone: Tz, here: Tz) -> bool {
    let Ok(graph) = serde_json::from_value::<PatternedRecurrence>(value.clone()) else { return false };
    let Some(back) = rrule(&graph, matches!(span, Span::AllDay { .. }), zone) else { return false };
    let from = tz::instant(start.and_time(NaiveTime::MIN), zone) - chrono::Duration::days(1);
    let window = recur::Window { start: from, end: from + chrono::Duration::days(4 * 366) };
    let days = |rule: &str| {
        let master = EventData { recurrence: Some(Recurrence { rules: vec![rule.to_string()], ..Recurrence::default() }), ..EventData::new("", *span) };
        recur::expand(&master, &[], window, here).into_iter().map(|o| o.span).collect::<Vec<_>>()
    };
    days(rule) == days(&back)
}

/// The last day an UNTIL lets the series reach, in the series' zone.
fn until_date(until: &str, zone: Tz) -> Option<NaiveDate> {
    if until.len() == 8 {
        return NaiveDate::parse_from_str(until, "%Y%m%d").ok();
    }
    let wall = NaiveDateTime::parse_from_str(until.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
    Some(match until.ends_with('Z') {
        true => wall.and_utc().with_timezone(&zone).date_naive(),
        false => wall.date(),
    })
}

fn day_name(d: &str) -> Option<&'static str> {
    Some(match d {
        "MO" => "monday",
        "TU" => "tuesday",
        "WE" => "wednesday",
        "TH" => "thursday",
        "FR" => "friday",
        "SA" => "saturday",
        "SU" => "sunday",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use chrono_tz::Europe::Brussels;

    use super::super::unpack_uid;
    use super::*;

    /// Each delete or patch of an occurrence Jolta holds changed carries
    /// that occurrence's own change key, and one held without a change key
    /// stops the update before anything is written.
    #[test]
    fn every_occurrence_write_over_a_changed_occurrence_is_guarded() {
        let at = |day: &str| match timed(day) {
            Span::Timed { start, .. } => Moment::Time(start),
            Span::AllDay { .. } => unreachable!(),
        };
        let series = EventData { recurrence: Some(Recurrence { exdates: vec![at("2026-11-24")], ..rules("FREQ=WEEKLY") }), ..EventData::new("u", timed("2026-11-10")) };
        let moved = EventData { recurrence_id: Some(at("2026-11-17")), ..EventData::new("u", timed("2026-11-18")) };
        let events = [series.clone(), moved.clone()];
        let copy = NewCopy { uid: "u", text: "", events: &events, me: "me@example.com", answer: None };
        let held = |rid: &str, etag: Option<&str>| Stored { id: EventId(2), remote_id: "X1".into(), etag: etag.map(str::to_string), raw: None, data: EventData { recurrence_id: Some(at(rid)), ..moved.clone() } };
        let plan = |stored: &[Stored]| occurrence_writes(&series, &copy, stored, Brussels).map(|w| w.into_iter().map(|w| (w.change.is_some(), w.guard)).collect::<Vec<_>>());

        assert_eq!(plan(&[]), Some(vec![(false, None), (true, None)]), "occurrences held unchanged have no change key of their own");
        let guarded = [held("2026-11-17", Some("ck7")), held("2026-11-24", Some("W/\"ck9\""))];
        assert_eq!(plan(&guarded), Some(vec![(false, Some("W/\"ck9\"".into())), (true, Some("W/\"ck7\"".into()))]));
        assert_eq!(plan(&[held("2026-11-17", None)]), None, "a changed occurrence held without its change key");
    }

    fn timed(day: &str) -> Span {
        let wall = NaiveDateTime::parse_from_str(&format!("{day}T09:30:00"), "%Y-%m-%dT%H:%M:%S").unwrap();
        Span::Timed { start: Local { wall, zone: Zone::Tz(Brussels) }, end: Local { wall: wall + chrono::Duration::minutes(30), zone: Zone::Tz(Brussels) } }
    }

    /// The starts a rule makes over two years.
    fn starts(rule: &str, span: Span) -> Vec<Span> {
        let master = EventData { recurrence: Some(rules(rule)), ..EventData::new("u", span) };
        let start = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 11, 1, 0, 0, 0).unwrap();
        let window = recur::Window { start, end: start + chrono::Duration::days(730) };
        recur::expand(&master, &[], window, Brussels).into_iter().map(|o| o.span).collect()
    }

    fn rules(rule: &str) -> Recurrence {
        Recurrence { rules: vec![rule.into()], ..Recurrence::default() }
    }

    #[test]
    fn every_rule_graph_can_hold_comes_back_the_same() {
        for (rule, day) in [
            ("FREQ=DAILY", "2026-11-10"),
            ("FREQ=DAILY;INTERVAL=3;COUNT=10", "2026-11-10"),
            ("FREQ=WEEKLY", "2026-11-10"),
            ("FREQ=WEEKLY;INTERVAL=2;BYDAY=TU,TH;UNTIL=20261219T225959Z", "2026-11-10"),
            ("FREQ=MONTHLY;BYMONTHDAY=12", "2026-11-12"),
            ("FREQ=MONTHLY;BYDAY=-1FR", "2026-11-27"),
            ("FREQ=MONTHLY;BYDAY=TU;BYSETPOS=2", "2026-11-10"),
            ("FREQ=MONTHLY;BYMONTHDAY=28", "2026-11-28"),
            ("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1", "2026-11-30"),
            ("FREQ=MONTHLY;BYDAY=MO,WE;BYSETPOS=3", "2026-11-09"),
            ("FREQ=MONTHLY;INTERVAL=2;BYDAY=-1SU", "2026-11-29"),
            ("FREQ=MONTHLY;BYMONTHDAY=31;COUNT=2", "2026-12-31"),
            ("FREQ=YEARLY", "2026-11-10"),
            ("FREQ=YEARLY;BYMONTH=3;BYDAY=1MO", "2027-03-01"),
        ] {
            let span = timed(day);
            let value = pattern(&rules(rule), &span, Brussels).unwrap_or_else(|| panic!("{rule}"));
            let p: PatternedRecurrence = serde_json::from_value(value.clone()).unwrap();
            let back = rrule(&p, false, Brussels).unwrap();
            assert_eq!(starts(rule, span), starts(&back, span), "{rule} became {value} and {back}");
        }
    }

    #[test]
    fn rules_graph_cant_hold_are_refused() {
        for rule in ["FREQ=HOURLY", "FREQ=DAILY;BYHOUR=9,15", "FREQ=MONTHLY;BYMONTHDAY=1,15", "FREQ=MONTHLY;BYDAY=5TU", "FREQ=YEARLY;BYWEEKNO=20"] {
            assert_eq!(pattern(&rules(rule), &timed("2026-11-10"), Brussels), None, "{rule}");
        }
        let two = Recurrence { rules: vec!["FREQ=DAILY".into(), "FREQ=WEEKLY".into()], ..Recurrence::default() };
        assert_eq!(pattern(&two, &timed("2026-11-10"), Brussels), None);
    }

    /// Exchange moves a day of the month that a month lacks to its last
    /// day, where RFC 5545 skips the month, so such a series isn't copied.
    #[test]
    fn a_series_graph_would_repeat_on_other_days_is_refused() {
        for (rule, day) in [
            ("FREQ=MONTHLY;BYMONTHDAY=29", "2026-11-29"),
            ("FREQ=MONTHLY;BYMONTHDAY=30", "2026-11-30"),
            ("FREQ=MONTHLY;BYMONTHDAY=31", "2026-12-31"),
            ("FREQ=MONTHLY;INTERVAL=3", "2027-01-31"),
            ("FREQ=MONTHLY;BYMONTHDAY=30;UNTIL=20270401T000000Z", "2026-12-30"),
        ] {
            assert_eq!(pattern(&rules(rule), &timed(day), Brussels), None, "{rule}");
        }
        let e = EventData { recurrence: Some(rules("FREQ=MONTHLY;BYMONTHDAY=31")), ..EventData::new("u", timed("2026-12-31")) };
        let refused = event_body(&e, None, Brussels).unwrap_err().to_string();
        assert!(refused.contains("Microsoft 365 can't repeat this meeting the same way"), "{refused}");
    }

    #[test]
    fn the_global_object_id_reads_back_as_the_invitations_uid() {
        let raw = STANDARD.decode(global_object_id("040000008200E00074C5B7101A82E008-not-ours@google.com")).unwrap();
        assert_eq!(unpack_uid(&hex::encode_upper(&raw)).as_deref(), Some("040000008200E00074C5B7101A82E008-not-ours@google.com"));
        assert_eq!(transaction_id("cal", "uid"), transaction_id("cal", "uid"), "a retry sends the same transaction");
        assert_ne!(transaction_id("cal", "uid"), transaction_id("other", "uid"));
    }

    #[test]
    fn the_copy_has_no_guests_and_shows_the_answer_as_its_time() {
        let mut e = EventData::new("u", timed("2026-11-10"));
        e.title = "Review".into();
        e.attendees.push(crate::calendar::Attendee {
            person: crate::calendar::Person { address: "ann@example.com".into(), name: "Ann".into() },
            role: crate::calendar::Role::Required,
            response: crate::calendar::Response::Accepted,
            comment: String::new(),
            is_me: false,
        });
        let v = event_body(&e, Some(Answer::Accept), Brussels).unwrap();
        assert!(v.get("attendees").is_none(), "guests would get Exchange's invitation");
        assert_eq!(v["start"], json!({ "dateTime": "2026-11-10T09:30:00", "timeZone": "Europe/Brussels" }));
        assert_eq!(v["showAs"], "busy");
        assert_eq!(event_body(&e, None, Brussels).unwrap()["showAs"], "tentative");
        assert_eq!(event_body(&e, Some(Answer::Decline), Brussels).unwrap()["showAs"], "free");
    }
}
