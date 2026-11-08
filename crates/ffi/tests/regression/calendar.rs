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
use jolta_core::MailboxRole;
use jolta_ffi::calendar::{
    AnswerOptions, CalendarKind, EventResponse, EventTime, InvitationAnswer, InvitationState, MeetingProvider, OccurrenceRecord, OccurrenceTime,
};

use crate::fixture::{App, BY_DATE, Mail, block_on};

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

/// Waits up to ten seconds for the scheduler to make `done` true.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(10), "timed out waiting: {what}");
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

    // The scheduler may move the series back at any moment after this, so
    // only where they end up is checked here.
    let other = if here == chrono_tz::Asia::Tokyo { chrono_tz::America::New_York } else { chrono_tz::Asia::Tokyo };
    let rebuilt = block_on(scheduler::refresh_occurrences(&app.jolta, Utc::now(), other)).unwrap();
    assert_eq!(rebuilt.len(), 1, "built in another zone");
    let focus = at(day(4), "08:00", here).timestamp();
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
    let delivered = &reminders[0];
    assert_ne!(delivered.event_id, renamed[0].event_id, "the sync replaced the event's row");
    let now = app.core.calendar_reminder(delivered.id.clone(), delivered.fire_at).unwrap().expect("a delivered reminder still resolves");
    assert_eq!((now.event_id, now.title.as_str()), (renamed[0].event_id, "Daily standup"), "a notification acts on today's row, not the one it was scheduled with");
    assert!(app.events.seen("calendar synced"));

    let five_before = at(day(0), "08:55", brussels).timestamp();
    let next = app.core.next_meeting(five_before).unwrap().expect("the standup is next");
    assert_eq!((next.title.as_str(), next.start), ("Daily standup", at(day(0), "09:00", brussels).timestamp()));
    assert!(next.meeting.join_url.starts_with("https://example.zoom.us/j/85612345678"));

    app.core.set_calendar_notify(info.id, false).unwrap();
    assert!(app.core.calendar_reminders(from, to).unwrap().is_empty(), "notifications off");
    assert!(app.core.calendar_reminder(delivered.id.clone(), delivered.fire_at).unwrap().is_none(), "a snoozed reminder of a silenced calendar resolves to nothing");

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

/// Answering an invitation in the reader of an account whose calendar is
/// off: the card offers the answers, Accept sends one reply mail to the
/// organizer, and the conversation opened again shows the answer.
#[test]
fn an_invitation_answered_from_the_reader() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let monday = next_monday(tz::here());
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:review@test.invalid\r\nDTSTAMP:20261101T000000Z\r\n\
         DTSTART:{}Z\r\nDTEND:{}Z\r\nSUMMARY:Design review\r\nORGANIZER;CN=Ann Organizer:mailto:ann@test.invalid\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.invalid\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:bo@test.invalid\r\n\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
        stamp(monday, "14:00"),
        stamp(monday, "15:00")
    );
    let mail = Mail::new("ann@test.invalid", "me@test.invalid", "Invitation: Design review", "Join us.").attach("invite.ics", &ics);
    let message = app.deliver(&me, MailboxRole::Inbox, &mail);
    let thread = app.thread_of(message);

    let before = app.open(thread);
    let invitation = before.invitations.first().expect("an invitation to answer");
    assert_eq!((invitation.message_id, invitation.answer, invitation.organizer.as_str(), invitation.expanded), (message.0, None, "Ann", true));
    assert!(before.html.contains(&format!("href=\"x-rsvp:{}/accept\"", message.0)), "the card has the buttons");
    assert!(!before.html.contains("jolta-chosen"));

    let options = AnswerOptions { note: "  ".into(), notify: true, occurrence_only: false };
    let toast = block_on(app.core.answer_invitation(message.0, InvitationAnswer::Accept, options)).expect("the answer goes out");
    assert_eq!(toast, "Accepted. Jolta emailed Ann.");
    assert_eq!(app.count("SELECT count(*) FROM outbox WHERE recipients = 'ann@test.invalid' AND sender = 'me@test.invalid'"), 1);
    assert_eq!(app.count("SELECT count(*) FROM outbox"), 1, "one reply, to the organizer only");

    let after = app.open(thread);
    assert_eq!(after.invitations[0].answer, Some(EventResponse::Accepted));
    assert!(after.html.contains("jolta-rsvp-accept jolta-chosen"), "the card shows the answer given");

    let quiet = AnswerOptions { note: String::new(), notify: false, occurrence_only: false };
    let toast = block_on(app.core.answer_invitation(message.0, InvitationAnswer::Decline, quiet)).unwrap();
    assert_eq!(toast, "Declined without telling Ann.");
    assert_eq!(app.count("SELECT count(*) FROM outbox"), 0, "Don't send a reply sends nothing and takes back the Accept still waiting");
    assert_eq!(app.open(thread).invitations[0].answer, Some(EventResponse::Declined));

    // Read, and followed by an ordinary reply, the invitation starts
    // collapsed, so Y, M and N have no card on screen to answer.
    app.jolta.store.write(|tx| Ok(tx.execute("UPDATE message SET flags = flags | 1 WHERE id = ?1", [message.0])?)).unwrap();
    app.deliver(&me, MailboxRole::Inbox, &Mail::reply(&mail, "bo@test.invalid", "See you there."));
    let later = app.open(thread);
    let shown: Vec<(i64, bool)> = later.invitations.iter().map(|i| (i.message_id, i.expanded)).collect();
    assert_eq!(shown, [(message.0, false)], "the invitation is listed, collapsed");
}

