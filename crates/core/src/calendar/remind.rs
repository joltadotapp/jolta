//! Reminders the app schedules as notifications, and the next online
//! meeting for the menu bar item and "Join next meeting".

use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use super::{Bounds, CalendarId, EventData, EventId, Meeting, meeting, tz};
use crate::store::JoinLink;
use crate::{Error, Result};

/// Alarms further ahead of their event than this aren't found. Google
/// allows up to 4 weeks.
const MAX_LEAD: Duration = Duration::days(29);
/// Alarms set to go off after the event started (a negative lead).
const MAX_AFTER: Duration = Duration::days(1);
/// A meeting starting this soon wins over one already running.
const SOON: Duration = Duration::minutes(10);

/// One alarm of one occurrence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reminder {
    /// The same for the same event, occurrence start and alarm, whatever
    /// syncs rewrote in between, so the app can replace a scheduled
    /// notification by id.
    pub id: String,
    pub fire_at: DateTime<Utc>,
    /// The override when this occurrence was changed, else the series.
    pub event_id: EventId,
    pub calendar_id: CalendarId,
    pub title: String,
    pub location: String,
    pub bounds: Bounds,
    pub meeting: Option<JoinLink>,
}

/// The alarms that go off from `from` up to `to`, in order, from shown
/// calendars with notifications on. Events the user declined and email
/// alarms (the server sends those) are left out. All-day events count
/// from midnight in `here`.
pub fn between(conn: &Connection, from: DateTime<Utc>, to: DateTime<Utc>, here: Tz) -> Result<Vec<Reminder>> {
    let (first, last) = (from - MAX_AFTER, to + MAX_LEAD);
    let day = |t: DateTime<Utc>| tz::day_number(t.with_timezone(&here).date_naive());
    let mut stmt = conn.prepare_cached(
        "SELECT e.uid, o.event_id, o.calendar_id, e.title, e.location, o.start_utc, o.end_utc, o.start_day, o.end_day,
                e.meeting_provider, e.join_url,
                json_extract(a.value, '$.trigger.MinutesBefore'), json_extract(a.value, '$.trigger.At'),
                c.source, c.is_default, ifnull(json_extract(e.data, '$.organizer_is_me'), 0)
         FROM occurrence o JOIN calendar c ON c.id = o.calendar_id JOIN event e ON e.id = o.event_id,
              json_each(e.data, '$.alarms') a
         WHERE ((o.start_utc >= ?1 AND o.start_utc < ?2) OR (o.start_day >= ?3 AND o.start_day <= ?4))
           AND c.shown AND c.notify AND e.status <> 'cancelled' AND ifnull(e.my_response, '') <> 'declined'
           AND NOT json_extract(a.value, '$.email')",
    )?;
    let rows = stmt.query_map(params![first.timestamp(), last.timestamp(), day(first) - 1, day(last) + 1], |r| {
        let bounds = match (r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?) {
            (Some(start), Some(end)) => Bounds::Utc { start: utc(start), end: utc(end) },
            _ => Bounds::Days { start: r.get(7)?, end: r.get(8)? },
        };
        let meeting = match (r.get(9)?, r.get(10)?) {
            (Some(provider), Some(url)) => Some(JoinLink { provider, url }),
            _ => None,
        };
        let uid: String = r.get(0)?;
        let trigger = match (r.get::<_, Option<i64>>(11)?, r.get::<_, Option<String>>(12)?) {
            (Some(minutes), _) => Trigger::Before(minutes),
            (None, Some(at)) => DateTime::parse_from_rfc3339(&at).map_or(Trigger::Unknown, |t| Trigger::At(t.to_utc())),
            (None, None) => Trigger::Unknown,
        };
        let reminder = Reminder {
            id: String::new(),
            fire_at: DateTime::<Utc>::MIN_UTC,
            event_id: r.get(1)?,
            calendar_id: r.get(2)?,
            title: r.get(3)?,
            location: r.get(4)?,
            bounds,
            meeting,
        };
        let rank = super::series::copy_rank(&uid, r.get(15)?, r.get(13)?, r.get(14)?, reminder.calendar_id);
        Ok((uid, trigger, rank, reminder))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (uid, trigger, rank, mut reminder) = row?;
        let start = match reminder.bounds {
            Bounds::Utc { start, .. } => start,
            Bounds::Days { start, .. } => {
                tz::instant((chrono::NaiveDate::default() + Duration::days(start.into())).and_time(NaiveTime::MIN), here)
            }
        };
        reminder.fire_at = match trigger {
            Trigger::Before(minutes) => start - Duration::minutes(minutes),
            Trigger::At(at) => at,
            Trigger::Unknown => continue,
        };
        if reminder.fire_at < from || reminder.fire_at >= to {
            continue;
        }
        reminder.id = reminder_id(reminder.calendar_id, &uid, &reminder.bounds, trigger);
        out.push((uid, rank, reminder));
    }
    // A meeting in two calendars reminds once, from the copy the views show.
    out.sort_by(|(ua, ra, a), (ub, rb, b)| (ua, a.fire_at, ra).cmp(&(ub, b.fire_at, rb)));
    out.dedup_by(|(ua, _, a), (ub, _, b)| ua == ub && a.bounds == b.bounds && a.fire_at == b.fire_at);
    let mut out: Vec<Reminder> = out.into_iter().map(|(_, _, r)| r).collect();
    out.sort_by(|a, b| (a.fire_at, &a.id).cmp(&(b.fire_at, &b.id)));
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
enum Trigger {
    Before(i64),
    At(DateTime<Utc>),
    Unknown,
}

fn reminder_id(calendar: CalendarId, uid: &str, bounds: &Bounds, trigger: Trigger) -> String {
    let start = match bounds {
        Bounds::Utc { start, .. } => start.timestamp().to_string(),
        Bounds::Days { start, .. } => format!("day {start}"),
    };
    let trigger = match trigger {
        Trigger::Before(m) => format!("{m} before"),
        Trigger::At(at) => format!("at {}", at.timestamp()),
        Trigger::Unknown => String::new(),
    };
    let key = format!("{calendar}\n{uid}\n{start}\n{trigger}");
    format!("jolta-calendar-{}", hex::encode(&Sha256::digest(key.as_bytes())[..16]))
}

fn utc(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextMeeting {
    pub event_id: EventId,
    pub calendar_id: CalendarId,
    pub color: Option<String>,
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub meeting: Meeting,
    /// Opens the provider's own app, for providers that have a link for it.
    pub app_url: Option<String>,
}

/// The online meeting to join at `now`, within the next week: one starting
/// in the next 10 minutes, else the one running that started last, else
/// the next. Declined and cancelled meetings and hidden calendars don't count.
pub fn next_meeting(conn: &Connection, now: DateTime<Utc>) -> Result<Option<NextMeeting>> {
    let mut stmt = conn.prepare_cached(
        "SELECT o.event_id, o.calendar_id, coalesce(c.user_color, c.color), e.title, o.start_utc, o.end_utc, e.data
         FROM occurrence o JOIN calendar c ON c.id = o.calendar_id JOIN event e ON e.id = o.event_id
         WHERE o.start_utc >= ?1 - 86400 AND o.start_utc < ?1 + 7 * 86400 AND o.end_utc > ?1
           AND c.shown AND e.join_url IS NOT NULL AND e.status <> 'cancelled' AND ifnull(e.my_response, '') <> 'declined'
         ORDER BY o.start_utc, o.end_utc",
    )?;
    let rows = stmt.query_map([now.timestamp()], |r| {
        Ok((
            r.get::<_, EventId>(0)?,
            r.get::<_, CalendarId>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            utc(r.get(4)?),
            utc(r.get(5)?),
            r.get::<_, String>(6)?,
        ))
    })?;
    let rows: Vec<_> = rows.collect::<rusqlite::Result<_>>()?;
    let soon = rows.iter().find(|r| r.4 > now && r.4 <= now + SOON);
    let running = rows.iter().filter(|r| r.4 <= now).max_by_key(|r| r.4);
    let next = rows.iter().find(|r| r.4 > now);
    let Some((event_id, calendar_id, color, title, start, end, data)) = soon.or(running).or(next).cloned() else {
        return Ok(None);
    };
    let event: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {event_id}: {e}")))?;
    let Some(m) = event.meeting else { return Ok(None) };
    Ok(Some(NextMeeting { event_id, calendar_id, color, title, start, end, app_url: meeting::app_url(&m), meeting: m }))
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDateTime, TimeZone};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::series::Horizon;
    use crate::calendar::{
        Access, Alarm, AlarmTrigger, Attendee, CalendarSource, Local, MeetingProvider, Person, Recurrence, Response, Role,
        Span, Zone,
    };
    use crate::store::{self, RemoteCalendar, Store};

    struct Folder(std::path::PathBuf);

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store(name: &str) -> (Store, CalendarId, Folder) {
        let dir = std::env::temp_dir().join(format!("jolta-remind-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        let remote = RemoteCalendar {
            remote_id: "work".into(),
            name: "Work".into(),
            color: Some("#3366ff".into()),
            zone: None,
            access: Access::Owner,
            primary: true,
            refresh: None,
            video: None,
        };
        let id = s.write(|tx| Ok(store::add_calendar(tx, CalendarSource::Local, &remote)?.id)).unwrap();
        (s, id, Folder(dir))
    }

    fn at(s: &str) -> DateTime<Utc> {
        Brussels.from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).unwrap().to_utc()
    }

    fn timed(start: &str, minutes: i64) -> Span {
        let wall = NaiveDateTime::parse_from_str(start, "%Y-%m-%d %H:%M").unwrap();
        let start = Local { wall, zone: Zone::Tz(Brussels) };
        Span::Timed { start, end: Local { wall: wall + Duration::minutes(minutes), ..start } }
    }

    fn alarm(minutes: i64) -> Alarm {
        Alarm { trigger: AlarmTrigger::MinutesBefore(minutes), email: false }
    }

    fn zoom() -> Meeting {
        Meeting {
            provider: MeetingProvider::Zoom,
            join_url: "https://example.zoom.us/j/123456789".into(),
            meeting_id: Some("123 456 789".into()),
            passcode: None,
            phones: Vec::new(),
            other_links: Vec::new(),
        }
    }

    fn put(s: &Store, cal: CalendarId, etag: &str, events: &[EventData]) {
        let horizon = Horizon::around(at("2026-11-04 12:00"), Brussels);
        s.write(|tx| store::apply_item(tx, cal, &events[0].uid, Some(etag), events, None, &horizon)).unwrap();
    }

    fn declined(e: EventData) -> EventData {
        let me = Attendee {
            person: Person { address: "me@example.com".into(), name: String::new() },
            role: Role::Required,
            response: Response::Declined,
            comment: String::new(),
            is_me: true,
        };
        EventData { attendees: vec![me], ..e }
    }

    #[test]
    fn reminders_between_two_instants() {
        let (s, cal, _folder) = store("between");
        let standup = EventData {
            title: "Standup".into(),
            recurrence: Some(Recurrence { rules: vec!["FREQ=DAILY;COUNT=5".into()], ..Recurrence::default() }),
            alarms: vec![alarm(10), Alarm { trigger: AlarmTrigger::MinutesBefore(60), email: true }],
            meeting: Some(zoom()),
            ..EventData::new("standup", timed("2026-11-09 09:00", 15))
        };
        let holiday = EventData {
            title: "Armistice".into(),
            alarms: vec![alarm(15 * 60)],
            ..EventData::new(
                "holiday",
                Span::AllDay {
                    start: chrono::NaiveDate::from_ymd_opt(2026, 11, 11).unwrap(),
                    end: chrono::NaiveDate::from_ymd_opt(2026, 11, 12).unwrap(),
                },
            )
        };
        let absolute = EventData {
            title: "Dentist".into(),
            alarms: vec![Alarm { trigger: AlarmTrigger::At(at("2026-11-09 20:00")), email: false }],
            ..EventData::new("dentist", timed("2026-12-01 08:00", 30))
        };
        let far = EventData { title: "Trip".into(), alarms: vec![alarm(14 * 24 * 60)], ..EventData::new("trip", timed("2026-11-24 07:00", 60)) };
        let no = declined(EventData { title: "Declined".into(), alarms: vec![alarm(5)], ..EventData::new("no", timed("2026-11-09 11:00", 30)) });
        for e in [&standup, &holiday, &absolute, &far, &no] {
            put(&s, cal, "1", std::slice::from_ref(e));
        }
        let list = |s: &Store, from: &str, to: &str| s.read(|c| between(c, at(from), at(to), Brussels)).unwrap();
        let fired: Vec<(String, String)> = list(&s, "2026-11-09 00:00", "2026-11-11 00:00")
            .into_iter()
            .map(|r| (r.fire_at.with_timezone(&Brussels).format("%d %H:%M").to_string(), r.title))
            .collect();
        let expected = [
            ("09 08:50", "Standup"),
            ("09 20:00", "Dentist"),
            ("10 07:00", "Trip"),
            ("10 08:50", "Standup"),
            ("10 09:00", "Armistice"),
        ];
        assert_eq!(fired, expected.map(|(a, b)| (a.to_string(), b.to_string())), "email alarms and declined events left out");

        let first = list(&s, "2026-11-09 00:00", "2026-11-11 00:00");
        let renamed = EventData { title: "Daily standup".into(), ..standup.clone() };
        put(&s, cal, "2", &[renamed]);
        let again = list(&s, "2026-11-09 00:00", "2026-11-11 00:00");
        assert_ne!(first[0].event_id, again[0].event_id, "the sync rewrote the rows");
        assert_eq!(first.iter().map(|r| &r.id).collect::<Vec<_>>(), again.iter().map(|r| &r.id).collect::<Vec<_>>(), "ids stay");
        assert_eq!(again[0].meeting.as_ref().map(|m| m.provider), Some(MeetingProvider::Zoom));

        s.write(|tx| store::set_calendar_notify(tx, cal, false)).unwrap();
        assert!(list(&s, "2026-11-09 00:00", "2026-11-11 00:00").is_empty(), "notifications off");
    }

    #[test]
    fn next_meeting_prefers_one_about_to_start() {
        let (s, cal, _folder) = store("next");
        let meeting = |uid: &str, title: &str, start: &str, minutes: i64| EventData {
            title: title.into(),
            meeting: Some(zoom()),
            ..EventData::new(uid, timed(start, minutes))
        };
        put(&s, cal, "1", &[meeting("long", "Workshop", "2026-11-09 09:00", 180)]);
        put(&s, cal, "1", &[meeting("next", "Review", "2026-11-09 11:00", 30)]);
        put(&s, cal, "1", &[declined(meeting("no", "Declined", "2026-11-09 10:30", 30))]);
        put(&s, cal, "1", &[EventData { title: "Lunch".into(), ..EventData::new("lunch", timed("2026-11-09 10:40", 30)) }]);
        let title = |now: &str| s.read(|c| next_meeting(c, at(now))).unwrap().map(|m| m.title);
        assert_eq!(title("2026-11-09 08:00").as_deref(), Some("Workshop"), "nothing running: the next one");
        assert_eq!(title("2026-11-09 10:00").as_deref(), Some("Workshop"), "running, and the next is an hour away");
        assert_eq!(title("2026-11-09 10:52").as_deref(), Some("Review"), "starting in 8 minutes wins");
        assert_eq!(title("2026-11-09 11:31").as_deref(), Some("Workshop"), "the review ended, the workshop runs on");
        assert_eq!(title("2026-11-09 12:00"), None, "all over");
        let m = s.read(|c| next_meeting(c, at("2026-11-09 08:00"))).unwrap().unwrap();
        assert_eq!((m.start, m.end), (at("2026-11-09 09:00"), at("2026-11-09 12:00")));
        assert_eq!(m.meeting.join_url, "https://example.zoom.us/j/123456789");
        assert!(m.app_url.is_some_and(|u| u.starts_with("zoommtg:")), "Zoom opens in its app");
    }
}
