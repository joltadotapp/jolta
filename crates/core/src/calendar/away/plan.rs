//! Setting out of office, and turning it off early (C4 U4).
//!
//! One request covers several accounts, and each provider gets what it
//! can hold. Microsoft 365 gets scheduled automatic replies, which block
//! nothing in the calendar, so also an out-of-office event; Graph can't
//! decline invitations for the user. Google gets an out-of-office event
//! that declines as asked; its Gmail auto-reply needs a scope Jolta
//! doesn't ask for, so the sheet links to Gmail. CalDAV calendars get an
//! event Outlook reads as out of office. The events go through the
//! calendar op queue and carry Jolta's marker, so any Mac can find them.
//!
//! `plan` and `ending` are pure: what each account gets, and what Turn off
//! ends. `apply` and `end` do it.

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Timelike, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::{Audience, AutoReply, Away};
use crate::calendar::ops::{self, Change, Edit, Extra};
use crate::calendar::series::Horizon;
use crate::calendar::{AutoDecline, Calendar, CalendarId, CalendarSource, EventData, EventKind, Local, ShowAs, Span, Zone, caps, meeting, tz};
use crate::{Account, AccountId, AccountKind, Jolta, Result, store};

/// The title of every out-of-office event Jolta makes.
pub const TITLE: &str = "Out of office";

/// The value of Jolta's hold marker (C4 U9) that names an out-of-office
/// event. Sync doesn't read markers yet, so Turn off finds the events by
/// what they show as (`away_events`).
const MARK: &str = "away";
const GOOGLE_MARK: &str = "joltaHold";
const GRAPH_MARK: &str = "String {9d3b2d55-5a0e-4a6e-9b4e-6a6f6c746130} Name JoltaHold";
const CALDAV_MARK: &str = "X-JOLTA-HOLD";

/// When the user is away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Absence {
    /// Whole days on the user's clock, the last one included.
    Days { first: NaiveDate, last: NaiveDate },
    Times { from: DateTime<Utc>, until: DateTime<Utc> },
}

impl Absence {
    /// When it starts and ends. Whole days run from midnight on the first
    /// to midnight after the last on `here`, which is 23 or 25 hours on a
    /// day the clocks change.
    pub fn instants(&self, here: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
        match *self {
            Absence::Days { first, last } => (midnight(first, here), midnight(last + Duration::days(1), here)),
            Absence::Times { from, until } => (from, until),
        }
    }

    /// As an event that may be all day.
    fn span(&self, here: Tz) -> Span {
        match *self {
            Absence::Days { first, last } => Span::AllDay { start: first, end: last + Duration::days(1) },
            Absence::Times { .. } => self.timed(here),
        }
    }

    /// As a timed event on `here`'s clock: Google refuses all-day
    /// out-of-office events.
    fn timed(&self, here: Tz) -> Span {
        let (from, until) = self.instants(here);
        let at = |t: DateTime<Utc>| Local { wall: t.with_timezone(&here).naive_local(), zone: Zone::Tz(here) };
        Span::Timed { start: at(from), end: at(until) }
    }
}

fn midnight(day: NaiveDate, here: Tz) -> DateTime<Utc> {
    tz::instant(day.and_time(NaiveTime::MIN), here)
}

/// Out of office as the sheet asks for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub accounts: Vec<AccountId>,
    pub absence: Absence,
    /// The reply as the user typed it.
    pub message: String,
    /// Who outside the organization gets the reply on Microsoft 365.
    pub audience: Audience,
    /// What Google's out-of-office event declines.
    pub declines: AutoDecline,
}

impl Request {
    /// Why the request can't be set as it is: it ends before it starts, or
    /// it is over already.
    pub fn problem(&self, now: DateTime<Utc>, here: Tz) -> Option<&'static str> {
        let (from, until) = self.absence.instants(here);
        if until <= from {
            Some("The end is before the start.")
        } else if until <= now {
            Some("That time is over already.")
        } else {
            None
        }
    }
}

/// An account and the calendar its out-of-office event would go in.
#[derive(Clone, Debug)]
pub struct Target {
    pub account: Account,
    pub calendar: Option<Calendar>,
}

/// Why an account can't take out of office from Jolta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// A Google or Microsoft 365 account with calendar off: Jolta has no
    /// grant to write its calendar or mailbox settings.
    CalendarOff,
    /// No calendar of the account's own that Jolta can write.
    NoCalendar,
}

/// The automatic replies Microsoft 365 gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replies {
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
    /// HTML, as Outlook keeps it.
    pub message: String,
    pub audience: Audience,
}

/// The event an account's calendar gets. Its UID is made when it's queued.
#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub calendar: CalendarId,
    pub event: EventData,
    pub extra: Extra,
}

