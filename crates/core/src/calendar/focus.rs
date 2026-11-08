//! Focus time (C4 U7): time blocked the way each calendar understands it.
//!
//! Google's primary calendar gets Google's own focus time event, which
//! declines invitations and can set Chat to Do not disturb, where the
//! account's edition offers it. Google answers one it doesn't offer with a
//! 400 (`malformedFocusTimeEvent`, Google Workspace test account,
//! 2026-11-07); its writer then creates a plain busy event, and `landed`
//! records the refusal on the account, so later focus time is a busy event
//! from the start until `RETRY_DAYS` have passed. Every other calendar
//! gets a busy event with the Focus time category, which `EventKind::shown`
//! reads back as focus time. Google calendars hold no categories. Jolta
//! doesn't change Teams presence on Microsoft 365: that needs
//! `Presence.ReadWrite`, Teams and Jolta running when focus starts, and
//! Microsoft keeps "Focusing" for Viva Insights.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::availability::{self, SuggestOptions, WorkingHours};
use super::ops::{self, CalendarOp, CalendarOpId, Change, Edit, Extra, Write};
use super::series::Horizon;
use super::{AutoDecline, Calendar, CalendarSource, EventData, EventKind, Local, ShowAs, Span, Zone, freebusy};
use crate::{AccountId, Error, Result, store};

/// The title of every focus time event Jolta makes.
pub const TITLE: &str = "Focus time";

/// The category Microsoft 365 and CalDAV focus time events carry.
pub const CATEGORY: &str = "Focus time";

/// How long a refusal stands before Jolta offers Google focus time again,
/// in case the organization's edition gained the feature.
pub const RETRY_DAYS: i64 = 30;

/// What the user hears, once, after Google refused focus time.
pub const NOTICE: &str = "Your Google account doesn't offer focus time, so Jolta added a busy event.";

/// Focus time as the user asks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// What Google's focus time declines.
    pub declines: AutoDecline,
    /// Google Chat's Do not disturb during it.
    pub do_not_disturb: bool,
}

impl Request {
    /// Why it can't be added as it is. Focus time stays within one day on
    /// the user's clock: Google refuses all-day focus time, and a block
    /// past midnight is two days' focus.
    pub fn problem(&self, now: DateTime<Utc>, here: Tz) -> Option<&'static str> {
        let day = |t: DateTime<Utc>| t.with_timezone(&here).date_naive();
        if self.until <= self.from {
            Some("The end is before the start.")
        } else if self.until <= now {
            Some("That time is over already.")
        } else if day(self.from) != day(self.until - Duration::seconds(1)) {
            Some("Focus time can't run past midnight. Add it for each day.")
        } else {
            None
        }
    }
}

/// How a calendar holds focus time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holds {
    /// Google's own focus time event.
    Status,
    /// A busy event with the Focus time category.
    Category,
    /// A plain busy event on Google, and why.
    Busy(Plain),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plain {
    /// Google refused focus time for the account in the last `RETRY_DAYS`.
    NotOffered,
    /// Google keeps status events in the account's primary calendar only.
    NotPrimary,
}

/// How `calendar` holds focus time. `address` is its account's, and
/// `refused` when Google last refused focus time for that account.
pub fn holds(calendar: &Calendar, address: Option<&str>, refused: Option<i64>, now: DateTime<Utc>) -> Holds {
    if calendar.source != CalendarSource::Google {
        return Holds::Category;
    }
    if !address.is_some_and(|a| calendar.is_own(a)) {
        return Holds::Busy(Plain::NotPrimary);
    }
    match refused {
        Some(at) if now.timestamp() - at < RETRY_DAYS * 86_400 => Holds::Busy(Plain::NotOffered),
        _ => Holds::Status,
    }
}

/// The event and the provider fields focus time gets. Its UID is made when
/// it's queued.
#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub event: EventData,
    pub extra: Option<Extra>,
}

