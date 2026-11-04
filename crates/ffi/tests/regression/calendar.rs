//! Calendar journeys through the app's calls: subscribe to a feed served
//! from this process, read a week, open events, hide the calendar, follow
//! a zone change, schedule reminders, find the next meeting, remove it.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use jolta_core::calendar::{scheduler, tz};
use jolta_ffi::calendar::{CalendarKind, EventResponse, EventTime, MeetingProvider, OccurrenceRecord, OccurrenceTime};

use crate::fixture::{App, block_on};

/// Serves whatever `body` holds at any path, one connection per request.
struct Feed {
    url: String,
    body: Arc<Mutex<String>>,
}

impl Feed {
    fn serve(body: String) -> Feed {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/team.ics", listener.local_addr().unwrap().port());
        let body = Arc::new(Mutex::new(body));
        let served = body.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let body = served.lock().unwrap().clone();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/calendar\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body.as_bytes()));
            }
        });
        Feed { url, body }
    }
}

/// Monday of next week in this Mac's zone, so every event is ahead of now.
fn next_monday(here: Tz) -> NaiveDate {
    let today = Utc::now().with_timezone(&here).date_naive();
    today + chrono::Duration::days(7 - i64::from(today.weekday().num_days_from_monday()))
}

fn stamp(day: NaiveDate, time: &str) -> String {
    format!("{}T{}00", day.format("%Y%m%d"), time.replace(':', ""))
}

fn feed(monday: NaiveDate, standup: &str) -> String {
    let day = |n: i64| monday + chrono::Duration::days(n);
    let alarm = "BEGIN:VALARM\nACTION:DISPLAY\nTRIGGER:-PT10M\nEND:VALARM";
    let zoom = "https://example.zoom.us/j/85612345678?pwd=aBcDeF.1";
    format!(
        "BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Jolta//Regression//EN
X-WR-CALNAME:Team
BEGIN:VEVENT
UID:standup@example.com
DTSTAMP:20261101T000000Z
DTSTART;TZID=Europe/Brussels:{mon_9}
DTEND;TZID=Europe/Brussels:{mon_915}
RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=6
SUMMARY:{standup}
LOCATION:{zoom}
{alarm}
END:VEVENT
BEGIN:VEVENT
UID:standup@example.com
DTSTAMP:20261101T000000Z
RECURRENCE-ID;TZID=Europe/Brussels:{wed_9}
DTSTART;TZID=Europe/Brussels:{wed_14}
DTEND;TZID=Europe/Brussels:{wed_1415}
SUMMARY:{standup}, moved
LOCATION:{zoom}
{alarm}
END:VEVENT
BEGIN:VEVENT
UID:board@example.com
DTSTAMP:20261101T000000Z
DTSTART;TZID=America/New_York:{tue_10}
DTEND;TZID=America/New_York:{tue_11}
SUMMARY:Board call
ORGANIZER;CN=Ann Chair:mailto:ann@example.com
ATTENDEE;CN=Ann Chair;PARTSTAT=ACCEPTED;ROLE=CHAIR:mailto:ann@example.com
ATTENDEE;CN=Bo;PARTSTAT=ACCEPTED:mailto:bo@example.com
ATTENDEE;CN=Cy;PARTSTAT=TENTATIVE;X-RESPONSE-COMMENT=\"Might be late\":mailto:cy@example.com
ATTENDEE;CN=Di:mailto:di@example.com
ATTENDEE;CUTYPE=ROOM;CN=Room 4:mailto:room4@example.com
DESCRIPTION:Agenda in the HTML part
X-ALT-DESC;FMTTYPE=text/html:<p>Agenda</p><script>alert(1)</script><img src=\"https://tracker.test/p.gif\">
END:VEVENT
BEGIN:VEVENT
UID:holiday@example.com
DTSTAMP:20261101T000000Z
DTSTART;VALUE=DATE:{thu}
DTEND;VALUE=DATE:{fri}
SUMMARY:Holiday
END:VEVENT
BEGIN:VEVENT
UID:focus@example.com
DTSTAMP:20261101T000000Z
DTSTART:{fri_8}
DTEND:{fri_9}
SUMMARY:Focus
END:VEVENT
END:VCALENDAR
",
        mon_9 = stamp(day(0), "09:00"),
        mon_915 = stamp(day(0), "09:15"),
        wed_9 = stamp(day(2), "09:00"),
        wed_14 = stamp(day(2), "14:00"),
        wed_1415 = stamp(day(2), "14:15"),
        tue_10 = stamp(day(1), "10:00"),
        tue_11 = stamp(day(1), "11:00"),
        thu = day(3).format("%Y%m%d"),
        fri = day(4).format("%Y%m%d"),
        fri_8 = stamp(day(4), "08:00"),
        fri_9 = stamp(day(4), "09:00"),
    )
    .replace('\n', "\r\n")
}