/// What one account gets.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub account: AccountId,
    pub replies: Option<Replies>,
    pub event: Option<Planned>,
    /// What the sheet says will happen.
    pub line: String,
    /// The provider's own setting, for what Jolta can't change.
    pub link: Option<(String, String)>,
    pub blocked: Option<Blocked>,
    /// Ticked when the sheet opens: accounts where Jolta does more than add
    /// a busy event.
    pub ticked: bool,
}

/// The account's own calendars Jolta could put its out-of-office event in,
/// for every account that has calendars.
pub fn targets(conn: &Connection) -> Result<Vec<Target>> {
    let mut out = Vec::new();
    for account in store::accounts(conn)? {
        let calendars = store::account_calendars(conn, account.id)?;
        let mut own: Vec<Calendar> = calendars.into_iter().filter(|c| c.is_own(&account.address) && caps::of(c).writable).collect();
        // Google takes out-of-office events only in the primary calendar,
        // the one `is_own` names. Elsewhere the calendar the account's
        // events go in by default, else one shown.
        own.sort_by_key(|c| (!c.is_default, !c.shown, c.name != "Calendar", c.id));
        let calendar = own.into_iter().next();
        if account.kind == AccountKind::Imap && !account.calendar && calendar.is_none() {
            continue;
        }
        out.push(Target { account, calendar });
    }
    Ok(out)
}

/// What each account gets for the request, in `targets`' order.
pub fn plan(targets: &[Target], request: &Request, here: Tz) -> Vec<Step> {
    targets.iter().map(|t| step(t, request, here)).collect()
}

