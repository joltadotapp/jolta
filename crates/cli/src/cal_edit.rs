//! `jolta cal create`, `edit`, `delete`, `ops`, `undo`, `apply-mine`,
//! `caps` and `new-local`: calendar edits through the op queue, as the app
//! makes them.

use anyhow::{Context, Result, bail};
use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use clap::Args;
use jolta_core::Jolta;
use jolta_core::calendar::ops::{self, CalendarOp, CalendarOpId, Change, Edit, Extra, OpState, Scope, Undone};
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{Attendee, Bounds, CalendarId, EventData, EventId, Local, Person, Recurrence, Response, Role, Span, Zone, caps, change, draft, tz};
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
    /// Guests, "ann@example.com,bo@example.com:optional": a meeting the
    /// calendar's server invites them to, as the editor's Save and send.
    #[arg(long, value_delimiter = ',')]
    with: Vec<String>,
    #[command(flatten)]
    people: Guests,
    #[command(flatten)]
    send: Send,
}

/// Who hears of a change to a meeting, as the Send an update? dialog asks.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum NotifyArg {
    All,
    /// Google's "Only guests who don't use Google Calendar".
    Outside,
    /// Only the people added or removed.
    New,
    None,
}

impl From<NotifyArg> for ops::Notify {
    fn from(n: NotifyArg) -> ops::Notify {
        match n {
            NotifyArg::All => ops::Notify::All,
            NotifyArg::Outside => ops::Notify::Outside,
            NotifyArg::New => ops::Notify::Changed,
            NotifyArg::None => ops::Notify::Nobody,
        }
    }
}

/// The dialog's choice and the organizer options, for a meeting.
#[derive(Args)]
pub struct Guests {
    /// The Send an update? dialog's choice; the command prints the dialog
    /// and stops without one when the change needs it.
    #[arg(long, value_enum)]
    notify: Option<NotifyArg>,
    /// Hide the guest list.
    #[arg(long)]
    hide_guests: bool,
    /// Don't allow new time proposals.
    #[arg(long)]
    no_proposals: bool,
    /// Queue the same save a second time right after, as a double click
    /// would: the second changes nothing (check 4).
    #[arg(long)]
    repeat_op: bool,
    /// Words that go with the invitation or update, where the dialog has a
    /// message field (a calendar on this Mac).
    #[arg(long = "note")]
    note: Option<String>,
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
    /// Guests to add, "addr" or "addr:optional".
    #[arg(long, value_delimiter = ',')]
    add: Vec<String>,
    /// Guests to remove, by address.
    #[arg(long, value_delimiter = ',')]
    remove: Vec<String>,
    #[command(flatten)]
    people: Guests,
    #[command(flatten)]
    occurrence: Occurrence,
    #[command(flatten)]
    send: Send,
}

#[derive(Args)]
pub struct CopyArgs {
    /// The event's id, as `jolta cal list` shows it after the title.
    event: i64,
    /// The day of the occurrence to copy, "2026-11-24", for a series.
    #[arg(long)]
    occurrence: Option<String>,
    /// Where the copy goes; the original's time without it.
    #[command(flatten)]
    when: When,
    #[command(flatten)]
    send: Send,
}

#[derive(Args)]
pub struct DeleteArgs {
    event: i64,
    /// Words that go with the cancellation of a meeting, where its server
    /// takes them (Microsoft 365).
    #[arg(long)]
    message: Option<String>,
    #[command(flatten)]
    occurrence: Occurrence,
    #[command(flatten)]
    send: Send,
}

/// "ann@example.com" or "ann@example.com:optional" as a guest.
fn guest(spec: &str) -> Attendee {
    let (address, role) = match spec.trim().rsplit_once(':') {
        Some((a, r)) if r.eq_ignore_ascii_case("optional") => (a, Role::Optional),
        Some((a, r)) if r.eq_ignore_ascii_case("room") => (a, Role::Room),
        _ => (spec.trim(), Role::Required),
    };
    Attendee { person: Person { address: address.trim().to_string(), name: String::new() }, role, response: Response::NeedsAction, comment: String::new(), is_me: false }
}

fn print_dialog(d: &change::UpdateDialog) {
    println!("{}", d.title);
    println!("  {}", d.body);
    if let Some(note) = &d.note {
        println!("  {note}");
    }
    for c in &d.choices {
        let flag = match c.notify {
            Some(ops::Notify::All) => "--notify all",
            Some(ops::Notify::Outside) => "--notify outside",
            Some(ops::Notify::Changed) => "--notify new",
            Some(ops::Notify::Nobody) => "--notify none",
            None => "",
        };
        println!("  [{}] {flag}", c.label);
    }
}

