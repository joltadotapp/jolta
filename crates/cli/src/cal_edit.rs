//! `jolta cal create`, `edit`, `delete`, `ops`, `undo`, `caps` and
//! `new-local`: calendar edits through the op queue, as the app makes them.

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use clap::Args;
use jolta_core::Jolta;
use jolta_core::calendar::ops::{self, CalendarOp, CalendarOpId, Change, Edit, OpState, Undone};
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{CalendarId, EventData, EventId, Local, Span, Zone, caps, tz};
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
    /// Keep the op waiting this many seconds before the worker may send it.
    #[arg(long)]
    hold: Option<i64>,
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
    #[arg(long)]
    hold: Option<i64>,
}

#[derive(Args)]
pub struct DeleteArgs {
    event: i64,
    #[arg(long)]
    hold: Option<i64>,
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
/// due, as the app's worker would.
async fn submit(jolta: &Jolta, edit: Edit, hold: Option<i64>) -> Result<()> {
    let now = Utc::now().timestamp();
    let h = horizon();
    let id = jolta.store.write(|tx| {
        let id = ops::queue(tx, &edit, now, &h)?;
        if let (Some(id), Some(hold)) = (id, hold) {
            store::hold_calendar_op(tx, id, now + hold)?;
        }
        Ok(id)
    })?;
    match id {
        None => println!("Nothing changed; no op queued."),
        Some(id) => {
            send_due(jolta).await?;
            print_op(&jolta.store.read(|c| store::calendar_op(c, id))?);
        }
    }
    Ok(())
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
    let event = EventData { title: a.title, location: a.location.unwrap_or_default(), ..EventData::new(ops::new_uid()?, span) };
    submit(jolta, Edit::quiet(CalendarId(a.calendar), Change::Create(vec![event])), a.hold).await
}

pub async fn edit(jolta: &Jolta, a: EditArgs) -> Result<()> {
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
    submit(jolta, Edit::quiet(calendar, Change::Update(event)), a.hold).await
}

pub async fn delete(jolta: &Jolta, a: DeleteArgs) -> Result<()> {
    let (calendar, rid, event) = stored_event(jolta, a.event)?;
    if !rid.is_empty() {
        bail!("#{} is one changed occurrence; deleting one occurrence isn't possible yet, delete the series", a.event);
    }
    submit(jolta, Edit::quiet(calendar, Change::Delete { uid: event.uid }), a.hold).await
}

pub fn list_ops(jolta: &Jolta) -> Result<()> {
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
    println!(
        "op {:>4}  {:<7} {:<6} {:<30} calendar {}  tried: {}, attempts: {}{wait}{error}",
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

pub fn new_local(jolta: &Jolta, name: &str, color: Option<&str>) -> Result<()> {
    let c = jolta.store.write(|tx| ops::new_local_calendar(tx, name, color))?;
    println!("Calendar {} ({}) is on this Mac only.", c.id, c.name);
    Ok(())
}

/// "#12" for `jolta cal list`, with the mark the app shows while an edit waits.
pub fn event_label(id: EventId, not_synced: bool) -> String {
    if not_synced { format!("#{} *", id.0) } else { format!("#{}", id.0) }
}
