//! `jolta cal create`, `edit`, `delete`, `ops`, `undo`, `apply-mine`,
//! `caps` and `new-local`: calendar edits through the op queue, as the app
//! makes them.

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use clap::Args;
use jolta_core::Jolta;
use jolta_core::calendar::ops::{self, CalendarOp, CalendarOpId, Change, Edit, Extra, OpState, Scope, Undone};
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{Bounds, CalendarId, EventData, EventId, Local, Recurrence, Span, Zone, caps, draft, tz};
use jolta_core::store;

use crate::truncate;

#[derive(Args)]
pub struct When {
    /// "2026-11-24 14:00" on this Mac's clock, or a day with --all-day.
    #[arg(long)]
    start: Option<String>,
    /// The end time, or with --all-day the last day.
    #[arg(long, conflicts_with = "minutes")]
    end: Option<String>,
    #[arg(long)]
    minutes: Option<i64>,
    #[arg(long)]
    all_day: bool,
}

#[derive(Args)]
pub struct CreateArgs {
    #[arg(long)]
    calendar: i64,
    #[arg(long)]
    title: String,
    #[arg(long)]
    location: Option<String>,
    #[command(flatten)]
    when: When,
    /// Provider fields Jolta doesn't model, as a JSON object merged into the
    /// Google or Microsoft 365 request ('{"eventType":"outOfOffice"}').
    #[arg(long, conflicts_with = "line")]
    extra: Option<String>,
    /// An iCalendar line added to a CalDAV event, such as X-JOLTA-HOLD:1.
    #[arg(long)]
    line: Vec<String>,
    /// Repeat by this RRULE, such as "FREQ=WEEKLY;BYDAY=TU;COUNT=6".
    #[arg(long)]
    repeat: Option<String>,
    #[command(flatten)]
    send: Send,
}

/// Which events of a series, as the editor's scope prompt asks.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum ScopeArg {
    This,
    Following,
    All,
}

impl From<ScopeArg> for Scope {
    fn from(s: ScopeArg) -> Scope {
        match s {
            ScopeArg::This => Scope::This,
            ScopeArg::Following => Scope::Following,
            ScopeArg::All => Scope::All,
        }
    }
}

/// One occurrence of a series and which events of it, for edit and delete.
#[derive(Args)]
pub struct Occurrence {
    /// The day of the occurrence to change, "2026-11-24": the editor opens
    /// that occurrence as the app does from the grid.
    #[arg(long)]
    occurrence: Option<String>,
    /// For an occurrence: this event, this and following events, or all.
    #[arg(long, value_enum, requires = "occurrence")]
    scope: Option<ScopeArg>,
}

#[derive(Args)]
pub struct Send {
    /// Keep the op waiting this many seconds before the worker may send it.
    #[arg(long)]
    hold: Option<i64>,
    /// Wait until the op is settled, through holds and retries.
    #[arg(long)]
    wait: bool,
}

#[derive(Args)]
pub struct EditArgs {
    /// The event's id, as `jolta cal list` shows it after the title.
    event: i64,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    location: Option<String>,
    #[command(flatten)]
    when: When,
    /// Repeat by this RRULE, or "none" for no repeat.
    #[arg(long)]
    repeat: Option<String>,
    #[command(flatten)]
    occurrence: Occurrence,
    #[command(flatten)]
    send: Send,
}

#[derive(Args)]
pub struct DeleteArgs {
    event: i64,
    #[command(flatten)]
    occurrence: Occurrence,
    #[command(flatten)]
    send: Send,
}

fn horizon() -> Horizon {
    Horizon::around(Utc::now(), tz::here())
}

/// The minutes a new event lasts without --end or --minutes: Outlook's default.
const DEFAULT_MINUTES: i64 = 30;

fn time(text: &str) -> Result<Local> {
    let wall = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M")
        .with_context(|| format!("{text:?} is not a time like \"2026-11-24 14:00\""))?;
    Ok(Local { wall, zone: Zone::Tz(tz::here()) })
}

fn day(text: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").with_context(|| format!("{text:?} is not a day like 2026-11-24"))
}