pub fn plan(holds: Holds, request: &Request, here: Tz) -> Planned {
    let at = |t: DateTime<Utc>| Local { wall: t.with_timezone(&here).naive_local(), zone: Zone::Tz(here) };
    let mut event = EventData::new("", Span::Timed { start: at(request.from), end: at(request.until) });
    event.title = TITLE.into();
    event.show_as = ShowAs::Busy;
    let extra = match holds {
        Holds::Status => {
            event.kind = EventKind::Focus { declines: request.declines, do_not_disturb: request.do_not_disturb, message: String::new() };
            // Google's status events guide asks for opaque focus time,
            // which a busy event's body otherwise leaves to its default.
            Some(object(json!({
                "eventType": "focusTime",
                "transparency": "opaque",
                "focusTimeProperties": {
                    "autoDeclineMode": decline_mode(request.declines),
                    "chatStatus": if request.do_not_disturb { "doNotDisturb" } else { "available" },
                },
            })))
        }
        Holds::Category => {
            event.categories = vec![CATEGORY.into()];
            None
        }
        Holds::Busy(_) => None,
    };
    Planned { event, extra }
}

/// What the sheet says will happen.
pub fn line(holds: Holds, request: &Request, calendar: &Calendar) -> String {
    match holds {
        Holds::Status => {
            let declines = match request.declines {
                AutoDecline::New => "declines new invitations",
                AutoDecline::All => "declines new invitations and cancels your meetings then",
                AutoDecline::None => "declines nothing",
            };
            let chat = if request.do_not_disturb { " Google Chat is set to Do not disturb." } else { "" };
            format!("Google focus time that {declines}.{chat}")
        }
        Holds::Busy(Plain::NotOffered) => "A busy event: your Google account doesn't offer focus time.".into(),
        Holds::Busy(Plain::NotPrimary) => "A busy event: Google keeps focus time only in your main calendar.".into(),
        Holds::Category if calendar.source == CalendarSource::Graph => "A busy event with the Focus time category. Your Teams status doesn't change.".into(),
        Holds::Category => "A busy event with the Focus time category.".into(),
    }
}

fn object(v: Value) -> Extra {
    match v {
        Value::Object(map) => Extra::Json(map),
        _ => unreachable!("built as an object"),
    }
}

fn decline_mode(d: AutoDecline) -> &'static str {
    match d {
        AutoDecline::None => "declineNone",
        AutoDecline::New => "declineOnlyNewConflictingInvitations",
        AutoDecline::All => "declineAllConflictingInvitations",
    }
}

/// How `calendar` holds focus time now, by what the store knows of its
/// account.
pub fn holds_in(conn: &Connection, calendar: &Calendar, now: DateTime<Utc>) -> Result<Holds> {
    let (address, refused) = match calendar.account_id {
        Some(id) => (Some(store::account(conn, id)?.address), store::focus_refused(conn, id)?),
        None => (None, None),
    };
    Ok(holds(calendar, address.as_deref(), refused, now))
}

/// Queues focus time in `calendar`, telling nobody. Returns the op, which
/// Undo takes back.
pub fn add(conn: &Connection, calendar: &Calendar, request: &Request, now: DateTime<Utc>, horizon: &Horizon) -> Result<CalendarOpId> {
    if let Some(problem) = request.problem(now, horizon.here) {
        return Err(Error::Other(problem.into()));
    }
    let planned = plan(holds_in(conn, calendar, now)?, request, horizon.here);
    let event = EventData { uid: ops::new_uid()?, ..planned.event };
    let edit = Edit { extra: planned.extra, ..Edit::quiet(calendar.id, Change::Create(vec![event])) };
    ops::queue(conn, &edit, now.timestamp(), horizon)?.ok_or_else(|| Error::Other("Jolta couldn't add the focus time.".into()))
}

/// After a create reached the server: when it asked Google for focus time,
/// records whether Google kept it as one. Returns the notice the first time
/// Google refuses for the account.
pub fn landed(conn: &Connection, op: &CalendarOp, made: &[EventData], account: Option<AccountId>, now: i64) -> Result<Option<&'static str>> {
    let asked = matches!(&op.extra, Some(Extra::Json(map)) if map.get("eventType").and_then(Value::as_str) == Some("focusTime"));
    let (true, Write::Create(_), Some(account)) = (asked, &op.write, account) else { return Ok(None) };
    let Some(event) = made.iter().find(|e| e.recurrence_id.is_none()) else { return Ok(None) };
    if matches!(event.kind, EventKind::Focus { .. }) {
        store::set_focus_refused(conn, account, None)?;
        return Ok(None);
    }
    let before = store::focus_refused(conn, account)?;
    store::set_focus_refused(conn, account, Some(now))?;
    Ok(before.is_none().then_some(NOTICE))
}