/// An invitation part for `uid` from `organizer` to the test account, on
/// `day` from 14:00 to 15:00 UTC.
fn invitation(uid: &str, organizer: &str, day: NaiveDate, method: &str, sequence: u32) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261101T000000Z\r\nSEQUENCE:{sequence}\r\n\
         DTSTART:{}Z\r\nDTEND:{}Z\r\nSUMMARY:{uid}\r\nORGANIZER:mailto:{organizer}\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.invalid\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        stamp(day, "14:00"),
        stamp(day, "15:00")
    )
}

/// The list marks conversations with an invitation and shows the answer
/// state of requests; `is:invite` finds the ones still to answer, and both
/// change once the reader's card answers one.
#[test]
fn invitations_in_the_list_and_is_invite() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let monday = next_monday(tz::here());
    let lastweek = monday - chrono::Duration::days(14);
    let deliver = |subject: &str, from: &str, ics: String| {
        app.deliver(&me, MailboxRole::Inbox, &Mail::new(from, "me@test.invalid", subject, "Details inside.").attach("invite.ics", &ics))
    };
    let review = deliver("Design review", "ann@test.invalid", invitation("review", "ann@test.invalid", monday, "REQUEST", 0));
    deliver("Old retro", "ann@test.invalid", invitation("retro", "ann@test.invalid", lastweek, "REQUEST", 0));
    deliver("My planning", "me@test.invalid", invitation("planning", "me@test.invalid", monday, "REQUEST", 0));
    deliver("Offsite", "bo@test.invalid", invitation("offsite", "bo@test.invalid", monday, "REQUEST", 0));
    deliver("Offsite cancelled", "bo@test.invalid", invitation("offsite", "bo@test.invalid", monday, "CANCEL", 1));
    app.deliver(&me, MailboxRole::Inbox, &Mail::new("cy@test.invalid", "me@test.invalid", "Lunch?", "No calendar here."));

    let states = |app: &App| -> Vec<(String, Option<InvitationState>)> { app.list("in:inbox").into_iter().map(|r| (r.subject, r.invitation)).collect() };
    assert_eq!(
        states(&app),
        [
            ("Lunch?".into(), None),
            ("Offsite cancelled".into(), Some(InvitationState::Cancelled)),
            ("Offsite".into(), Some(InvitationState::Cancelled)),
            ("My planning".into(), Some(InvitationState::Plain)),
            ("Old retro".into(), Some(InvitationState::Plain)),
            ("Design review".into(), Some(InvitationState::Unanswered)),
        ]
    );
    assert_eq!(app.subjects("-is:invite in:inbox").len(), 5);
    assert_eq!(app.subjects("is:invite"), ["Design review"]);

    let options = AnswerOptions { note: String::new(), notify: true, occurrence_only: false };
    block_on(app.core.answer_invitation(review.0, InvitationAnswer::Accept, options)).expect("the answer goes out");
    assert!(app.events.seen("calendar changed"), "the app reloads its list on this");
    assert_eq!(app.core.list_count("is:invite".into(), BY_DATE).unwrap(), 0, "the cached order went with the answer");
    let answered = app.list("in:inbox").into_iter().find(|r| r.message_id == review.0).unwrap();
    assert_eq!(answered.invitation, Some(InvitationState::Answered { response: EventResponse::Accepted }));
}