fn step(target: &Target, request: &Request, here: Tz) -> Step {
    let a = &target.account;
    let blank = Step { account: a.id, replies: None, event: None, line: String::new(), link: None, blocked: None, ticked: false };
    if !a.calendar && matches!(a.kind, AccountKind::Graph | AccountKind::Gmail) {
        let line = match a.kind {
            AccountKind::Graph => "Turn on calendar for this account, and Jolta can set its automatic replies and an out-of-office event.",
            _ => "Turn on calendar for this account, and Jolta can add an out-of-office event that declines invitations.",
        };
        return Step { line: line.into(), blocked: Some(Blocked::CalendarOff), ..blank };
    }
    let Some(calendar) = &target.calendar else {
        return Step { line: "Jolta has no calendar of this account's own to put an out-of-office event in.".into(), blocked: Some(Blocked::NoCalendar), ..blank };
    };
    let mut event = EventData::new("", request.absence.span(here));
    event.title = TITLE.into();
    event.show_as = ShowAs::OutOfOffice;
    let all_day = matches!(request.absence, Absence::Days { .. });
    match calendar.source {
        CalendarSource::Graph => {
            let (from, until) = request.absence.instants(here);
            let extra = json!({ "singleValueExtendedProperties": [{ "id": GRAPH_MARK, "value": MARK }] });
            Step {
                replies: Some(Replies { from, until, message: html(&request.message), audience: request.audience }),
                event: Some(Planned { calendar: calendar.id, event, extra: object(extra) }),
                line: format!(
                    "Automatic replies on, to {}. An out-of-office event in your calendar. Microsoft 365 doesn't let apps decline new invitations; Outlook on the web can.",
                    match request.audience {
                        Audience::All => "people in your organization and outside it",
                        Audience::Contacts => "people in your organization and your contacts",
                        Audience::Organization => "people in your organization only",
                    }
                ),
                link: Some(("Open Outlook settings".into(), "https://outlook.office.com/mail/options/calendar/AutomaticReplies".into())),
                ticked: true,
                ..blank
            }
        }
        CalendarSource::Google => {
            event.span = request.absence.timed(here);
            event.kind = EventKind::OutOfOffice { declines: request.declines, message: request.message.clone() };
            let extra = json!({
                "eventType": "outOfOffice",
                "outOfOfficeProperties": { "autoDeclineMode": decline_mode(request.declines), "declineMessage": request.message },
                "extendedProperties": { "private": { GOOGLE_MARK: MARK } },
            });
            let declines = match request.declines {
                AutoDecline::New => "declines new invitations for those days, with your reply as the note",
                AutoDecline::All => "declines new invitations and your meetings for those days, with your reply as the note",
                AutoDecline::None => "declines nothing",
            };
            Step {
                event: Some(Planned { calendar: calendar.id, event, extra: object(extra) }),
                line: format!("An out-of-office event that {declines}. The Gmail auto-reply is set in Gmail."),
                link: Some(("Open Gmail settings".into(), format!("https://mail.google.com/mail/u/?authuser={}#settings/general", a.address))),
                ticked: true,
                ..blank
            }
        }
        _ => {
            let icloud = a.provider_label() == "iCloud";
            let what = if all_day { "A busy all-day event only." } else { "A busy event only." };
            Step {
                event: Some(Planned { calendar: calendar.id, event, extra: Extra::Lines(vec![format!("{CALDAV_MARK}:{MARK}")]) }),
                line: if icloud { format!("{what} iCloud's auto-reply is set on icloud.com.") } else { what.into() },
                link: icloud.then(|| ("Open iCloud Mail".into(), "https://www.icloud.com/mail/".into())),
                ..blank
            }
        }
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

/// The reply as Outlook keeps it: the text escaped, its lines kept.
fn html(text: &str) -> String {
    let escaped = text.trim().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    escaped.lines().collect::<Vec<_>>().join("<br>")
}

/// The reply Outlook keeps, as text for the sheet.
pub fn text(html: &str) -> String {
    let plain = meeting::plain_text(html).replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
    let lines: Vec<&str> = plain.lines().map(str::trim_end).collect();
    let mut out: Vec<&str> = Vec::new();
    for line in lines {
        if line.trim().is_empty() && out.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

/// Graph's `PATCH /me/mailboxSettings` body for the replies.
pub fn replies_body(r: &Replies) -> Value {
    let at = |t: DateTime<Utc>| json!({ "dateTime": t.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" });
    json!({ "automaticRepliesSetting": {
        "status": "scheduled",
        "scheduledStartDateTime": at(r.from),
        "scheduledEndDateTime": at(r.until),
        "internalReplyMessage": r.message,
        "externalReplyMessage": r.message,
        "externalAudience": match r.audience {
            Audience::All => "all",
            Audience::Contacts => "contactsOnly",
            Audience::Organization => "none",
        },
    } })
}

/// Graph's body that turns the replies off and keeps their window and
/// messages for next time.
pub fn disabled_body() -> Value {
    json!({ "automaticRepliesSetting": { "status": "disabled" } })
}

/// An out-of-office event in one of the account's calendars.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub calendar: CalendarId,
    pub event: EventData,
}

/// Guest-less single events in the account's own calendars that show as
/// out of office or carry Jolta's title, and end after `now`. Google keeps
/// the busy event it gets where it refuses status events by its title.
pub fn away_events(conn: &Connection, account: &Account, now: DateTime<Utc>, here: Tz) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    for c in store::account_calendars(conn, account.id)? {
        if !c.is_own(&account.address) {
            continue;
        }
        let mut stmt = conn.prepare_cached(
            "SELECT data FROM event WHERE calendar_id = ?1 AND recurrence_id = '' AND status <> 'cancelled'
                 AND (json_extract(data, '$.show_as') = 'oof' OR title = ?2)",
        )?;
        let rows = stmt.query_map(params![c.id, TITLE], |r| r.get::<_, String>(0))?;
        for data in rows {
            let Ok(event) = serde_json::from_str::<EventData>(&data?) else { continue };
            let guests = event.attendees.iter().any(|g| !g.is_me);
            if guests || event.recurrence.is_some() || window(&event.span, here).1 <= now {
                continue;
            }
            out.push(Found { calendar: c.id, event });
        }
    }
    out.sort_by_key(|f| window(&f.event.span, here).0);
    Ok(out)
}

/// When an event starts and ends, all-day ones from midnight on `here`.
fn window(span: &Span, here: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    match span {
        Span::AllDay { start, end } => (midnight(*start, here), midnight(*end, here)),
        Span::Timed { start, end } => (start.instant(here), end.instant(here)),
    }
}

/// How far Turn off reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Only what is on now: the strip's Turn off, which leaves replies set
    /// for later alone, as when they were set elsewhere since it read.
    Now,
    /// What is on now, else the next one set: the sheet's and Settings'.
    NowOrNext,
}

/// What Turn off changes for one account.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ending {
    pub replies_off: bool,
    pub changes: Vec<(CalendarId, Change)>,
}

impl Ending {
    pub fn is_empty(&self) -> bool {
        !self.replies_off && self.changes.is_empty()
    }
}

/// What Turn off ends: the replies and events on now, shortened to end
/// now; with `NowOrNext` and nothing on, the next out of office set, with
/// whatever overlaps it, deleted. `replies` is what Microsoft 365 says now.
pub fn ending(replies: Option<AutoReply>, events: &[Found], reach: Reach, now: DateTime<Utc>, here: Tz) -> Ending {
    let window_of = |r: AutoReply| match r {
        AutoReply::On { from, until } => Some((from.unwrap_or(DateTime::<Utc>::MIN_UTC), until.unwrap_or(DateTime::<Utc>::MAX_UTC))),
        AutoReply::Off => None,
    };
    let reply = replies.and_then(window_of).filter(|(_, until)| *until > now);
    let on = |(from, until): (DateTime<Utc>, DateTime<Utc>)| from <= now && now < until;
    let current: Vec<&Found> = events.iter().filter(|f| on(window(&f.event.span, here))).collect();
    if reply.is_some_and(on) || !current.is_empty() {
        return Ending {
            replies_off: reply.is_some_and(on),
            changes: current.into_iter().map(|f| (f.calendar, shortened(&f.event, now, here))).collect(),
        };
    }
    if reach == Reach::Now {
        return Ending::default();
    }
    let next = reply.into_iter().chain(events.iter().map(|f| window(&f.event.span, here))).min_by_key(|(from, _)| *from);
    let Some((from, until)) = next else { return Ending::default() };
    let overlaps = |(a, b): (DateTime<Utc>, DateTime<Utc>)| a < until && from < b;
    Ending {
        replies_off: reply.is_some_and(overlaps),
        changes: events
            .iter()
            .filter(|f| overlaps(window(&f.event.span, here)))
            .map(|f| (f.calendar, Change::Delete { uid: f.event.uid.clone() }))
            .collect(),
    }
}

/// The event cut to end at `now`'s minute, or deleted when that leaves
/// nothing of it. An all-day event becomes timed from midnight on its
/// first day, so the rest of today shows free.
fn shortened(e: &EventData, now: DateTime<Utc>, here: Tz) -> Change {
    let now = now.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(now);
    let (start, _) = window(&e.span, here);
    if now <= start {
        return Change::Delete { uid: e.uid.clone() };
    }
    let span = match e.span {
        Span::AllDay { start, .. } => {
            Span::Timed { start: Local { wall: start.and_time(NaiveTime::MIN), zone: Zone::Tz(here) }, end: Local { wall: now.with_timezone(&here).naive_local(), zone: Zone::Tz(here) } }
        }
        Span::Timed { start, end } => Span::Timed { start, end: Local { wall: tz::wall(now, end.zone, here), zone: end.zone } },
    };
    Change::Update(EventData { span, ..e.clone() })
}

/// The window an account is out of office for now or next, by its
/// replies on Microsoft 365 and its out-of-office events: for the sheet.
pub fn current(replies: Option<AutoReply>, events: &[Found], now: DateTime<Utc>, here: Tz) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let reply = match replies {
        Some(AutoReply::On { from, until }) => Some((from.unwrap_or(now), until.unwrap_or(DateTime::<Utc>::MAX_UTC))),
        _ => None,
    };
    reply.into_iter().chain(events.iter().map(|f| window(&f.event.span, here))).filter(|(_, until)| *until > now).min_by_key(|(from, _)| *from)
}

/// What setting out of office did for one account.
#[derive(Debug)]
pub struct Applied {
    pub account: AccountId,
    pub result: Result<()>,
}

/// Sets out of office for each step's account: Microsoft 365's replies
/// first, then the event, queued. An out-of-office event of the account's
/// that overlaps the new one is moved to it instead, so setting the same
/// days twice leaves one event. Returns each account's outcome and the
/// calendars whose events changed.
pub async fn apply(jolta: &Jolta, steps: &[Step], horizon: &Horizon, here: Tz) -> (Vec<Applied>, Vec<CalendarId>) {
    let mut done = Vec::new();
    let mut calendars = Vec::new();
    for s in steps.iter().filter(|s| s.blocked.is_none()) {
        let result = apply_one(jolta, s, horizon, here).await;
        if result.is_ok()
            && let Some(p) = &s.event
        {
            calendars.push(p.calendar);
        }
        done.push(Applied { account: s.account, result });
    }
    (done, calendars)
}

async fn apply_one(jolta: &Jolta, s: &Step, horizon: &Horizon, here: Tz) -> Result<()> {
    let id = s.account;
    let account = jolta.store.read_async(move |c| store::account(c, id)).await?;
    if let Some(r) = &s.replies {
        let away = super::write_replies(jolta, &account, &replies_body(r)).await?;
        let fetched = Utc::now().timestamp_millis();
        jolta.store.write_async(move |tx| store::save_away(tx, &away, fetched)).await?;
    }
    let Some(planned) = s.event.clone() else { return Ok(()) };
    let now = Utc::now();
    let horizon = horizon.clone();
    jolta
        .store
        .write_async(move |tx| {
            let (from, until) = window(&planned.event.span, here);
            let overlapping = away_events(tx, &account, now, here)?
                .into_iter()
                .find(|f| f.calendar == planned.calendar && reusable(&f.event, &planned.event) && { let (a, b) = window(&f.event.span, here); a < until && from < b });
            let edit = match overlapping {
                Some(f) => {
                    let event = EventData { span: planned.event.span, kind: kind_for(&f.event, &planned.event), ..f.event };
                    Edit { extra: update_extra(&planned.extra, &event), ..Edit::quiet(planned.calendar, Change::Update(event)) }
                }
                None => {
                    let event = EventData { uid: ops::new_uid()?, ..planned.event };
                    Edit { extra: Some(planned.extra), ..Edit::quiet(planned.calendar, Change::Create(vec![event])) }
                }
            };
            ops::queue(tx, &edit, now.timestamp(), &horizon).map(drop)
        })
        .await
}

/// Whether an existing event can become the planned one by its time alone:
/// it shows as out of office, and on Google it is a status event, which a
/// patch can't make of a plain one.
fn reusable(existing: &EventData, planned: &EventData) -> bool {
    existing.show_as == ShowAs::OutOfOffice && (planned.kind.is_default() || matches!(existing.kind, EventKind::OutOfOffice { .. }))
}

/// The kind an existing event keeps when it moves to the new days: Google's
/// out of office takes the new reply and declines.
fn kind_for(existing: &EventData, planned: &EventData) -> EventKind {
    match (&existing.kind, &planned.kind) {
        (EventKind::OutOfOffice { .. }, EventKind::OutOfOffice { .. }) => planned.kind.clone(),
        _ => existing.kind.clone(),
    }
}

/// What an update of an existing event sends beside its fields: Google's
/// out-of-office properties, when it is one.
fn update_extra(planned: &Extra, event: &EventData) -> Option<Extra> {
    let (Extra::Json(map), EventKind::OutOfOffice { .. }) = (planned, &event.kind) else { return None };
    let props = map.get("outOfOfficeProperties")?.clone();
    Some(object(json!({ "outOfOfficeProperties": props })))
}

/// What Turn off did for one account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ended {
    pub replies: bool,
    pub events: usize,
}