impl When {
    /// The span these options give, starting from `old` for what they leave out.
    fn span(&self, old: Option<&Span>) -> Result<Option<Span>> {
        let all_day = self.all_day || matches!(old, Some(Span::AllDay { .. })) && self.start.as_deref().is_none_or(|s| !s.contains(':'));
        if self.start.is_none() && self.end.is_none() && self.minutes.is_none() && !self.all_day {
            return Ok(None);
        }
        if all_day {
            let (old_start, old_days) = match old {
                Some(Span::AllDay { start, end }) => (Some(*start), (*end - *start).num_days()),
                _ => (None, 1),
            };
            let start = match &self.start {
                Some(s) => day(s)?,
                None => old_start.context("--start is needed")?,
            };
            let end = match &self.end {
                Some(e) => day(e)? + Duration::days(1),
                None => start + Duration::days(old_days.max(1)),
            };
            if end <= start {
                bail!("the last day is before the first");
            }
            return Ok(Some(Span::AllDay { start, end }));
        }
        let (old_start, old_minutes) = match old {
            Some(Span::Timed { start, end }) => (Some(*start), (end.wall - start.wall).num_minutes()),
            _ => (None, DEFAULT_MINUTES),
        };
        let start = match &self.start {
            Some(s) => time(s)?,
            None => old_start.context("--start is needed")?,
        };
        let end = match (&self.end, self.minutes) {
            (Some(e), _) => time(e)?,
            (None, Some(m)) => Local { wall: start.wall + Duration::minutes(m), ..start },
            (None, None) => Local { wall: start.wall + Duration::minutes(old_minutes), ..start },
        };
        if end.wall <= start.wall {
            bail!("the end is not after the start");
        }
        Ok(Some(Span::Timed { start, end }))
    }
}

/// Queues an edit as the app does, holds it when asked, then sends what is
/// due, as the app's worker would, or with `--wait` everything up to this
/// op's end.
async fn submit(jolta: &Jolta, edit: Edit, send: &Send) -> Result<()> {
    let now = Utc::now().timestamp();
    let h = horizon();
    let id = jolta.store.write(|tx| {
        let id = ops::queue(tx, &edit, now, &h)?;
        if let (Some(id), Some(hold)) = (id, send.hold) {
            store::hold_calendar_op(tx, id, now + hold)?;
        }
        Ok(id)
    })?;
    match id {
        None => println!("Nothing changed; no op queued."),
        Some(id) => settle_op(jolta, id, send.wait).await?,
    }
    Ok(())
}

/// Sends what is due and prints the op; with `wait`, until it is settled.
async fn settle_op(jolta: &Jolta, id: CalendarOpId, wait: bool) -> Result<()> {
    loop {
        send_due(jolta).await?;
        let op = jolta.store.read(|c| store::calendar_op(c, id))?;
        if !wait || !matches!(op.state, OpState::Pending | OpState::Sending) {
            print_op(&op);
            return Ok(());
        }
        let left = (op.send_after - Utc::now().timestamp()).clamp(1, 60);
        tokio::time::sleep(std::time::Duration::from_secs(left as u64)).await;
    }
}

async fn send_due(jolta: &Jolta) -> Result<()> {
    for s in ops::run_due(jolta, horizon()).await? {
        if let Some(f) = s.failure {
            println!("{}", f.message);
        }
    }
    Ok(())
}