/// A cached list holds only until a "Not answered" invitation in it ends,
/// and the app hears then that lists changed.
#[test]
fn the_list_order_expires_when_an_unanswered_invitation_ends() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let end = chrono::DateTime::from_timestamp(Utc::now().timestamp() + 3, 0).unwrap();
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:soon\r\nDTSTAMP:20261101T000000Z\r\n\
         DTSTART:{}\r\nDTEND:{}\r\nSUMMARY:Soon\r\nORGANIZER:mailto:ann@test.invalid\r\n\
         ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:me@test.invalid\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        (end - chrono::Duration::hours(1)).format("%Y%m%dT%H%M%SZ"),
        end.format("%Y%m%dT%H%M%SZ"),
    );
    app.deliver(&me, MailboxRole::Inbox, &Mail::new("ann@test.invalid", "me@test.invalid", "Soon", "Details inside.").attach("invite.ics", &ics));
    assert_eq!(app.core.list_count("is:invite".into(), BY_DATE).unwrap(), 1);
    let mut heard = app.events.0.lock().unwrap().len();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let events = app.events.0.lock().unwrap().clone();
        if events[heard..].contains(&"lists") {
            if Utc::now() >= end {
                break;
            }
            heard = events.len();
        }
        assert!(std::time::Instant::now() < deadline, "the app hears that lists changed when the event ends");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(app.core.list_count("is:invite".into(), BY_DATE).unwrap(), 0, "the cached order went with it");
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

/// A calendar on this Mac: made from Settings, edits shown at once with the
/// "not synced yet" mark, the worker settling them, and Undo.
#[test]
fn a_local_calendar_takes_edits_through_the_queue() {
    use jolta_core::calendar::ops::{self, Change, Edit, OpState};
    use jolta_core::calendar::series::Horizon;
    use jolta_core::calendar::{CalendarId, EventData, Local, Span, Zone};

    let app = App::new();
    let here = tz::here();
    let info = app.core.new_calendar_here("Test".into(), Some("#3366ff".into())).unwrap();
    assert_eq!((info.kind, info.shown, info.events), (CalendarKind::Local, true, 0));
    let caps = app.core.capabilities(info.id).unwrap();
    assert!(caps.writable && !caps.sends_itself, "a local calendar is written here and mails its guests itself");

    let monday = next_monday(here);
    let event = |uid: &str, title: &str, time: &str| {
        let wall = NaiveDateTime::new(monday, chrono::NaiveTime::parse_from_str(time, "%H:%M").unwrap());
        let start = Local { wall, zone: Zone::Tz(here) };
        EventData { title: title.into(), ..EventData::new(uid, Span::Timed { start, end: Local { wall: wall + chrono::Duration::minutes(30), ..start } }) }
    };
    let queue = |change: Change, hold: bool| {
        let now = Utc::now().timestamp();
        app.jolta
            .store
            .write(|tx| {
                let id = ops::queue(tx, &Edit::quiet(CalendarId(info.id), change), now, &Horizon::around(Utc::now(), here))?.unwrap();
                if hold {
                    jolta_core::store::hold_calendar_op(tx, id, now + 3600)?;
                }
                Ok(id)
            })
            .unwrap()
    };
    let held = queue(Change::Create(vec![event("held@test", "Held", "09:00")]), true);
    let due = queue(Change::Create(vec![event("due@test", "Due", "11:00")]), false);
    let marks = |app: &App| week(app, monday, here).into_iter().map(|o| (o.title, o.not_synced)).collect::<Vec<_>>();
    assert_eq!(marks(&app), [("Held".to_string(), true), ("Due".to_string(), true)], "both show at once, marked");

    app.core.undo_calendar(held.0).unwrap();
    assert_eq!(titles(&week(&app, monday, here)), ["Due"], "the held op was never sent: undo drops it");
    eventually("the worker settles the due op", || {
        app.jolta.store.read(|c| jolta_core::store::calendar_op(c, due)).unwrap().state == OpState::Done
    });
    assert_eq!(marks(&app), [("Due".to_string(), false)], "settled: the mark goes");

    app.core.undo_calendar(due.0).unwrap();
    eventually("the opposite op removes it", || week(&app, monday, here).is_empty());
    assert!(app.core.undo_calendar(due.0).is_err(), "undoing twice changes nothing more");
}