/// The next free stretch of `minutes` in the user's working hours, from the
/// stored occurrences of every calendar that counts as busy: what the
/// palette's Focus time offers first.
pub fn next_free(conn: &Connection, minutes: i64, now: DateTime<Utc>, here: Tz) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
    const DAYS: u32 = 10;
    let hours = store::working_hours(conn)?.unwrap_or_else(|| WorkingHours::standard(here));
    let windows = hours.windows(now, DAYS);
    let (Some(first), Some(last)) = (windows.first(), windows.last()) else { return Ok(None) };
    let (from_day, to_day) = (first.start.with_timezone(&here).date_naive(), last.end.with_timezone(&here).date_naive() + Duration::days(1));
    let own: Vec<_> = store::occurrences(conn, from_day, to_day, here)?.iter().filter_map(|o| freebusy::stored_busy(o, here)).collect();
    let slots = availability::suggest(&own, &[], &hours, Duration::minutes(minutes), now, DAYS, SuggestOptions { respect_others_hours: false });
    let day = |t: DateTime<Utc>| t.with_timezone(&here).date_naive();
    Ok(slots.into_iter().map(|s| (s.start, s.end)).find(|(s, e)| day(*s) == day(*e - Duration::seconds(1))))
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDateTime, TimeZone};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::Access;
    use crate::calendar::ops::Sent;
    use crate::calendar::sources::Version;
    use crate::model::AccountKind;
    use crate::store::{NewAccount, RemoteCalendar, Store};

    fn utc(s: &str) -> DateTime<Utc> {
        Brussels.from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).unwrap().with_timezone(&Utc)
    }

    fn request(from: &str, until: &str) -> Request {
        Request { from: utc(from), until: utc(until), declines: AutoDecline::New, do_not_disturb: true }
    }

    fn calendar(source: CalendarSource, remote_id: &str) -> Calendar {
        Calendar {
            id: super::super::CalendarId(1),
            account_id: Some(AccountId(1)),
            source,
            remote_id: remote_id.into(),
            name: "Calendar".into(),
            color: None,
            zone: None,
            access: Access::Owner,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: true,
            cursor: None,
            refresh: None,
            synced: None,
            video: None,
        }
    }

    const ME: &str = "me@example.com";

    #[test]
    fn each_source_gets_what_it_can_hold() {
        let now = utc("2026-11-17 09:00");
        let r = request("2026-11-18 14:00", "2026-11-18 16:00");

        let google = calendar(CalendarSource::Google, ME);
        assert_eq!(holds(&google, Some(ME), None, now), Holds::Status);
        let p = plan(Holds::Status, &r, Brussels);
        let Some(Extra::Json(extra)) = &p.extra else { panic!("Google's fields") };
        assert_eq!(extra["eventType"], "focusTime");
        assert_eq!(extra["focusTimeProperties"], json!({ "autoDeclineMode": "declineOnlyNewConflictingInvitations", "chatStatus": "doNotDisturb" }));
        assert_eq!((p.event.title.as_str(), p.event.show_as), (TITLE, ShowAs::Busy), "opaque, as Google asks focus time to be");
        assert_eq!(p.event.kind, EventKind::Focus { declines: AutoDecline::New, do_not_disturb: true, message: String::new() });
        assert!(p.event.categories.is_empty(), "Google holds no categories");
        let Span::Timed { start, end } = p.event.span else { panic!("Google refuses all-day focus time") };
        assert_eq!((start.instant(Brussels), end.instant(Brussels)), (r.from, r.until));
        assert!(p.event.attendees.is_empty() && p.event.alarms.is_empty());
        assert_eq!(line(Holds::Status, &r, &google), "Google focus time that declines new invitations. Google Chat is set to Do not disturb.");
        let quiet = Request { declines: AutoDecline::None, do_not_disturb: false, ..r };
        let Some(Extra::Json(extra)) = plan(Holds::Status, &quiet, Brussels).extra else { panic!() };
        assert_eq!(extra["focusTimeProperties"], json!({ "autoDeclineMode": "declineNone", "chatStatus": "available" }));

        let shared = calendar(CalendarSource::Google, "team@group.calendar.google.com");
        assert_eq!(holds(&shared, Some(ME), None, now), Holds::Busy(Plain::NotPrimary), "status events live in the primary calendar alone");
        let p = plan(Holds::Busy(Plain::NotPrimary), &r, Brussels);
        assert_eq!((p.extra, p.event.kind, p.event.categories.len()), (None, EventKind::Default, 0));

        let graph = calendar(CalendarSource::Graph, "AAMk");
        assert_eq!(holds(&graph, Some(ME), None, now), Holds::Category);
        let p = plan(Holds::Category, &r, Brussels);
        assert_eq!((p.extra, p.event.show_as, p.event.categories.clone()), (None, ShowAs::Busy, vec![CATEGORY.to_string()]));
        assert_eq!(p.event.kind.shown(p.event.show_as, &p.event.categories).line().as_deref(), Some("Focus time"), "read back as focus time");
        assert!(line(Holds::Category, &r, &graph).contains("Teams status doesn't change"));

        for source in [CalendarSource::CalDav, CalendarSource::Local] {
            assert_eq!(holds(&calendar(source, "x"), None, None, now), Holds::Category);
        }
    }

    #[test]
    fn a_refusal_stands_for_thirty_days() {
        let google = calendar(CalendarSource::Google, ME);
        let refused = utc("2026-11-01 10:00").timestamp();
        assert_eq!(holds(&google, Some(ME), Some(refused), utc("2026-11-30 10:00")), Holds::Busy(Plain::NotOffered));
        assert_eq!(holds(&google, Some(ME), Some(refused), utc("2026-12-01 10:01")), Holds::Status, "asked again a month later");
        assert_eq!(line(Holds::Busy(Plain::NotOffered), &request("2026-11-18 14:00", "2026-11-18 16:00"), &google), "A busy event: your Google account doesn't offer focus time.");
    }

    #[test]
    fn a_request_must_fit_in_one_day_and_be_ahead() {
        let now = utc("2026-11-17 09:00");
        assert_eq!(request("2026-11-18 14:00", "2026-11-18 16:00").problem(now, Brussels), None);
        assert_eq!(request("2026-11-18 22:00", "2026-11-19 00:00").problem(now, Brussels), None, "ending at midnight stays in the day");
        assert_eq!(request("2026-11-18 23:00", "2026-11-19 01:00").problem(now, Brussels), Some("Focus time can't run past midnight. Add it for each day."));
        assert_eq!(request("2026-11-18 16:00", "2026-11-18 14:00").problem(now, Brussels), Some("The end is before the start."));
        assert_eq!(request("2026-11-16 14:00", "2026-11-16 16:00").problem(now, Brussels), Some("That time is over already."));
        assert_eq!(request("2026-11-17 08:00", "2026-11-17 10:00").problem(now, Brussels), None, "focus from now on");
    }

    fn store(name: &str) -> (Store, Calendar) {
        let dir = std::env::temp_dir().join(format!("jolta-focus-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        let calendar = s
            .write(|tx| {
                let a = store::insert_account(tx, &NewAccount { kind: AccountKind::Gmail, address: ME.into(), display_name: String::new(), username: ME.into(), imap_host: "imap.gmail.com".into(), imap_port: 993, smtp_host: "smtp.gmail.com".into(), smtp_port: 465 })?;
                let list = [RemoteCalendar { remote_id: ME.into(), name: ME.into(), color: None, zone: None, access: Access::Owner, primary: true, refresh: None, video: None }];
                Ok(store::sync_calendar_list(tx, a.id, CalendarSource::Google, &list)?.remove(0))
            })
            .unwrap();
        (s, calendar)
    }

    fn horizon() -> Horizon {
        Horizon::around(utc("2026-11-17 09:00"), Brussels)
    }

    /// Sends the claimed op as Google would answer it: as focus time when
    /// `offered`, else as the plain busy event the writer falls back to.
    fn send(s: &Store, offered: bool) -> ops::Settled {
        let now = utc("2026-11-17 09:00").timestamp();
        let op = s.write(|tx| store::claim_calendar_ops(tx, now, 1)).unwrap().remove(0);
        let Write::Create(events) = &op.write else { panic!("a create") };
        let mut made = events[0].clone();
        if !offered {
            made.kind = EventKind::Default;
        }
        let version = Version { remote_id: op.remote_key.clone(), etag: Some("1".into()), events: vec![made], raw: None, instances: Vec::new() };
        s.write(|tx| ops::settle(tx, &op, Ok(Sent::Done { version: Some((version, None)), merged: false }), now, &horizon())).unwrap()
    }

    fn extra(s: &Store, op: CalendarOpId) -> Option<Extra> {
        s.read(|c| store::calendar_op(c, op)).unwrap().extra
    }

    /// Check 4: the refusal is learnt once and told once; the next focus
    /// time goes as a busy event without asking Google again.
    #[test]
    fn google_refusing_focus_time_is_remembered_and_told_once() {
        let (s, google) = store("refused");
        let now = utc("2026-11-17 09:00");
        let r = request("2026-11-18 14:00", "2026-11-18 16:00");
        let op = s.write(|tx| add(tx, &google, &r, now, &horizon())).unwrap();
        assert!(matches!(extra(&s, op), Some(Extra::Json(m)) if m["eventType"] == "focusTime"), "first asked as Google's focus time");
        let settled = send(&s, false);
        assert_eq!((settled.failure, settled.notice), (None, Some(NOTICE.to_string())));
        assert_eq!(s.read(|c| store::focus_refused(c, google.account_id.unwrap())).unwrap(), Some(now.timestamp()));

        let op = s.write(|tx| add(tx, &google, &request("2026-11-19 14:00", "2026-11-19 16:00"), now, &horizon())).unwrap();
        assert_eq!(extra(&s, op), None, "a busy event from the start");
        assert_eq!(send(&s, false).notice, None, "not asked, so nothing to tell");

        s.write(|tx| store::set_focus_refused(tx, google.account_id.unwrap(), Some(now.timestamp() - RETRY_DAYS * 86_400))).unwrap();
        let op = s.write(|tx| add(tx, &google, &request("2026-11-20 14:00", "2026-11-20 16:00"), now, &horizon())).unwrap();
        assert!(extra(&s, op).is_some(), "asked again after thirty days");
        assert_eq!(send(&s, false).notice, None, "a second refusal isn't told again");
    }

    #[test]
    fn google_taking_focus_time_forgets_an_old_refusal() {
        let (s, google) = store("taken");
        let now = utc("2026-11-17 09:00");
        let account = google.account_id.unwrap();
        s.write(|tx| store::set_focus_refused(tx, account, Some(now.timestamp() - 40 * 86_400))).unwrap();
        s.write(|tx| add(tx, &google, &request("2026-11-18 14:00", "2026-11-18 16:00"), now, &horizon())).unwrap();
        assert_eq!(send(&s, true).notice, None);
        assert_eq!(s.read(|c| store::focus_refused(c, account)).unwrap(), None);
        let err = s.write(|tx| add(tx, &google, &request("2026-11-18 23:00", "2026-11-19 01:00"), now, &horizon())).unwrap_err();
        assert!(err.to_string().contains("past midnight"), "{err}");
    }

    #[test]
    fn the_next_free_two_hours_skip_what_is_busy() {
        let (s, google) = store("free");
        let now = utc("2026-11-17 08:00");
        let busy = |from: &str, until: &str| Request { from: utc(from), until: utc(until), declines: AutoDecline::None, do_not_disturb: false };
        s.write(|tx| {
            add(tx, &google, &busy("2026-11-17 09:00", "2026-11-17 10:00"), now, &horizon())?;
            add(tx, &google, &busy("2026-11-17 11:30", "2026-11-17 16:00"), now, &horizon())
        })
        .unwrap();
        let (start, end) = s.read(|c| next_free(c, 120, now, Brussels)).unwrap().unwrap();
        assert_eq!((start, end), (utc("2026-11-18 09:00"), utc("2026-11-18 11:00")), "10:00 to 11:30 and 16:00 to 17:00 are too short");
        let (start, _) = s.read(|c| next_free(c, 60, now, Brussels)).unwrap().unwrap();
        assert_eq!(start, utc("2026-11-17 10:00"));
    }
}
