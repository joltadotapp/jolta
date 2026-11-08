//! Focus time for the app: the palette's sheet, which starts at the next
//! free stretch and says what the calendar will get, and ⌥-drag on empty
//! time in the calendar. Times cross as unix seconds.

use chrono::{DateTime, Duration, Utc};
use jolta_core::calendar::focus::{self, Holds, Request};
use jolta_core::calendar::{AutoDecline, CalendarId, CalendarSource, draft, tz};
use jolta_core::store;

use crate::away::AwayDeclines;
use crate::editor::SavedEdit;
use crate::{Core, CoreError, Result};

/// Focus time as the sheet holds it.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FocusRequest {
    pub calendar_id: i64,
    pub start: i64,
    pub end: i64,
    /// What Google's focus time declines; other calendars decline nothing.
    pub declines: AwayDeclines,
    /// Google Chat's Do not disturb during it.
    pub do_not_disturb: bool,
}

/// What the sheet says about a request.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FocusPlan {
    /// "Google focus time that declines new invitations."
    pub line: String,
    /// Why it can't be added as it is.
    pub problem: Option<String>,
    /// The calendar holds Google's focus time, so the sheet offers what it
    /// declines and Do not disturb.
    pub declines: bool,
}

fn at(s: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp(s, 0).ok_or_else(|| CoreError::Failed { message: format!("{s} isn't a time") })
}

fn declines(d: AwayDeclines) -> AutoDecline {
    match d {
        AwayDeclines::Nothing => AutoDecline::None,
        AwayDeclines::NewInvitations => AutoDecline::New,
        AwayDeclines::Everything => AutoDecline::All,
    }
}

fn request(start: i64, end: i64, d: AwayDeclines, do_not_disturb: bool) -> Result<Request> {
    Ok(Request { from: at(start)?, until: at(end)?, declines: declines(d), do_not_disturb })
}

#[uniffi::export]
impl Core {
    /// What the palette's Focus time starts from: the next free stretch of
    /// `minutes` in the working hours, else the next hour, in the calendar
    /// new events go to.
    pub fn focus_draft(&self, minutes: i64) -> Result<FocusRequest> {
        let (here, now) = (tz::here(), Utc::now());
        let (calendar, free) = self.jolta.store.read(|c| Ok((draft::default_calendar(c)?, focus::next_free(c, minutes, now, here)?)))?;
        let calendar = calendar.ok_or_else(|| CoreError::Failed { message: "No calendar can take focus time. Turn on calendar for an account, or make a calendar on this Mac.".into() })?;
        let (start, end) = free.unwrap_or_else(|| {
            let next = DateTime::from_timestamp((now.timestamp() / 3600 + 1) * 3600, 0).unwrap_or(now);
            (next, next + Duration::minutes(minutes))
        });
        Ok(FocusRequest { calendar_id: calendar.id.0, start: start.timestamp(), end: end.timestamp(), declines: AwayDeclines::NewInvitations, do_not_disturb: false })
    }

    /// What the request's calendar will get, and why it can't, if it can't.
    pub fn focus_plan(&self, request: FocusRequest) -> Result<FocusPlan> {
        let (here, now) = (tz::here(), Utc::now());
        let r = self::request(request.start, request.end, request.declines, request.do_not_disturb)?;
        let (calendar, holds) = self.jolta.store.read(|c| {
            let calendar = store::calendar(c, CalendarId(request.calendar_id))?;
            let holds = focus::holds_in(c, &calendar, now)?;
            Ok((calendar, holds))
        })?;
        Ok(FocusPlan { line: focus::line(holds, &r, &calendar), problem: r.problem(now, here).map(str::to_string), declines: holds == Holds::Status && calendar.source == CalendarSource::Google })
    }

    /// Adds focus time to the calendar, telling nobody. The returned token
    /// undoes it.
    pub fn add_focus(&self, calendar_id: i64, start: i64, end: i64, declines: AwayDeclines, do_not_disturb: bool) -> Result<SavedEdit> {
        let (here, now) = (tz::here(), Utc::now());
        let r = request(start, end, declines, do_not_disturb)?;
        let horizon = jolta_core::calendar::series::Horizon::around(now, here);
        let id = CalendarId(calendar_id);
        let op = self.jolta.store.write(|tx| {
            let calendar = store::calendar(tx, id)?;
            focus::add(tx, &calendar, &r, now, &horizon)
        })?;
        self.edited(id);
        let (s, e) = (r.from.with_timezone(&here), r.until.with_timezone(&here));
        let uid = self.jolta.store.read(|c| Ok(store::calendar_op(c, op)?.uid))?;
        Ok(SavedEdit { token: Some(op.0), uid, message: format!("Focus time added for {}, {} to {}.", s.format("%a %-d %b"), s.format("%H:%M"), e.format("%H:%M")) })
    }
}