/// The editor's draft goes to Swift and back whole: a new event saves, opens
/// again with its stored version, takes a typed time and a reminder, and
/// Undo of its delete brings it back.
#[test]
fn an_event_draft_round_trips_through_the_bridge() {
    use jolta_ffi::editor::{DraftField, DraftReminder};

    let app = App::new();
    let here = tz::here();
    let info = app.core.new_calendar_here("Test".into(), None).unwrap();
    assert_eq!(app.core.default_calendar().unwrap(), Some(info.id));
    let monday = next_monday(here);
    let slot = OccurrenceTime::Timed { start: at(monday, "14:00", here).timestamp(), end: at(monday, "15:00", here).timestamp() };

    let mut draft = app.core.new_draft(None, Some(slot.clone())).unwrap();
    assert_eq!((draft.calendar_id, draft.time.clone(), draft.reminder, draft.zone.as_str()), (info.id, slot, DraftReminder::Default, here.name()));
    draft.title = "Design review".into();
    assert!(app.core.check_draft(draft_copy(&draft)).unwrap().is_empty());
    let saved = app.core.save_event(draft, None, None, None).unwrap();
    assert_eq!(saved.message, "Added Design review");
    let row = week(&app, monday, here).into_iter().find(|o| o.title == "Design review").expect("shown at once");

    let mut draft = app.core.draft_for_event(row.event_id, Some(row.time.clone())).unwrap();
    assert!(draft.base.is_some() && draft.locks.is_empty());
    assert!(app.core.plan_save(draft_copy(&draft)).unwrap().unchanged);
    let minutes = app.core.parse_time_input("1530".into()).unwrap();
    assert_eq!(minutes, 15 * 60 + 30);
    draft.time = OccurrenceTime::Timed { start: at(monday, "15:30", here).timestamp(), end: at(monday, "16:30", here).timestamp() };
    draft.reminder = DraftReminder::Before { minutes: 10 };
    draft.locks = vec![DraftField::Title];
    assert_eq!(app.core.save_event(draft, None, None, None).unwrap().message, "Saved Design review", "locks handed back are the core's to decide");
    assert_eq!(start_of(&week(&app, monday, here), "Design review"), at(monday, "15:30", here).timestamp());

    let row = week(&app, monday, here).into_iter().find(|o| o.title == "Design review").unwrap();
    assert_eq!(app.core.plan_delete(row.event_id, None).unwrap().refusal, None);
    let deleted = app.core.delete_event(row.event_id, None).unwrap();
    assert!(week(&app, monday, here).is_empty());
    app.core.undo_calendar(deleted.token.unwrap()).unwrap();
    assert_eq!(titles(&week(&app, monday, here)), ["Design review"], "Undo brings it back");
}

fn draft_copy(d: &jolta_ffi::editor::EventDraft) -> jolta_ffi::editor::EventDraft {
    jolta_ffi::editor::EventDraft {
        calendar_id: d.calendar_id,
        base: d.base.clone(),
        uid: d.uid.clone(),
        title: d.title.clone(),
        time: d.time.clone(),
        zone: d.zone.clone(),
        zone_text: d.zone_text.clone(),
        rule: d.rule.clone(),
        rule_was: d.rule_was.clone(),
        people: d.people.clone(),
        unresolved: d.unresolved.clone(),
        options: d.options,
        responses: d.responses.clone(),
        show_as_note: d.show_as_note.clone(),
        move_to: None,
        room: d.room.clone(),
        location: d.location.clone(),
        description: d.description.clone(),
        video: d.video,
        show_as: d.show_as,
        reminder: d.reminder,
        visibility: d.visibility,
        locks: d.locks.clone(),
        notice: d.notice.clone(),
        organizer: None,
        meeting: None,
        repeat: d.repeat.clone(),
        description_note: d.description_note.clone(),
    }
}