fn at(day: NaiveDate, time: &str, zone: Tz) -> DateTime<Utc> {
    let wall = NaiveDateTime::parse_from_str(&format!("{day} {time}"), "%Y-%m-%d %H:%M").unwrap();
    zone.from_local_datetime(&wall).unwrap().to_utc()
}

fn week(app: &App, monday: NaiveDate, here: Tz) -> Vec<OccurrenceRecord> {
    let midnight = |d: NaiveDate| at(d, "00:00", here).timestamp();
    app.core.calendar_occurrences(midnight(monday), midnight(monday + chrono::Duration::days(7))).unwrap()
}

fn titles(rows: &[OccurrenceRecord]) -> Vec<&str> {
    rows.iter().map(|o| o.title.as_str()).collect()
}

fn start_of(rows: &[OccurrenceRecord], title: &str) -> i64 {
    match rows.iter().find(|o| o.title == title).map(|o| &o.time) {
        Some(OccurrenceTime::Timed { start, .. }) => *start,
        other => panic!("{title}: {other:?}"),
    }
}

/// Waits up to two seconds for the scheduler to make `done` true.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(2), "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_feed_from_subscription_to_removal() {
    let app = App::new();
    let here = tz::here();
    let monday = next_monday(here);
    let server = Feed::serve(feed(monday, "Standup"));

    let info = block_on(app.core.subscribe_calendar(server.url.clone())).unwrap();
    assert_eq!((info.name.as_str(), info.kind, info.shown, info.counts_as_busy), ("Team", CalendarKind::Webcal, true, false));
    assert_eq!((info.events, info.sync_error.as_deref()), (5, None), "the first download is in");
    assert!(app.events.seen("calendar changed"));
    let list = app.core.calendars().unwrap();
    assert_eq!(list.other_calendars.iter().map(|c| c.id).collect::<Vec<_>>(), [info.id]);

    let rows = week(&app, monday, here);
    assert_eq!(titles(&rows), ["Standup", "Board call", "Standup, moved", "Holiday", "Focus"]);
    let day = |n: i64| monday + chrono::Duration::days(n);
    assert_eq!(start_of(&rows, "Standup"), at(day(0), "09:00", chrono_tz::Europe::Brussels).timestamp());
    assert_eq!(start_of(&rows, "Board call"), at(day(1), "10:00", chrono_tz::America::New_York).timestamp());
    assert_eq!(start_of(&rows, "Focus"), at(day(4), "08:00", here).timestamp(), "a floating time is read here");
    let holiday = rows.iter().find(|o| o.title == "Holiday").unwrap();
    let thursday = day(3).format("%Y-%m-%d").to_string();
    assert_eq!(holiday.time, OccurrenceTime::AllDay { first_day: thursday.clone(), last_day: thursday });
    let standup = &rows[0];
    assert_eq!((standup.meeting_provider, standup.has_attendees, standup.busy), (Some(MeetingProvider::Zoom), false, false));

    let board = rows.iter().find(|o| o.title == "Board call").unwrap();
    let detail = app.core.calendar_event(board.event_id, Some(board.time.clone()), "body{}".into()).unwrap();
    let ny = chrono_tz::America::New_York;
    let (start, end) = (at(day(1), "10:00", ny), at(day(1), "11:00", ny));
    let offset = |z: Tz| z.offset_from_utc_datetime(&start.naive_utc()).to_string();
    let expected = if offset(ny) == offset(here) {
        "10:00 to 11:00".to_string()
    } else {
        let local = |t: DateTime<Utc>| t.with_timezone(&here).format("%H:%M").to_string();
        format!("10:00 to 11:00 New York ({} to {} here)", local(start), local(end))
    };
    match &detail.time {
        EventTime::Timed { text, start: s, .. } => assert_eq!((text.as_str(), *s), (expected.as_str(), start.timestamp())),
        other => panic!("{other:?}"),
    }
    assert_eq!(detail.responses, "2 accepted, 1 maybe, 1 no answer");
    let groups: Vec<(EventResponse, Vec<&str>)> =
        detail.attendees.iter().map(|g| (g.response, g.people.iter().map(|p| p.name.as_str()).collect())).collect();
    assert_eq!(
        groups,
        [
            (EventResponse::Accepted, vec!["Ann Chair", "Bo"]),
            (EventResponse::Tentative, vec!["Cy"]),
            (EventResponse::NeedsAction, vec!["Di"]),
        ]
    );
    assert_eq!(detail.attendees[1].people[0].comment, "Might be late");
    assert_eq!(detail.rooms.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Room 4"]);
    assert_eq!(detail.organizer.as_ref().map(|o| o.address.as_str()), Some("ann@example.com"));
    assert!(detail.description_html.contains("<p>Agenda</p>") && detail.description_html.contains("Content-Security-Policy"));
    assert!(!detail.description_html.contains("<script"), "descriptions are sanitised");
    assert!(detail.description_html.contains("x-remote:https://tracker.test/p.gif"), "remote images stay blocked");
    assert_eq!(detail.repeat, None);

    let moved = rows.iter().find(|o| o.title == "Standup, moved").unwrap();
    let detail = app.core.calendar_event(moved.event_id, Some(moved.time.clone()), String::new()).unwrap();
    assert!(
        detail.repeat.as_deref().is_some_and(|r| r.starts_with("Weekly on Monday and Wednesday")),
        "a changed occurrence describes its series: {:?}",
        detail.repeat
    );
    let meeting = detail.meeting.expect("the Zoom link is found");
    assert_eq!((meeting.provider, meeting.meeting_id.as_deref()), (MeetingProvider::Zoom, Some("85612345678")));
    assert!(meeting.app_url.is_some_and(|u| u.starts_with("zoommtg://")));
    let detail = app.core.calendar_event(holiday.event_id, None, String::new()).unwrap();
    assert!(matches!(detail.time, EventTime::AllDay { ref first_day, .. } if *first_day == day(3).format("%Y-%m-%d").to_string()));

    app.core.set_calendar_shown(info.id, false).unwrap();
    assert!(week(&app, monday, here).is_empty(), "a hidden calendar shows nothing");
    app.core.set_calendar_shown(info.id, true).unwrap();
    assert_eq!(week(&app, monday, here).len(), 5);

    let other = if here == chrono_tz::Asia::Tokyo { chrono_tz::America::New_York } else { chrono_tz::Asia::Tokyo };
    let rebuilt = block_on(scheduler::refresh_occurrences(&app.jolta, Utc::now(), other)).unwrap();
    assert_eq!(rebuilt.len(), 1, "built in another zone");
    let focus = at(day(4), "08:00", here).timestamp();
    assert_eq!(start_of(&week(&app, monday, here), "Focus"), at(day(4), "08:00", other).timestamp(), "the floating event moved");
    app.core.sync_now();
    eventually("the scheduler rebuilds in this Mac's zone", || start_of(&week(&app, monday, here), "Focus") == focus);
    assert_eq!(start_of(&week(&app, monday, here), "Board call"), at(day(1), "10:00", chrono_tz::America::New_York).timestamp());

    let (from, to) = (at(monday, "00:00", here).timestamp(), at(day(7), "00:00", here).timestamp());
    let reminders = app.core.calendar_reminders(from, to).unwrap();
    let brussels = chrono_tz::Europe::Brussels;
    let fired: Vec<(i64, &str)> = reminders.iter().map(|r| (r.fire_at, r.title.as_str())).collect();
    assert_eq!(
        fired,
        [(at(day(0), "08:50", brussels).timestamp(), "Standup"), (at(day(2), "13:50", brussels).timestamp(), "Standup, moved")]
    );
    assert_eq!(app.core.calendar_reminders(from, to).unwrap().iter().map(|r| &r.id).collect::<Vec<_>>(), reminders.iter().map(|r| &r.id).collect::<Vec<_>>());

    *server.body.lock().unwrap() = feed(monday, "Daily standup");
    app.core.sync_now();
    eventually("the scheduler downloads the renamed series", || week(&app, monday, here)[0].title == "Daily standup");
    let renamed = app.core.calendar_reminders(from, to).unwrap();
    assert_eq!(renamed.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["Daily standup", "Daily standup, moved"]);
    assert_eq!(
        renamed.iter().map(|r| &r.id).collect::<Vec<_>>(),
        reminders.iter().map(|r| &r.id).collect::<Vec<_>>(),
        "rewritten rows keep their reminder ids"
    );
    assert!(app.events.seen("calendar synced"));

    let five_before = at(day(0), "08:55", brussels).timestamp();
    let next = app.core.next_meeting(five_before).unwrap().expect("the standup is next");
    assert_eq!((next.title.as_str(), next.start), ("Daily standup", at(day(0), "09:00", brussels).timestamp()));
    assert!(next.meeting.join_url.starts_with("https://example.zoom.us/j/85612345678"));

    app.core.set_calendar_notify(info.id, false).unwrap();
    assert!(app.core.calendar_reminders(from, to).unwrap().is_empty(), "notifications off");

    app.core.remove_calendar(info.id).unwrap();
    assert!(week(&app, monday, here).is_empty());
    assert!(app.core.calendars().unwrap().other_calendars.is_empty());
    let left: i64 = app
        .jolta
        .store
        .read(|c| Ok(c.query_row("SELECT (SELECT count(*) FROM calendar) + (SELECT count(*) FROM event) + (SELECT count(*) FROM occurrence)", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(left, 0, "nothing of the calendar remains");
}

#[test]
fn account_calendars_stay_with_their_account() {
    let app = App::new();
    let account = app.account("me@test.invalid");
    let list = app.core.calendars().unwrap();
    assert_eq!(list.accounts.len(), 1);
    assert!(!list.accounts[0].calendar_on && list.accounts[0].calendars.is_empty(), "calendar starts off");
    assert!(list.other_calendars.is_empty());
    assert!(block_on(app.core.set_calendar_enabled(account.id.0, false)).unwrap().is_empty(), "turning off what is off is harmless");
    assert!(app.core.calendar_event(999_999, None, String::new()).is_err());
    let bad = OccurrenceTime::AllDay { first_day: "next friday".into(), last_day: "2026-11-09".into() };
    assert!(app.core.calendar_event(1, Some(bad), String::new()).is_err());
}

/// The week query the views make, timed through `Core` on the synthetic
/// calendar `jolta cal gen` builds, with UniFFI writing the records into the
/// buffer Swift reads. Run with
/// `cargo test --release -p jolta-ffi --test regression -- --ignored --nocapture week_query`.
#[test]
#[ignore]
fn week_query_through_the_bridge() {
    use uniffi::Lower;

    let app = App::unstarted();
    let built = jolta_core::calendar::synth::generate(&app.jolta, Utc::now()).unwrap();
    let here = tz::here();
    let this_monday = next_monday(here) - chrono::Duration::weeks(1);
    let runs = 200;
    let fresh = jolta_core::Jolta::open(&app.dir).unwrap();
    let (mut store_only, mut query, mut total, mut rows) = (Vec::new(), Vec::new(), Vec::new(), 0);
    for i in 0..runs {
        let offset = if i == 0 { 0 } else { (i as i64 * 7_919) % 156 - 52 };
        let monday = this_monday + chrono::Duration::weeks(offset);
        let (from, to) = (at(monday, "00:00", here).timestamp(), at(monday + chrono::Duration::days(7), "00:00", here).timestamp());
        let t = Instant::now();
        let found = app.core.calendar_occurrences(from, to).unwrap();
        query.push(t.elapsed().as_secs_f64() * 1000.0);
        let count = found.len();
        rows += count;
        let buffer = <Vec<OccurrenceRecord> as Lower<jolta_ffi::UniFfiTag>>::lower(found);
        total.push(t.elapsed().as_secs_f64() * 1000.0);
        drop(buffer.destroy_into_vec());
        let t = Instant::now();
        let bare = fresh.store.read(|c| jolta_core::store::occurrences(c, monday, monday + chrono::Duration::days(7), here)).unwrap();
        store_only.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(bare.len(), count, "week of {monday}");
    }
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() as f64 - 1.0) * p).round() as usize]
    };
    println!("{} occurrences stored; {runs} one-week calls, {:.0} occurrences a week on average.", built.occurrences, rows as f64 / runs as f64);
    println!("then the store query alone, on a second handle: p50 {:.2} ms, p95 {:.2} ms", pct(&mut store_only, 0.5), pct(&mut store_only, 0.95));
    println!("calendar_occurrences p50 {:.2} ms, p95 {:.2} ms", pct(&mut query, 0.5), pct(&mut query, 0.95));
    println!("with UniFFI lowering p50 {:.2} ms, p95 {:.2} ms (budget 5 ms)", pct(&mut total, 0.5), pct(&mut total, 0.95));
}