/// Saves a draft as the editor's Save does after Send an update?, with the
/// dialog printed first; with --repeat-op the same save again.
async fn save_with_guests(jolta: &Jolta, d: &draft::EventDraft, scope: Option<Scope>, g: &Guests, send: &Send) -> Result<()> {
    let here = tz::here();
    let problems = jolta.store.read(|c| draft::check_draft(c, d, here))?;
    print_draft(d, &problems);
    let plan = jolta.store.read(|c| draft::plan_save(c, d, here))?;
    if let Some(prompt) = &plan.prompt {
        println!("{prompt}");
        print_scopes(&plan.scopes);
    }
    if let Some(dialog) = &plan.dialog {
        print_dialog(dialog);
    }
    if !plan.senders.is_empty() {
        let list: Vec<&str> = plan.senders.iter().map(|s| s.address.as_str()).collect();
        println!("Send invitations from: `jolta cal send-from {} <account>` with one of {}", d.calendar, list.join(", "));
    }
    let h = horizon();
    let notify = g.notify.map(Into::into);
    let saved = jolta.store.write(|tx| draft::save_draft(tx, d, scope, notify, g.note.clone(), Utc::now().timestamp(), &h, here))?;
    if g.repeat_op {
        let again = jolta.store.write(|tx| draft::save_draft(tx, d, scope, notify, g.note.clone(), Utc::now().timestamp(), &h, here))?;
        println!("The same save again: {}", again.op.map_or("no op queued".to_string(), |o| format!("op {} queued", o.0)));
    }
    finish(jolta, saved, send).await
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

/// Sends what is due and prints the op; with `wait`, until it is settled
/// and the invitations it queued have left the outbox.
async fn settle_op(jolta: &Jolta, id: CalendarOpId, wait: bool) -> Result<()> {
    loop {
        send_due(jolta).await?;
        let op = jolta.store.read(|c| store::calendar_op(c, id))?;
        if !wait || !matches!(op.state, OpState::Pending | OpState::Sending) {
            if wait {
                send_mail(jolta, id).await?;
            }
            print_op(jolta, &op);
            return Ok(());
        }
        let left = (op.send_after - Utc::now().timestamp()).clamp(1, 60);
        tokio::time::sleep(std::time::Duration::from_secs(left as u64)).await;
    }
}

/// Sends the invitations an op queued, through the engine as the app
/// does, and waits up to three minutes for each to leave.
async fn send_mail(jolta: &Jolta, id: CalendarOpId) -> Result<()> {
    let waiting = |j: &Jolta| -> Result<Vec<i64>> { Ok(j.store.read(|c| store::calendar_op_outbox(c, id))?.into_iter().filter(|(_, state, _)| state == "queued").map(|(row, _, _)| row).collect()) };
    if waiting(jolta)?.is_empty() {
        return Ok(());
    }
    let account: jolta_core::AccountId = jolta.store.read(|c| {
        let rows = store::calendar_op_outbox(c, id)?;
        Ok(c.query_row("SELECT account_id FROM outbox WHERE id = ?1", [rows[0].0], |r| r.get(0))?)
    })?;
    let sink: jolta_core::engine::Sink = std::sync::Arc::new(|_| {});
    let engine = jolta_core::engine::Engine::start(jolta.clone(), sink, tokio::runtime::Handle::current())?;
    let started = std::time::Instant::now();
    while !waiting(jolta)?.is_empty() {
        if started.elapsed() > std::time::Duration::from_secs(180) {
            bail!("the invitations didn't leave the outbox within three minutes");
        }
        engine.wake(account);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    // Lets the engine file the Sent copies.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
    if !a.with.is_empty() {
        let here = tz::here();
        let mut d = jolta.store.read(|c| draft::draft_for_new(c, Some(CalendarId(a.calendar)), Some(span), Utc::now(), here))?;
        d.title = a.title;
        d.location = a.location.unwrap_or_default();
        d.people = a.with.iter().map(|s| guest(s)).collect();
        d.options.hide_guests = a.people.hide_guests;
        d.options.allow_new_times = !a.people.no_proposals;
        d.reminder = draft::Reminder::Off;
        d.rule = a.repeat.map(|r| Recurrence { rules: vec![r.trim_start_matches("RRULE:").to_string()], ..Recurrence::default() });
        return save_with_guests(jolta, &d, None, &a.people, &a.send).await;
    }
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
                print_op(jolta, &jolta.store.read(|c| store::calendar_op(c, first))?);
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
    let meeting = !a.add.is_empty() || !a.remove.is_empty() || a.people.notify.is_some() || a.people.hide_guests || a.people.no_proposals;
    if a.occurrence.occurrence.is_some() || a.repeat.is_some() || meeting {
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
        d.people.retain(|p| !a.remove.iter().any(|r| r.trim().eq_ignore_ascii_case(&p.person.address)));
        d.people.extend(a.add.iter().map(|s| guest(s)));
        if a.people.hide_guests {
            d.options.hide_guests = true;
        }
        if a.people.no_proposals {
            d.options.allow_new_times = false;
        }
        if meeting {
            return save_with_guests(jolta, &d, a.occurrence.scope.map(Into::into), &a.people, &a.send).await;
        }
        let problems = jolta.store.read(|c| draft::check_draft(c, &d, here))?;
        print_draft(&d, &problems);
        let plan = jolta.store.read(|c| draft::plan_save(c, &d, here))?;
        if let Some(prompt) = &plan.prompt {
            println!("{prompt}");
            print_scopes(&plan.scopes);
        }
        let h = horizon();
        let saved = jolta.store.write(|tx| draft::save_draft(tx, &d, a.occurrence.scope.map(Into::into), None, None, Utc::now().timestamp(), &h, here))?;
        return finish(jolta, saved, &a.send).await;
    }
    let (calendar, _, mut event) = stored_event(jolta, a.event)?;
    if event.attendees.iter().any(|x| !x.is_me) {
        bail!("#{} is a meeting; change it with --notify (or --occurrence), as the editor's Send an update? does", a.event);
    }
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

/// ⌘D's and ⌥-drag's copy: a new event without the original's people,
/// repeat or meeting, saved as the grid saves it.
pub async fn copy(jolta: &Jolta, a: CopyArgs) -> Result<()> {
    let here = tz::here();
    let at = a.occurrence.as_deref().map(|d| occurrence_on(jolta, a.event, d)).transpose()?;
    let mut d = jolta.store.read(|c| draft::draft_for_copy(c, EventId(a.event), at, Utc::now(), here))?;
    if let Some(span) = a.when.span(Some(&d.span))? {
        d.span = span;
    }
    let problems = jolta.store.read(|c| draft::check_draft(c, &d, here))?;
    print_draft(&d, &problems);
    let h = horizon();
    let saved = jolta.store.write(|tx| draft::save_draft(tx, &d, None, None, None, Utc::now().timestamp(), &h, here))?;
    finish(jolta, saved, &a.send).await
}

fn print_delete_plan(plan: &draft::DeletePlan) {
    print_scopes(&plan.scopes);
    for (scope, c) in &plan.cancel {
        println!("{} ({scope})", c.title);
        println!("  {}", c.body);
        match (&c.note, c.message) {
            (_, true) => println!("  --message carries your words"),
            (Some(note), false) => println!("  {note}"),
            (None, false) => {}
        }
    }
    if let Some(d) = &plan.decline {
        println!("{} {}", d.title, d.body);
    }
}

pub async fn delete(jolta: &Jolta, a: DeleteArgs) -> Result<()> {
    let here = tz::here();
    let h = horizon();
    if let Some(day) = a.occurrence.occurrence.as_deref() {
        let d = drafted(jolta, a.event, Some(day))?;
        let at = occurrence_on(jolta, a.event, day)?;
        let plan = jolta.store.read(|c| draft::plan_delete(c, EventId(a.event), Some(at), here))?;
        print_delete_plan(&plan);
        let saved = jolta.store.write(|tx| draft::delete_drafted(tx, &d, a.occurrence.scope.map(Into::into), a.message.clone(), Utc::now().timestamp(), &h, here))?;
        return finish(jolta, saved, &a.send).await;
    }
    let (_, rid, _) = stored_event(jolta, a.event)?;
    if !rid.is_empty() {
        bail!("#{} is one changed occurrence; delete it with --occurrence and --scope", a.event);
    }
    let plan = jolta.store.read(|c| draft::plan_delete(c, EventId(a.event), None, here))?;
    print_delete_plan(&plan);
    let saved = jolta.store.write(|tx| draft::delete_event(tx, EventId(a.event), a.message.clone(), Utc::now().timestamp(), &h, here))?;
    finish(jolta, saved, &a.send).await
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
        print_op(jolta, op);
    }
    Ok(())
}

fn print_op(jolta: &Jolta, op: &CalendarOp) {
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
    for (row, state, to) in jolta.store.read(|c| store::calendar_op_outbox(c, op.id)).unwrap_or_default() {
        println!("          mail: outbox {row} to {to}, {state}");
    }
}

pub async fn undo(jolta: &Jolta, op: i64) -> Result<()> {
    let h = horizon();
    let undone = jolta.store.write(|tx| ops::undo(tx, CalendarOpId(op), Utc::now().timestamp(), &h))?;
    match undone {
        Undone::Dropped => println!("Op {op} was never sent: dropped, and the event is back as it was."),
        Undone::Queued(inverse) => {
            send_due(jolta).await?;
            println!("Op {op} may have reached the server; its opposite is queued:");
            print_op(jolta, &jolta.store.read(|c| store::calendar_op(c, inverse))?);
        }
    }
    Ok(())
}

/// The account a calendar on this Mac sends its invitations from.
pub fn send_from(jolta: &Jolta, calendar: i64, account: &str) -> Result<()> {
    let id = CalendarId(calendar);
    let (c, a) = jolta.store.read(|c| Ok((store::calendar(c, id)?, store::find_account(c, account)?)))?;
    if !jolta_core::calendar::organizer::sends_itself(&c) {
        bail!("{} mails its guests itself", c.name);
    }
    jolta.store.write(|tx| store::set_calendar_send_from(tx, id, Some(a.id)))?;
    println!("{} sends its invitations from {}.", c.name, a.address);
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
        (true, _, _) => jolta.store.write(|tx| draft::save_draft(tx, &d, None, None, None, now, &h, here))?,
        (_, true, Some(id)) => jolta.store.write(|tx| draft::delete_event(tx, EventId(id), None, now, &h, here))?,
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

#[derive(Args)]
pub struct QuickArgs {
    text: Vec<String>,
    /// Read as if it were this time on this Mac's clock, "2026-11-07 15:00".
    #[arg(long)]
    now: Option<String>,
    /// Let the AI provider read what the parser leaves, as Return in the app does.
    #[arg(long)]
    ai: bool,
    /// The language that wins a tie, "en", "nl" or "fr".
    #[arg(long, default_value = "en")]
    lang: String,
}

fn moment(text: Option<&str>) -> Result<chrono::DateTime<Utc>> {
    let here = tz::here();
    Ok(match text {
        Some(t) => tz::instant(NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M").context("--now takes \"2026-11-07 15:00\"")?, here),
        None => Utc::now(),
    })
}

pub async fn quick(jolta: &Jolta, a: QuickArgs) -> Result<()> {
    use jolta_core::ai;
    use jolta_core::calendar::quick::{self, Lang, Prefs};

    let text = a.text.join(" ");
    let here = tz::here();
    let now = moment(a.now.as_deref())?;
    let minutes = jolta.store.read(store::default_event_minutes)?;
    let prefs = Prefs::new(minutes, &Lang::of(&a.lang).into_iter().collect::<Vec<_>>());
    let started = std::time::Instant::now();
    let parsed = quick::parse(&text, now.with_timezone(&here), &prefs);
    let took = started.elapsed();
    println!("{text}");
    println!("  read as {:?} in {:.0} µs", parsed.language, took.as_secs_f64() * 1e6);
    for (range, part) in &parsed.parts {
        println!("  {part:?}: {}", &text[range.clone()]);
    }
    if !parsed.leftover.is_empty() {
        println!("  left over: {}", parsed.leftover.join(", "));
    }
    let people = jolta.store.read(|c| jolta_core::people::People::build(c, now.timestamp(), &[]))?;
    let (d, by_ai, dropped) = if a.ai {
        let helper = std::env::var_os("JOLTA_AI_HELPER")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_exe().unwrap_or_default().with_file_name("jolta-ai"));
        let engine = ai::Ai::new(&jolta.data_dir, helper);
        let started = std::time::Instant::now();
        let q = ai::quick::draft(jolta, &engine, &people, &text, now, here, &prefs, &ai::Cancel::default()).await?;
        println!("  asked the AI: {} ({:.1} s)", ai::quick::wanted(&text, &parsed), started.elapsed().as_secs_f64());
        (q.draft, q.by_ai, q.dropped)
    } else {
        (jolta.store.read(|c| quick::draft(c, &people, &parsed, now, here))?, false, Vec::new())
    };
    if by_ai {
        println!("  Read by AI");
    }
    for why in dropped {
        println!("  dropped: {why}");
    }
    println!("  {}", quick::when_text(&d.span, now.with_timezone(&here).date_naive(), here));
    for u in &d.unresolved {
        let choices: Vec<&str> = u.choices.iter().map(|p| p.address.as_str()).collect();
        println!("  unresolved: {} ({})", u.typed, if choices.is_empty() { "nobody you wrote to".into() } else { choices.join(", ") });
    }
    let problems = jolta.store.read(|c| draft::check_draft(c, &d, here))?;
    print_draft(&d, &problems);
    Ok(())
}

pub fn go(text: &str, today: Option<&str>) -> Result<()> {
    use jolta_core::calendar::quick::{self, Prefs};
    let today = match today {
        Some(t) => NaiveDate::parse_from_str(t, "%Y-%m-%d").context("--today takes 2026-11-07")?,
        None => Utc::now().with_timezone(&tz::here()).date_naive(),
    };
    match quick::go_to_date(text, today, &Prefs::new(30, &[])) {
        Some(d) => println!("{}", d.format("%a %-d %b %Y")),
        None => bail!("{text:?} isn't a day"),
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
    if !d.options.is_default() {
        println!("  options: {:?}", d.options);
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