/// Ends the account's out of office as far as `reach` goes: Microsoft
/// 365's replies, read first so a window set elsewhere since is seen, and
/// its out-of-office events through the op queue. Returns what it ended and
/// the calendars whose events changed.
pub async fn end(jolta: &Jolta, account: AccountId, reach: Reach, horizon: &Horizon, here: Tz) -> Result<(Ended, Vec<CalendarId>)> {
    let a = jolta.store.read_async(move |c| store::account(c, account)).await?;
    let replies = match super::read_editable(jolta, &a).await? {
        Some(away) => {
            let fetched = Utc::now().timestamp_millis();
            let reply = away.reply;
            jolta.store.write_async(move |tx| store::save_away(tx, &away, fetched)).await?;
            Some(reply)
        }
        None => None,
    };
    let now = Utc::now();
    let events = {
        let a = a.clone();
        jolta.store.read_async(move |c| away_events(c, &a, now, here)).await?
    };
    let plan = ending(replies, &events, reach, now, here);
    if plan.replies_off {
        let away: Away = super::write_replies(jolta, &a, &disabled_body()).await?;
        let fetched = Utc::now().timestamp_millis();
        jolta.store.write_async(move |tx| store::save_away(tx, &away, fetched)).await?;
    }
    let calendars: Vec<CalendarId> = plan.changes.iter().map(|(c, _)| *c).collect();
    let ended = Ended { replies: plan.replies_off, events: plan.changes.len() };
    let horizon = horizon.clone();
    jolta
        .store
        .write_async(move |tx| {
            for (calendar, change) in plan.changes {
                ops::queue(tx, &Edit::quiet(calendar, change), now.timestamp(), &horizon)?;
            }
            Ok(())
        })
        .await?;
    Ok((ended, calendars))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::{Attendee, Person, Response, Role};
    use crate::{Security, SmtpAuth};
    use crate::calendar::Access;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        Utc.from_utc_datetime(&chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
    }

    fn account(id: i64, kind: AccountKind, host: &str, calendar: bool) -> Account {
        Account {
            id: AccountId(id),
            kind,
            address: format!("me{id}@example.com"),
            display_name: String::new(),
            username: String::new(),
            imap_host: host.into(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 587,
            secret_ref: String::new(),
            imap_security: Security::Tls,
            smtp_security: Security::Tls,
            smtp_auth: SmtpAuth::SameAsIncoming,
            smtp_username: String::new(),
            verify_certs: true,
            signature: String::new(),
            calendar,
        }
    }

    fn calendar(id: i64, account: &Account, source: CalendarSource) -> Calendar {
        Calendar {
            id: CalendarId(id),
            account_id: Some(account.id),
            source,
            remote_id: account.address.clone(),
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

    fn target(id: i64, kind: AccountKind, source: Option<CalendarSource>, calendar_on: bool) -> Target {
        let host = if source == Some(CalendarSource::CalDav) { "imap.mail.me.com" } else { "" };
        let account = account(id, kind, host, calendar_on);
        let calendar = source.map(|s| self::calendar(id * 10, &account, s));
        Target { account, calendar }
    }

    fn request(absence: Absence) -> Request {
        Request { accounts: vec![], absence, message: "Back on <Monday>.\nWrite to the office.".into(), audience: Audience::All, declines: AutoDecline::New }
    }

    const WEEK: Absence = Absence::Days { first: NaiveDate::from_ymd_opt(2026, 11, 11).unwrap(), last: NaiveDate::from_ymd_opt(2026, 11, 13).unwrap() };

    #[test]
    fn each_account_kind_gets_what_its_provider_can_hold() {
        let targets = [
            target(1, AccountKind::Graph, Some(CalendarSource::Graph), true),
            target(2, AccountKind::Gmail, Some(CalendarSource::Google), true),
            target(3, AccountKind::Imap, Some(CalendarSource::CalDav), true),
            target(4, AccountKind::Graph, None, false),
            target(5, AccountKind::Gmail, None, true),
        ];
        let steps = plan(&targets, &request(WEEK), Brussels);

        let graph = &steps[0];
        let replies = graph.replies.as_ref().expect("Microsoft 365 gets automatic replies");
        assert_eq!((replies.from, replies.until), (utc("2026-11-10 23:00"), utc("2026-11-13 23:00")), "midnight to midnight in Brussels");
        assert_eq!(replies.message, "Back on &lt;Monday&gt;.<br>Write to the office.");
        let event = graph.event.as_ref().expect("and an event, since replies block nothing");
        assert_eq!(event.event.span, Span::AllDay { start: day("2026-11-11"), end: day("2026-11-14") });
        assert_eq!((event.event.show_as, event.event.title.as_str()), (ShowAs::OutOfOffice, TITLE));
        let Extra::Json(extra) = &event.extra else { panic!("JSON for Graph") };
        assert_eq!(extra["singleValueExtendedProperties"][0], json!({ "id": GRAPH_MARK, "value": "away" }));
        assert!(graph.line.contains("doesn't let apps decline"), "{}", graph.line);
        assert!(graph.ticked);

        let google = &steps[1];
        assert!(google.replies.is_none(), "Gmail's auto-reply needs a scope Jolta doesn't have");
        let event = google.event.as_ref().unwrap();
        let Span::Timed { start, end } = event.event.span else { panic!("Google refuses all-day out of office") };
        assert_eq!((start.instant(Brussels), end.instant(Brussels)), (utc("2026-11-10 23:00"), utc("2026-11-13 23:00")));
        let Extra::Json(extra) = &event.extra else { panic!("JSON for Google") };
        assert_eq!(extra["eventType"], "outOfOffice");
        assert_eq!(extra["outOfOfficeProperties"]["autoDeclineMode"], "declineOnlyNewConflictingInvitations", "sent, since Google's API defaults to declining nothing");
        assert_eq!(extra["outOfOfficeProperties"]["declineMessage"], "Back on <Monday>.\nWrite to the office.");
        assert_eq!(extra["extendedProperties"]["private"]["joltaHold"], "away");
        assert!(google.link.as_ref().unwrap().1.contains("authuser=me2@example.com"));

        let icloud = &steps[2];
        assert!(icloud.replies.is_none() && !icloud.ticked, "a busy event alone isn't ticked at first");
        let event = icloud.event.as_ref().unwrap();
        assert_eq!(event.extra, Extra::Lines(vec!["X-JOLTA-HOLD:away".into()]));
        assert_eq!(event.event.show_as, ShowAs::OutOfOffice, "written as X-MICROSOFT-CDO-BUSYSTATUS:OOF");
        assert_eq!(icloud.line, "A busy all-day event only. iCloud's auto-reply is set on icloud.com.");

        assert_eq!((steps[3].blocked, steps[3].event.is_none()), (Some(Blocked::CalendarOff), true));
        assert_eq!(steps[4].blocked, Some(Blocked::NoCalendar));
    }

    #[test]
    fn google_declines_what_the_sheet_says() {
        let target = [target(2, AccountKind::Gmail, Some(CalendarSource::Google), true)];
        for (declines, mode) in [(AutoDecline::None, "declineNone"), (AutoDecline::All, "declineAllConflictingInvitations")] {
            let step = &plan(&target, &Request { declines, ..request(WEEK) }, Brussels)[0];
            let Extra::Json(extra) = &step.event.as_ref().unwrap().extra else { panic!() };
            assert_eq!(extra["outOfOfficeProperties"]["autoDeclineMode"], mode);
            assert_eq!(step.event.as_ref().unwrap().event.kind, EventKind::OutOfOffice { declines, message: request(WEEK).message });
        }
    }

    #[test]
    fn whole_days_on_google_cross_the_october_clock_change() {
        // Brussels leaves summer time on Sunday 25 October 2026 at 03:00.
        let absence = Absence::Days { first: day("2026-10-23"), last: day("2026-10-26") };
        let target = [target(2, AccountKind::Gmail, Some(CalendarSource::Google), true)];
        let event = plan(&target, &request(absence), Brussels)[0].event.clone().unwrap().event;
        let Span::Timed { start, end } = event.span else { panic!("timed") };
        assert_eq!(start.wall, day("2026-10-23").and_time(NaiveTime::MIN));
        assert_eq!(end.wall, day("2026-10-27").and_time(NaiveTime::MIN), "midnight after the last day on the user's clock");
        assert_eq!(start.instant(Brussels), utc("2026-10-22 22:00"), "summer time");
        assert_eq!(end.instant(Brussels), utc("2026-10-26 23:00"), "winter time");
        assert_eq!((end.instant(Brussels) - start.instant(Brussels)).num_hours(), 4 * 24 + 1);
    }

    #[test]
    fn the_graph_body_for_each_audience() {
        let r = |audience| Replies { from: utc("2026-11-10 23:00"), until: utc("2026-11-13 23:00"), message: "Away".into(), audience };
        assert_eq!(
            replies_body(&r(Audience::All)),
            json!({ "automaticRepliesSetting": {
                "status": "scheduled",
                "scheduledStartDateTime": { "dateTime": "2026-11-10T23:00:00", "timeZone": "UTC" },
                "scheduledEndDateTime": { "dateTime": "2026-11-13T23:00:00", "timeZone": "UTC" },
                "internalReplyMessage": "Away",
                "externalReplyMessage": "Away",
                "externalAudience": "all",
            } })
        );
        assert_eq!(replies_body(&r(Audience::Contacts))["automaticRepliesSetting"]["externalAudience"], "contactsOnly");
        assert_eq!(replies_body(&r(Audience::Organization))["automaticRepliesSetting"]["externalAudience"], "none");
        assert_eq!(disabled_body(), json!({ "automaticRepliesSetting": { "status": "disabled" } }));
    }

    #[test]
    fn outlook_html_reads_back_as_text() {
        assert_eq!(text("<html><body><div>Back on Monday.</div><div><br></div><div>Write to &lt;the office&gt;.</div></body></html>"), "Back on Monday.\n\nWrite to <the office>.");
        assert_eq!(text(&html("Away & back\nMonday")), "Away & back\nMonday");
    }

    fn found(uid: &str, span: Span) -> Found {
        let mut event = EventData::new(uid, span);
        event.show_as = ShowAs::OutOfOffice;
        event.title = TITLE.into();
        Found { calendar: CalendarId(7), event }
    }

    fn timed(from: &str, until: &str) -> Span {
        let at = |s: &str| Local { wall: utc(s).with_timezone(&Brussels).naive_local(), zone: Zone::Tz(Brussels) };
        Span::Timed { start: at(from), end: at(until) }
    }

    #[test]
    fn turning_off_before_the_start_deletes_and_after_it_shortens() {
        let week = found("week", Span::AllDay { start: day("2026-11-11"), end: day("2026-11-14") });
        let google = found("google", timed("2026-11-10 23:00", "2026-11-13 23:00"));
        let replies = Some(AutoReply::On { from: Some(utc("2026-11-10 23:00")), until: Some(utc("2026-11-13 23:00")) });
        let events = [week.clone(), google.clone()];

        let before = ending(replies, &events, Reach::NowOrNext, utc("2026-11-09 09:00"), Brussels);
        assert!(before.replies_off);
        assert_eq!(before.changes, vec![(CalendarId(7), Change::Delete { uid: "week".into() }), (CalendarId(7), Change::Delete { uid: "google".into() })]);
        assert!(ending(replies, &events, Reach::Now, utc("2026-11-09 09:00"), Brussels).is_empty(), "the strip's Turn off ends only what is on now");

        let during = ending(replies, &events, Reach::Now, utc("2026-11-12 13:30") + Duration::seconds(42), Brussels);
        assert!(during.replies_off);
        let [(_, Change::Update(week_now)), (_, Change::Update(google_now))] = during.changes.as_slice() else { panic!("both shortened: {during:?}") };
        assert_eq!(week_now.span, timed("2026-11-10 23:00", "2026-11-12 13:30"), "an all-day event becomes timed and ends at this minute");
        assert_eq!(google_now.span, timed("2026-11-10 23:00", "2026-11-12 13:30"));
        assert_eq!((week_now.title.as_str(), week_now.show_as), (TITLE, ShowAs::OutOfOffice), "only the end changes");

        let started = found("now", timed("2026-11-12 13:30", "2026-11-12 18:00"));
        let just = ending(None, &[started], Reach::Now, utc("2026-11-12 13:30") + Duration::seconds(20), Brussels);
        assert_eq!(just.changes, vec![(CalendarId(7), Change::Delete { uid: "now".into() })], "nothing left to keep");
    }

    #[test]
    fn turning_off_reaches_only_the_next_out_of_office() {
        let soon = found("soon", Span::AllDay { start: day("2026-11-11"), end: day("2026-11-14") });
        let christmas = found("christmas", Span::AllDay { start: day("2026-12-21"), end: day("2027-01-04") });
        let replies = Some(AutoReply::On { from: Some(utc("2026-12-20 23:00")), until: Some(utc("2027-01-03 23:00")) });
        let next = ending(replies, &[soon.clone(), christmas.clone()], Reach::NowOrNext, utc("2026-11-09 09:00"), Brussels);
        assert!(!next.replies_off, "Christmas' replies stay");
        assert_eq!(next.changes, vec![(CalendarId(7), Change::Delete { uid: "soon".into() })]);

        let ended = Some(AutoReply::On { from: Some(utc("2026-11-01 00:00")), until: Some(utc("2026-11-02 00:00")) });
        assert!(ending(ended, &[], Reach::NowOrNext, utc("2026-11-09 09:00"), Brussels).is_empty(), "replies already over stay as they are");
        assert!(ending(Some(AutoReply::Off), &[], Reach::NowOrNext, utc("2026-11-09 09:00"), Brussels).is_empty());
        let open = ending(Some(AutoReply::On { from: None, until: None }), &[], Reach::Now, utc("2026-11-09 09:00"), Brussels);
        assert!(open.replies_off, "replies on with no end turn off");
    }

    #[test]
    fn a_request_that_ends_before_it_starts_or_is_over() {
        let now = utc("2026-11-12 09:00");
        assert_eq!(request(WEEK).problem(now, Brussels), None, "on now still counts");
        assert_eq!(request(Absence::Days { first: day("2026-11-13"), last: day("2026-11-11") }).problem(now, Brussels), Some("The end is before the start."));
        assert_eq!(request(Absence::Days { first: day("2026-11-11"), last: day("2026-11-11") }).problem(now, Brussels), Some("That time is over already."));
        assert_eq!(request(Absence::Times { from: now, until: now }).problem(now, Brussels), Some("The end is before the start."));
    }

    #[test]
    fn only_an_out_of_office_event_is_moved_to_new_days() {
        let target = [target(2, AccountKind::Gmail, Some(CalendarSource::Google), true)];
        let google = plan(&target, &request(WEEK), Brussels)[0].event.clone().unwrap().event;
        let plain = EventData { show_as: ShowAs::Busy, title: TITLE.into(), ..EventData::new("busy", google.span) };
        assert!(!reusable(&plain, &google), "a busy event titled Out of office would never decline");
        let status = EventData { show_as: ShowAs::OutOfOffice, kind: EventKind::OutOfOffice { declines: AutoDecline::None, message: String::new() }, ..plain.clone() };
        assert!(reusable(&status, &google));
        let graph = EventData { show_as: ShowAs::OutOfOffice, ..plain.clone() };
        assert!(reusable(&graph, &EventData { kind: EventKind::Default, ..google }), "Microsoft 365 and CalDAV say it by show as");
    }

    #[test]
    fn the_current_window_for_the_sheet() {
        let soon = found("soon", Span::AllDay { start: day("2026-11-11"), end: day("2026-11-14") });
        assert_eq!(current(None, &[soon], utc("2026-11-09 09:00"), Brussels), Some((utc("2026-11-10 23:00"), utc("2026-11-13 23:00"))));
        assert_eq!(current(Some(AutoReply::Off), &[], utc("2026-11-09 09:00"), Brussels), None);
    }

    #[test]
    fn meetings_and_series_are_never_out_of_office_events() {
        let dir = std::env::temp_dir().join(format!("jolta-away-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let j = crate::Jolta::open(&dir).unwrap();
        let now = utc("2026-11-09 09:00");
        let horizon = Horizon::around(now, Brussels);
        let uids = j
            .store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO account (id, kind, address, username, secret_ref, created, calendar)
                     VALUES (1, 'graph', 'me1@example.com', 'me', 'a1', 0, 1);",
                )?;
                let own = store::RemoteCalendar {
                    remote_id: "AAMk".into(),
                    name: "Calendar".into(),
                    color: None,
                    zone: None,
                    access: crate::calendar::Access::Owner,
                    primary: true,
                    refresh: None,
                    video: None,
                };
                let c = store::sync_calendar_list(tx, AccountId(1), CalendarSource::Graph, &[own])?[0].id;
                let alone = found("alone", Span::AllDay { start: day("2026-11-11"), end: day("2026-11-14") }).event;
                let mut meeting = EventData { uid: "meeting".into(), ..alone.clone() };
                meeting.attendees.push(Attendee {
                    person: Person { address: "other@example.com".into(), name: String::new() },
                    role: Role::Required,
                    response: Response::NeedsAction,
                    comment: String::new(),
                    is_me: false,
                });
                let series = EventData { uid: "series".into(), recurrence: Some(Default::default()), ..alone.clone() };
                let named = EventData { uid: "named".into(), show_as: ShowAs::Busy, ..alone.clone() };
                let busy = EventData { uid: "busy".into(), show_as: ShowAs::Busy, title: "Dentist".into(), ..alone.clone() };
                let past = EventData { uid: "past".into(), span: Span::AllDay { start: day("2026-11-01"), end: day("2026-11-02") }, ..alone.clone() };
                for e in [alone, meeting, series, named, busy, past] {
                    store::apply_item(tx, c, &e.uid.clone(), None, &[e], None, &horizon)?;
                }
                let a = store::account(tx, AccountId(1))?;
                Ok(away_events(tx, &a, now, Brussels)?.into_iter().map(|f| f.event.uid).collect::<Vec<_>>())
            })
            .unwrap();
        let mut uids = uids;
        uids.sort();
        assert_eq!(uids, ["alone", "named"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