fn stored_event(jolta: &Jolta, id: i64) -> Result<(CalendarId, String, EventData)> {
    let (calendar, rid, data): (CalendarId, String, String) = jolta
        .store
        .read(|c| Ok(c.query_row("SELECT calendar_id, recurrence_id, data FROM event WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?))
        .with_context(|| format!("no event #{id}; `jolta cal list` shows the ids"))?;
    Ok((calendar, rid, serde_json::from_str(&data)?))
}

pub async fn create(jolta: &Jolta, a: CreateArgs) -> Result<()> {
    let span = a.when.span(None)?.context("--start is needed")?;
    let recurrence = a.repeat.map(|r| Recurrence { rules: vec![r.trim_start_matches("RRULE:").to_string()], ..Recurrence::default() });
    let event = EventData { title: a.title, location: a.location.unwrap_or_default(), recurrence, ..EventData::new(ops::new_uid()?, span) };
    let extra = match (a.extra, a.line) {
        (Some(json), _) => match serde_json::from_str(&json).context("--extra is not JSON")? {
            serde_json::Value::Object(fields) => Some(Extra::Json(fields)),
            _ => bail!("--extra must be a JSON object"),
        },
        (None, lines) if !lines.is_empty() => Some(Extra::Lines(lines)),
        (None, _) => None,
    };
    let edit = Edit { extra, ..Edit::quiet(CalendarId(a.calendar), Change::Create(vec![event])) };
    submit(jolta, edit, &a.send).await
}

/// The bounds of the occurrence of event `id`'s series that starts on `day`.
fn occurrence_on(jolta: &Jolta, id: i64, day: &str) -> Result<Bounds> {
    let (calendar, _, event) = stored_event(jolta, id)?;
    let day = self::day(day)?;
    let rows = jolta.store.read(|c| store::occurrences(c, day, day + Duration::days(1), tz::here()))?;
    let found = rows.into_iter().find(|o| o.calendar_id == calendar && o.uid == event.uid && match o.bounds {
        Bounds::Utc { start, .. } => start.with_timezone(&tz::here()).date_naive() == day,
        Bounds::Days { start, .. } => NaiveDate::default() + Duration::days(start.into()) == day,
    });
    found.map(|o| o.bounds).with_context(|| format!("#{id} has no occurrence on {day}"))
}

/// The editor's draft of the event, at the occurrence asked for.
fn drafted(jolta: &Jolta, id: i64, occurrence: Option<&str>) -> Result<draft::EventDraft> {
    let at = occurrence.map(|d| occurrence_on(jolta, id, d)).transpose()?;
    Ok(jolta.store.read(|c| draft::draft_for_event(c, EventId(id), at, tz::here()))?)
}

/// Queues the editor's save or delete and settles it as `submit` does.
async fn finish(jolta: &Jolta, saved: draft::Saved, send: &Send) -> Result<()> {
    println!("{}", saved.message);
    match saved.op {
        None => println!("Nothing changed; no op queued."),
        Some(id) => {
            if let Some(hold) = send.hold {
                jolta.store.write(|tx| store::hold_calendar_op(tx, id, Utc::now().timestamp() + hold))?;
            }
            settle_op(jolta, id, send.wait).await?;
            // This and following waits for the new series' create first.
            let op = jolta.store.read(|c| store::calendar_op(c, id))?;
            if let Some(first) = op.after {
                print_op(&jolta.store.read(|c| store::calendar_op(c, first))?);
            }
        }
    }
    Ok(())
}

fn print_scopes(plan: &[draft::ScopeChoice]) {
    for (i, c) in plan.iter().enumerate() {
        println!("  {} {:<26} {}", i + 1, c.label, c.detail.as_deref().unwrap_or(""));
    }
}

pub async fn edit(jolta: &Jolta, a: EditArgs) -> Result<()> {
    if a.occurrence.occurrence.is_some() || a.repeat.is_some() {
        let here = tz::here();
        let mut d = drafted(jolta, a.event, a.occurrence.occurrence.as_deref())?;
        if let Some(title) = a.title {
            d.title = title;
        }
        if let Some(location) = a.location {
            d.location = location;
        }
        if let Some(span) = a.when.span(Some(&d.span))? {
            d.span = span;
        }
        if let Some(rule) = a.repeat {
            d.rule = match rule.trim() {
                "none" => None,
                r => Some(Recurrence { rules: vec![r.trim_start_matches("RRULE:").to_string()], ..d.rule_was.clone().unwrap_or_default() }),
            };
        }
        let problems = jolta.store.read(|c| draft::check_draft(c, &d, here))?;
        print_draft(&d, &problems);
        let plan = jolta.store.read(|c| draft::plan_save(c, &d, here))?;
        if let Some(prompt) = &plan.prompt {
            println!("{prompt}");
            print_scopes(&plan.scopes);
        }
        let h = horizon();
        let saved = jolta.store.write(|tx| draft::save_draft(tx, &d, a.occurrence.scope.map(Into::into), Utc::now().timestamp(), &h, here))?;
        return finish(jolta, saved, &a.send).await;
    }
    let (calendar, _, mut event) = stored_event(jolta, a.event)?;
    if let Some(title) = a.title {
        event.title = title;
    }
    if let Some(location) = a.location {
        event.location = location;
    }
    if let Some(span) = a.when.span(Some(&event.span))? {
        event.span = span;
    }
    submit(jolta, Edit::quiet(calendar, Change::Update(event)), &a.send).await
}

pub async fn delete(jolta: &Jolta, a: DeleteArgs) -> Result<()> {
    if let Some(day) = a.occurrence.occurrence.as_deref() {
        let here = tz::here();
        let d = drafted(jolta, a.event, Some(day))?;
        let at = occurrence_on(jolta, a.event, day)?;
        let plan = jolta.store.read(|c| draft::plan_delete(c, EventId(a.event), Some(at), here))?;
        print_scopes(&plan.scopes);
        let h = horizon();
        let saved = jolta.store.write(|tx| draft::delete_drafted(tx, &d, a.occurrence.scope.map(Into::into), Utc::now().timestamp(), &h, here))?;
        return finish(jolta, saved, &a.send).await;
    }
    let (calendar, rid, event) = stored_event(jolta, a.event)?;
    if !rid.is_empty() {
        bail!("#{} is one changed occurrence; delete it with --occurrence and --scope", a.event);
    }
    submit(jolta, Edit::quiet(calendar, Change::Delete { uid: event.uid }), &a.send).await
}

pub async fn apply_mine(jolta: &Jolta, op: i64, wait: bool) -> Result<()> {
    let h = horizon();
    match jolta.store.write(|tx| ops::apply_mine(tx, CalendarOpId(op), Utc::now().timestamp(), &h))? {
        None => println!("The event already is as you had it; no op queued."),
        Some(id) => settle_op(jolta, id, wait).await?,
    }
    Ok(())
}

pub async fn list_ops(jolta: &Jolta, send: bool) -> Result<()> {
    if send {
        send_due(jolta).await?;
    }
    let ops = jolta.store.read(|c| store::calendar_ops(c, 50))?;
    if ops.is_empty() {
        println!("No calendar ops.");
    }
    for op in ops.iter().rev() {
        print_op(op);
    }
    Ok(())
}

fn print_op(op: &CalendarOp) {
    let title = op.write.event().map_or("(no event)", |e| e.title.as_str());
    let now = Utc::now().timestamp();
    let wait = match op.state {
        OpState::Pending if op.send_after > now => format!(", due in {} s", op.send_after - now),
        _ => String::new(),
    };
    let error = op.error.as_deref().map(|e| format!(", last error: {e}")).unwrap_or_default();
    let lost = if op.lost.is_empty() { String::new() } else { format!(", the server kept: {}", op.lost.iter().map(|f| f.noun()).collect::<Vec<_>>().join(", ")) };
    println!(
        "op {:>4}  {:<8} {:<6} {:<30} calendar {}  tried: {}, attempts: {}{wait}{error}{lost}",
        op.id.0,
        op.state,
        op.write.kind(),
        truncate(title, 30),
        op.calendar,
        if op.tried { "yes" } else { "no" },
        op.attempts
    );
}

pub async fn undo(jolta: &Jolta, op: i64) -> Result<()> {
    let h = horizon();
    let undone = jolta.store.write(|tx| ops::undo(tx, CalendarOpId(op), Utc::now().timestamp(), &h))?;
    match undone {
        Undone::Dropped => println!("Op {op} was never sent: dropped, and the event is back as it was."),
        Undone::Queued(inverse) => {
            send_due(jolta).await?;
            println!("Op {op} may have reached the server; its opposite is queued:");
            print_op(&jolta.store.read(|c| store::calendar_op(c, inverse))?);
        }
    }
    Ok(())
}

pub fn print_caps(jolta: &Jolta, calendar: i64) -> Result<()> {
    let c = jolta.store.read(|c| store::calendar(c, CalendarId(calendar)))?;
    let k = caps::of(&c);
    println!("{} ({}, {})", c.name, c.source, c.access);
    println!("  writable {}, invite {}, sends itself {}", k.writable, k.invite, k.sends_itself);
    println!("  Don't send: {:?}, outside only {}, new people only {}, cancel message {}", k.hold_update, k.outside_only, k.new_people_only, k.cancel_message);
    println!("  repeat: hourly {}, BYSETPOS {}, several BYMONTH {}; this and following {}", k.repeat.hourly, k.repeat.set_pos, k.repeat.several_months, k.split_series);
    println!("  move {:?}, video {:?}, remove video {}", k.move_within, k.video, k.remove_video);
    println!("  options {:?}", k.options);
    println!("  show as {:?}, max people {:?}", k.show_as, k.max_people);
    Ok(())
}

#[derive(Args)]
pub struct DraftArgs {
    /// The event's id, as `jolta cal list` shows it after the title; none for a new event.
    event: Option<i64>,
    /// The calendar of a new event; the default calendar without it.
    #[arg(long)]
    calendar: Option<i64>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    room: Option<String>,
    #[arg(long)]
    location: Option<String>,
    #[command(flatten)]
    when: When,
    /// No reminder, as the editor's Reminder menu "None"; a new event
    /// otherwise takes its calendar's default.
    #[arg(long)]
    no_reminder: bool,
    /// Save the draft as the editor's Save does.
    #[arg(long, conflicts_with = "delete")]
    save: bool,
    /// Delete the event as the editor's Delete does.
    #[arg(long, requires = "event")]
    delete: bool,
    #[command(flatten)]
    send: Send,
}

pub async fn draft(jolta: &Jolta, a: DraftArgs) -> Result<()> {
    let here = tz::here();
    let mut d = jolta.store.read(|c| match a.event {
        Some(id) => draft::draft_for_event(c, EventId(id), None, here),
        None => draft::draft_for_new(c, a.calendar.map(CalendarId), None, Utc::now(), here),
    })?;
    if let Some(title) = a.title {
        d.title = title;
    }
    if let Some(room) = a.room {
        d.room = room;
    }
    if let Some(location) = a.location {
        d.location = location;
    }
    if a.no_reminder {
        d.reminder = draft::Reminder::Off;
    }
    if let Some(span) = a.when.span(Some(&d.span))? {
        d.span = span;
    }
    let problems = jolta.store.read(|c| draft::check_draft(c, &d, here))?;
    print_draft(&d, &problems);
    let h = horizon();
    let now = Utc::now().timestamp();
    let saved = match (a.save, a.delete, a.event) {
        (true, _, _) => jolta.store.write(|tx| draft::save_draft(tx, &d, None, now, &h, here))?,
        (_, true, Some(id)) => jolta.store.write(|tx| draft::delete_event(tx, EventId(id), now, &h, here))?,
        _ => return Ok(()),
    };
    println!("{}", saved.message);
    if let Some(id) = saved.op {
        if let Some(hold) = a.send.hold {
            jolta.store.write(|tx| store::hold_calendar_op(tx, id, now + hold))?;
        }
        settle_op(jolta, id, a.send.wait).await?;
    }
    if !a.delete
        && let Some(stored) = jolta.store.read(|c| ops::stored(c, d.calendar, &d.uid, None))?
    {
        println!("event #{}", stored.id.0);
    }
    Ok(())
}

fn print_draft(d: &draft::EventDraft, problems: &[draft::Problem]) {
    let when = match d.span {
        Span::Timed { start, end } => format!("{} to {} ({:?})", start.wall.format("%a %-d %b %H:%M"), end.wall.format("%H:%M"), start.zone),
        Span::AllDay { start, end } => format!("all day {} to {}", start.format("%a %-d %b"), (end - Duration::days(1)).format("%a %-d %b")),
    };
    println!("{} in calendar {}", if d.title.is_empty() { "(no title)" } else { &d.title }, d.calendar);
    println!("  when: {when}{}", d.repeat.as_deref().map(|r| format!(", {r}")).unwrap_or_default());
    if let Some(rule) = &d.rule {
        println!("  repeat: {}", rule.rules.join(" / "));
    }
    if let Some(o) = &d.organizer {
        println!("  organizer: {}", if o.name.is_empty() { &o.address } else { &o.name });
    }
    for p in &d.people {
        println!("  person: {} ({:?}, {:?}){}", p.person.address, p.role, p.response, if p.is_me { ", you" } else { "" });
    }
    for (label, value) in [("room", &d.room), ("location", &d.location), ("description", &d.description)] {
        if !value.is_empty() {
            println!("  {label}: {}", truncate(&value.replace('\n', " / "), 80));
        }
    }
    println!("  show as {:?}, reminder {:?}, visibility {:?}", d.show_as, d.reminder, d.visibility);
    match &d.notice {
        Some(n) => println!("  locked ({} fields): {n}", d.locks.len()),
        None => println!("  nothing locked"),
    }
    for p in problems {
        println!("  {}: {}{}", p.field.map_or("event".into(), |f| format!("{f:?}")), p.text, if p.blocking { "" } else { " (warning)" });
    }
}

pub fn default_calendar(jolta: &Jolta, calendar: Option<i64>, clear: bool) -> Result<()> {
    if calendar.is_some() || clear {
        jolta.store.write(|tx| store::set_default_calendar(tx, calendar.map(CalendarId)))?;
    }
    match jolta.store.read(draft::default_calendar)? {
        Some(c) => println!("New events go to {} ({}){}.", c.name, c.id, if c.is_default { "" } else { ", the first writable calendar" }),
        None => println!("No calendar takes new events."),
    }
    Ok(())
}

pub fn new_local(jolta: &Jolta, name: &str, color: Option<&str>) -> Result<()> {
    let c = jolta.store.write(|tx| ops::new_local_calendar(tx, name, color))?;
    println!("Calendar {} ({}) is on this Mac only.", c.id, c.name);
    Ok(())
}

/// "#12" for `jolta cal list`, with the mark the app shows while an edit waits.
pub fn event_label(id: EventId, not_synced: bool) -> String {
    if not_synced { format!("#{} *", id.0) } else { format!("#{}", id.0) }
}