/// Quick create through the bridge: the preview marks what became which
/// field, the draft names the one Jan the user wrote to, holds the unclear
/// Els, and saves only once Els is picked. G reads a day.
#[test]
fn quick_create_saves_only_once_every_name_is_clear() {
    use jolta_ffi::editor::{DraftField, DraftPerson};
    use jolta_ffi::quick::QuickPartKind;

    let app = App::new();
    let me = app.account("me@example.net");
    app.deliver(&me, MailboxRole::Sent, &Mail::new("me@example.net", "Jan Peeters <jan@example.com>, Els Maes <els@example.com>, Els Claes <claes@example.com>", "Plans", "See you."));
    app.core.new_calendar_here("Personal".into(), None).unwrap();
    let en = vec!["en".to_string()];

    let text = "Lunch with Jan and Els tomorrow 12:30 at Mok";
    let preview = app.core.quick_parse(text.into(), en.clone()).unwrap();
    assert_eq!(preview.title, "Lunch with Jan and Els");
    assert!(preview.has_time && preview.when.ends_with(", 12:30 to 13:00"), "{}", preview.when);
    assert_eq!(preview.location.as_deref(), Some("Mok"));
    assert_eq!(preview.people.iter().map(|p| (p.typed.as_str(), p.person.as_ref().map(|p| p.address.as_str()))).collect::<Vec<_>>(), [("Jan", Some("jan@example.com")), ("Els", None)]);
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let marked = |kind| preview.parts.iter().filter(|p| p.kind == kind).map(|p| String::from_utf16(&utf16[p.start as usize..p.end as usize]).unwrap()).collect::<Vec<_>>();
    assert_eq!(marked(QuickPartKind::Person), ["Jan", "Els"]);
    assert_eq!(marked(QuickPartKind::When), ["tomorrow", "12:30"]);
    assert_eq!(marked(QuickPartKind::Place), ["at Mok"]);
    assert!(!preview.asks_ai, "AI is off");

    let q = block_on(app.core.quick_draft(text.into(), en.clone())).unwrap();
    assert!(!q.read_by_ai && !q.focus_time);
    let mut draft = q.draft;
    assert_eq!(draft.people.iter().map(|p| p.address.as_str()).collect::<Vec<_>>(), ["jan@example.com"]);
    assert_eq!(draft.unresolved.len(), 1);
    assert_eq!(draft.unresolved[0].typed, "Els");
    let problems = app.core.check_draft(draft_copy(&draft)).unwrap();
    assert!(problems.iter().any(|p| p.blocking && p.field == Some(DraftField::People)), "{problems:?}");
    assert!(app.core.save_event(draft_copy(&draft), None, None, None).is_err(), "nothing is saved while Els is unclear");

    let els = draft.unresolved.remove(0).choices.into_iter().find(|p| p.address == "els@example.com").unwrap();
    draft.people.push(DraftPerson { name: els.name, address: els.address, role: draft.people[0].role, response: draft.people[0].response, comment: String::new(), is_me: false });
    assert!(app.core.check_draft(draft_copy(&draft)).unwrap().iter().all(|p| !p.blocking));
    assert!(app.core.plan_save(draft_copy(&draft)).unwrap().dialog.is_some(), "a meeting with guests asks Send invitations? first");

    let q = block_on(app.core.quick_draft("Read chapter 12".into(), en.clone())).unwrap();
    assert!(q.focus_time, "no day or time: the editor puts the focus on the time");

    let here = tz::here();
    let tomorrow = (Utc::now().with_timezone(&here).date_naive() + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
    assert_eq!(app.core.go_to_date("morgen".into(), en.clone()).unwrap(), Some(tomorrow.clone()));
    assert_eq!(app.core.go_to_date("demain".into(), en.clone()).unwrap(), Some(tomorrow));
    assert_eq!(app.core.go_to_date("lunch".into(), en).unwrap(), None);
}
