//! `jolta cal`: calendars from the command line.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use clap::{Subcommand, ValueEnum};
use jolta_core::Jolta;
use jolta_core::oauth::{self, Provider};
use jolta_core::calendar::recur::Source as Origin;
use jolta_core::calendar::series::{self, Horizon};
use jolta_core::calendar::card::{self, Card, Edit};
use jolta_core::calendar::{invitation, remove, respond};
use jolta_core::calendar::rsvp::{self, Answer, Rsvp, Scope};
use jolta_core::calendar::{Bounds, CalendarId, EventData, EventId, Moment, Zone, ical, sync, synth, tz};
use jolta_core::store::{self, OccurrenceRow};

use crate::{pct, truncate};

#[derive(Subcommand)]
pub enum CalCommand {
    /// Turn calendar on for an account and list its calendars. Google and
    /// Microsoft accounts first grant calendar access in the browser.
    Enable {
        account: String,
        /// Listen for the sign-in on this port, for an SSH tunnel (see `account add-microsoft`).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Turn calendar off for an account: its calendars leave Jolta, the server keeps them.
    Disable { account: String },
    /// Every calendar with its source, account, settings and event count.
    Calendars,
    /// Show or hide a calendar. Hidden calendars aren't synced.
    Show {
        calendar: i64,
        #[arg(value_enum)]
        state: OnOff,
    },
    /// Subscribe to a feed (webcal:// or https://).
    Subscribe { url: String },
    /// Sync shown calendars, or one.
    Sync { calendar: Option<i64> },
    /// Occurrences by day in this Mac's zone; this week (Monday first) by default.
    List {
        #[arg(long)]
        from: Option<String>,
        /// The last day shown.
        #[arg(long)]
        to: Option<String>,
    },
    /// Expand the events of an .ics file without storing them.
    Expand {
        file: PathBuf,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
    },
    /// Print the online meeting found in each event of an .ics file.
    Meeting { file: PathBuf },
    /// Build the synthetic calendar: 5 calendars, 20,000 events, 300 repeating.
    Gen,
    /// Read an .ics file into a local calendar of that name, replacing its events.
    Import {
        file: PathBuf,
        #[arg(long)]
        name: String,
        /// "#rrggbb".
        #[arg(long)]
        color: Option<String>,
    },
    /// Answer an invitation through the event's calendar, which sends the organizer the reply.
    #[command(group = clap::ArgGroup::new("answer").required(true))]
    Rsvp {
        /// The event's id, as `jolta cal list` shows it after the title.
        event: i64,
        #[arg(long, group = "answer")]
        accept: bool,
        #[arg(long, group = "answer")]
        maybe: bool,
        #[arg(long, group = "answer")]
        decline: bool,
        /// A note for the organizer.
        #[arg(long)]
        note: Option<String>,
        /// Save the answer without sending the organizer a reply.
        #[arg(long)]
        no_reply: bool,
        /// Answer one occurrence of a series: its day (2026-11-12) or recurrence id (20261112T100000).
        #[arg(long)]
        occurrence: Option<String>,
    },
    /// Answer the invitation on a message as the reader's card does: through
    /// the calendar's provider, the CalDAV copy and a reply mail, or the
    /// reply mail alone. Waits until a reply mail has left.
    #[command(group = clap::ArgGroup::new("reply").required(true))]
    Answer {
        /// The message's id.
        message: i64,
        #[arg(long, group = "reply")]
        accept: bool,
        #[arg(long, group = "reply")]
        maybe: bool,
        #[arg(long, group = "reply")]
        decline: bool,
        #[arg(long)]
        note: Option<String>,
        /// Keep the answer without telling the organizer.
        #[arg(long)]
        no_reply: bool,
        /// Only the occurrence the card shows.
        #[arg(long)]
        occurrence: bool,
    },
    /// Print the invitation cards of a message as the reader shows them,
    /// with what an update struck through as ~~old~~.
    Card { message: i64 },
    /// Take the meeting a cancellation names out of the calendar, as its
    /// card's Remove from calendar does.
    Remove { message: i64 },
    /// Check one provider fact from the calendar plan against an account.
    Fact {
        #[arg(value_enum)]
        fact: crate::cal_fact::Fact,
        /// The account (id or address); the Google fact needs none.
        account: Option<String>,
    },
    /// Time one-week occurrence queries over the store.
    Bench {
        #[arg(long, default_value_t = 200)]
        runs: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

pub async fn run(jolta: &Jolta, cmd: CalCommand) -> Result<()> {
    match cmd {
        CalCommand::Enable { account, port } => {
            let a = jolta.store.read(|c| store::find_account(c, &account))?;
            if sync::needs_consent(&a) {
                let provider = Provider::for_kind(a.kind).context("an OAuth account")?;
                let pending = oauth::begin_calendar(&a, provider.client(&jolta.data_dir)?, port).await?;
                println!("Jolta needs your permission to read {}'s calendars.", a.address);
                let signed = crate::browser_sign_in(pending, port, a.provider_label()).await?;
                oauth::save_grant(&a, &signed)?;
            }
            let t = Instant::now();
            let list = sync::enable(jolta, a.id).await?;
            println!("Calendar is on for {} ({} calendars, listed in {:.1} s):", a.address, list.len(), t.elapsed().as_secs_f64());
            for c in &list {
                println!("  {:>4}  [{}] {}", c.id, if c.shown { "x" } else { " " }, c.name);
            }
            println!("Hidden calendars aren't synced. Show one with `jolta cal show <id> on`.");
            Ok(())
        }
        CalCommand::Disable { account } => {
            let a = jolta.store.read(|c| store::find_account(c, &account))?;
            sync::disable(jolta, a.id)?;
            jolta.collect_blobs(std::time::Duration::from_secs(3600))?;
            println!("Calendar is off for {}. Its calendars and events are gone from Jolta; the server keeps them.", a.address);
            Ok(())
        }
        CalCommand::Calendars => {
            let calendars = jolta.store.read(store::calendars)?;
            let counts = jolta.store.read(store::event_counts)?;
            let accounts: BTreeMap<_, _> =
                jolta.store.read(store::accounts)?.into_iter().map(|a| (a.id, a.address)).collect();
            println!("{:>4}  {:<8} {:<28} {:<5} {:<4} {:>7}  name", "id", "source", "account", "shown", "busy", "events");
            for c in calendars {
                let account = c.account_id.and_then(|id| accounts.get(&id)).map_or("-", String::as_str);
                println!(
                    "{:>4}  {:<8} {:<28} {:<5} {:<4} {:>7}  {}",
                    c.id,
                    c.source,
                    truncate(account, 28),
                    yes(c.shown),
                    yes(c.counts_as_busy),
                    counts.get(&c.id).copied().unwrap_or(0),
                    c.name
                );
            }
            Ok(())
        }
        CalCommand::Show { calendar, state } => {
            let id = CalendarId(calendar);
            let shown = matches!(state, OnOff::On);
            jolta.store.write(|tx| store::set_calendar_shown(tx, id, shown))?;
            let c = jolta.store.read(|c| store::calendar(c, id))?;
            println!("{} is {}.", c.name, if shown { "shown; run `jolta cal sync` to fetch it" } else { "hidden" });
            Ok(())
        }
        CalCommand::Subscribe { url } => {
            let c = sync::subscribe(jolta, &url).await?;
            println!("Subscribed: calendar {} ({}). Run `jolta cal sync {}` next.", c.id, c.name, c.id);
            Ok(())
        }
        CalCommand::Sync { calendar } => {
            let calendars = match calendar {
                Some(id) => vec![jolta.store.read(|c| store::calendar(c, CalendarId(id)))?],
                None => sync::shown(jolta)?,
            };
            let t = Instant::now();
            let mut failed = 0;
            for c in &calendars {
                match sync::sync_calendar(jolta, c.id).await {
                    Ok(r) if r.upserted + r.removed == 0 => {
                        println!("{:>4}  {}: no changes ({:.2} s)", c.id, c.name, r.elapsed.as_secs_f64())
                    }
                    Ok(r) => println!(
                        "{:>4}  {}: {} changed, {} removed ({:.2} s)",
                        c.id,
                        c.name,
                        r.upserted,
                        r.removed,
                        r.elapsed.as_secs_f64()
                    ),
                    Err(e) => {
                        failed += 1;
                        println!("{:>4}  {}: failed: {e}", c.id, c.name);
                    }
                }
            }
            println!("{} calendars in {:.2} s.", calendars.len(), t.elapsed().as_secs_f64());
            if failed > 0 {
                bail!("{failed} calendars failed to sync");
            }
            Ok(())
        }
        CalCommand::List { from, to } => {
            let here = tz::here();
            let (from, to) = range(from.as_deref(), to.as_deref(), here)?;
            let rows = jolta.store.read(|c| store::occurrences(c, from, to, here))?;
            let names: BTreeMap<CalendarId, String> =
                jolta.store.read(store::calendars)?.into_iter().map(|c| (c.id, c.name)).collect();
            let mut days: BTreeMap<NaiveDate, Vec<String>> = BTreeMap::new();
            for o in &rows {
                let (day, when) = when(&o.bounds, here, from);
                let response = o.my_response.map(|r| r.to_string()).unwrap_or_default();
                let meeting = o.meeting.as_ref().map(|m| m.provider.to_string()).unwrap_or_default();
                let calendar = names.get(&o.calendar_id).map_or("", String::as_str);
                days.entry(day).or_default().push(format!(
                    "  {when:<19} {:<40} {:>6} {:<18} {response:<12} {meeting}",
                    truncate(&title_of(o), 40),
                    format!("#{}", o.event_id),
                    truncate(calendar, 18)
                ));
            }
            for (day, lines) in days {
                println!("{}", day.format("%a %-d %b %Y"));
                for l in lines {
                    println!("{}", l.trim_end());
                }
            }
            println!("{} occurrences from {from} to {}.", rows.len(), to - Duration::days(1));
            Ok(())
        }
        CalCommand::Expand { file, from, to } => {
            let here = tz::here();
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let (from, to) = range(from.as_deref(), to.as_deref(), here)?;
            let midnight = |d: NaiveDate| tz::instant(d.and_time(chrono::NaiveTime::MIN), here);
            let horizon = Horizon { window: jolta_core::calendar::recur::Window { start: midnight(from), end: midnight(to) }, here };
            let mut out: Vec<(Bounds, String)> = Vec::new();
            for (_, events) in series::group(ical::read(&text, &[])?) {
                let (masters, overrides): (Vec<EventData>, Vec<EventData>) =
                    events.into_iter().partition(|e| e.recurrence_id.is_none());
                for occ in series::place(masters.first(), &overrides, &horizon) {
                    let event = match occ.source {
                        Origin::Master => &masters[0],
                        Origin::Override(i) => &overrides[i],
                    };
                    let mark = if matches!(occ.source, Origin::Override(_)) { "  (changed)" } else { "" };
                    out.push((occ.span.bounds(here), format!("{}{mark}", event.title)));
                }
            }
            out.sort_by_key(|(b, _)| start_key(b, here));
            for (bounds, title) in &out {
                let (day, when) = when(bounds, here, from);
                println!("{}  {when:<19} {title}", day.format("%a %Y-%m-%d"));
            }
            println!("{} occurrences.", out.len());
            Ok(())
        }
        CalCommand::Meeting { file } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            for e in ical::read(&text, &[])? {
                match &e.meeting {
                    None => println!("{}: no online meeting", e.title),
                    Some(m) => {
                        println!("{}: {} {}", e.title, m.provider, m.join_url);
                        for (label, value) in [("meeting id", &m.meeting_id), ("passcode", &m.passcode)] {
                            if let Some(v) = value {
                                println!("  {label}: {v}");
                            }
                        }
                        for p in &m.phones {
                            println!("  phone: {p}");
                        }
                        if let Some(app) = jolta_core::calendar::meeting::app_url(m) {
                            println!("  app: {app}");
                        }
                        for l in &m.other_links {
                            println!("  other link: {l}");
                        }
                    }
                }
            }
            Ok(())
        }
        CalCommand::Gen => {
            let t = Instant::now();
            let built = synth::generate(jolta, Utc::now())?;
            println!(
                "{} calendars, {} events ({} repeating, {} changed occurrences), {} occurrences, built in {:.1} s.",
                synth::CALENDARS,
                built.events,
                synth::SERIES,
                built.overrides,
                built.occurrences,
                t.elapsed().as_secs_f64()
            );
            Ok(())
        }
        CalCommand::Import { file, name, color } => {
            let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
            let me: Vec<String> = jolta.store.read(store::accounts)?.into_iter().map(|a| a.address).collect();
            let items = series::group(ical::read(&text, &me)?);
            let horizon = Horizon::around(Utc::now(), tz::here());
            let remote = store::RemoteCalendar {
                remote_id: format!("import:{name}"),
                name: name.clone(),
                color,
                zone: None,
                access: jolta_core::calendar::Access::Owner,
                primary: true,
                refresh: None,
            };
            let count = jolta.store.write(|tx| {
                let calendar = store::add_calendar(tx, jolta_core::calendar::CalendarSource::Local, &remote)?;
                tx.execute("DELETE FROM event WHERE calendar_id = ?1", [calendar.id])?;
                for (uid, events) in &items {
                    store::apply_item(tx, calendar.id, uid, None, events, None, &horizon)?;
                }
                Ok(items.len())
            })?;
            println!("{count} events in the local calendar {name}.");
            Ok(())
        }
        CalCommand::Answer { message, accept, maybe, decline: _, note, no_reply, occurrence } => {
            let answer = if accept {
                Answer::Accept
            } else if maybe {
                Answer::Maybe
            } else {
                Answer::Decline
            };
            let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let sink: jolta_core::engine::Sink = std::sync::Arc::new(move |e| {
                let _ = events.send(e);
            });
            let engine = jolta_core::engine::Engine::start(jolta.clone(), sink, tokio::runtime::Handle::current())?;
            let request = respond::Request {
                subject: respond::Subject::Message(jolta_core::MessageId(message)),
                answer,
                note,
                notify: !no_reply,
                occurrence_only: occurrence,
            };
            let t = Instant::now();
            let done = respond::answer(jolta, &request).await?;
            println!("{} ({:.2} s)", done.text, t.elapsed().as_secs_f64());
            let Some(account) = done.mailed_from else { return Ok(()) };
            engine.wake(account);
            let wait = tokio::time::timeout(std::time::Duration::from_secs(120), async {
                while let Some(e) = rx.recv().await {
                    match e {
                        jolta_core::engine::Event::Sent { subject, .. } => return Ok(subject),
                        jolta_core::engine::Event::SendFailed(m) => bail!("the reply mail failed: {m}"),
                        _ => {}
                    }
                }
                bail!("the engine stopped")
            });
            let subject = wait.await.context("the reply mail didn't leave within two minutes")??;
            println!("sent {subject:?} after {:.1} s", t.elapsed().as_secs_f64());
            // Lets the engine file the Sent copy.
            tokio::time::sleep(std::time::Duration::from_secs(8)).await;
            Ok(())
        }
        CalCommand::Card { message } => {
            for card in message_cards(jolta, jolta_core::MessageId(message)).await? {
                print_card(&card);
            }
            Ok(())
        }
        CalCommand::Remove { message } => {
            let t = Instant::now();
            let done = remove::remove(jolta, jolta_core::MessageId(message)).await?;
            println!("{} ({:.2} s)", done.text, t.elapsed().as_secs_f64());
            Ok(())
        }
        CalCommand::Rsvp { event, accept, maybe, decline: _, note, no_reply, occurrence } => {
            let answer = if accept {
                Answer::Accept
            } else if maybe {
                Answer::Maybe
            } else {
                Answer::Decline
            };
            let event = EventId(event);
            let scope = match &occurrence {
                Some(spec) => Scope::Occurrence(rsvp::occurrence(jolta, event, spec)?),
                None => Scope::Series,
            };
            let (calendar, uid, title): (CalendarId, String, String) = jolta.store.read(|c| {
                Ok(c.query_row("SELECT calendar_id, uid, title FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
            })?;
            let what = match &scope {
                Scope::Series => "the whole event".to_string(),
                Scope::Occurrence(rid) => format!("the occurrence of {}", moment_text(rid)),
            };
            let t = Instant::now();
            rsvp::answer(jolta, &Rsvp { event, answer, note, notify: !no_reply, scope }).await?;
            println!(
                "{} {what} of {title:?} in {:.2} s; {}.",
                match answer {
                    Answer::Accept => "Accepted",
                    Answer::Maybe => "Tentatively accepted",
                    Answer::Decline => "Declined",
                },
                t.elapsed().as_secs_f64(),
                if no_reply { "no reply goes to the organizer" } else { "the organizer gets a reply" }
            );
            let report = sync::sync_calendar(jolta, calendar).await?;
            println!("Synced the calendar: {} changed, {} removed. Stored answers now:", report.upserted, report.removed);
            let rows: Vec<String> = jolta.store.read(|c| {
                let mut stmt = c.prepare("SELECT data FROM event WHERE calendar_id = ?1 AND uid = ?2 ORDER BY recurrence_id")?;
                let rows = stmt.query_map((calendar, &uid), |r| r.get(0))?;
                Ok(rows.collect::<std::result::Result<Vec<String>, _>>()?)
            })?;
            for data in rows {
                let e: EventData = serde_json::from_str(&data)?;
                let me = e.attendees.iter().find(|a| a.is_me);
                println!(
                    "  {:<34} {:<12} {}",
                    e.recurrence_id.as_ref().map_or("series or single event".to_string(), moment_text),
                    me.map_or("-".to_string(), |a| a.response.to_string()),
                    me.map(|a| a.comment.as_str()).filter(|c| !c.is_empty()).map(|c| format!("{c:?}")).unwrap_or_default()
                );
            }
            Ok(())
        }
        CalCommand::Fact { fact, account } => {
            let account = account.map(|spec| jolta.store.read(|c| store::find_account(c, &spec))).transpose()?;
            crate::cal_fact::run(fact, account.as_ref()).await
        }
        CalCommand::Bench { runs } => bench(jolta, runs),
    }
}

/// One-week queries for weeks spread over the stored window, the current
/// week included.
fn bench(jolta: &Jolta, runs: usize) -> Result<()> {
    let here = tz::here();
    let this_monday = monday(Utc::now().with_timezone(&here).date_naive());
    let total: i64 = jolta.store.read(|c| Ok(c.query_row("SELECT count(*) FROM occurrence", [], |r| r.get(0))?))?;
    let mut times = Vec::with_capacity(runs);
    let mut rows = 0;
    for i in 0..runs {
        let offset = if i == 0 { 0 } else { (i as i64 * 7_919) % 156 - 52 };
        let from = this_monday + Duration::weeks(offset);
        let t = Instant::now();
        let found: Vec<OccurrenceRow> = jolta.store.read(|c| store::occurrences(c, from, from + Duration::days(7), here))?;
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        rows += found.len();
    }
    let mean = rows as f64 / runs as f64;
    let (p50, p95) = (pct(&mut times, 0.50), pct(&mut times, 0.95));
    println!("{total} occurrences stored; {runs} one-week queries, {mean:.0} occurrences a week on average.");
    println!("week query p50 {p50:.2} ms, p95 {p95:.2} ms (budget 5 ms).");
    Ok(())
}

fn monday(day: NaiveDate) -> NaiveDate {
    day - Duration::days(day.weekday().num_days_from_monday().into())
}

/// `--from` and `--to` (the last day, inclusive) as a half-open range of
/// days; this week from Monday by default.
fn range(from: Option<&str>, to: Option<&str>, here: chrono_tz::Tz) -> Result<(NaiveDate, NaiveDate)> {
    let parse = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").with_context(|| format!("{s} is not a date like 2026-11-09"));
    let from = match from {
        Some(s) => parse(s)?,
        None => monday(Utc::now().with_timezone(&here).date_naive()),
    };
    let to = match to {
        Some(s) => parse(s)? + Duration::days(1),
        None => from + Duration::days(7),
    };
    if to <= from {
        bail!("--to comes before --from");
    }
    Ok((from, to))
}

/// The day an occurrence is listed under (its start, or the first day of the
/// range when it started earlier) and its time as text.
fn when(bounds: &Bounds, here: chrono_tz::Tz, first: NaiveDate) -> (NaiveDate, String) {
    match *bounds {
        Bounds::Days { start, end } => {
            let text = match (day(start) < first, end - start > 1) {
                (true, _) => format!("all day until {}", day(end - 1).format("%a %-d")),
                (false, true) => format!("all day to {}", day(end - 1).format("%a %-d")),
                (false, false) => "all day".into(),
            };
            (day(start).max(first), text)
        }
        Bounds::Utc { start, end } => {
            let (s, e) = (start.with_timezone(&here), end.with_timezone(&here));
            let end_text = if e.date_naive() == s.date_naive() || e - s <= Duration::zero() {
                e.format("%H:%M").to_string()
            } else {
                e.format("%a %-d %H:%M").to_string()
            };
            let text = if s.date_naive() < first {
                format!("until {end_text}")
            } else {
                format!("{}–{end_text}", s.format("%H:%M"))
            };
            (s.date_naive().max(first), text)
        }
    }
}

fn start_key(bounds: &Bounds, here: chrono_tz::Tz) -> DateTime<Utc> {
    match *bounds {
        Bounds::Days { start, .. } => tz::instant(day(start).and_time(chrono::NaiveTime::MIN), here),
        Bounds::Utc { start, .. } => start,
    }
}

fn day(n: i32) -> NaiveDate {
    NaiveDate::default() + Duration::days(n.into())
}

fn title_of(o: &OccurrenceRow) -> String {
    if o.title.is_empty() { "(no title)".into() } else { o.title.clone() }
}

fn moment_text(m: &Moment) -> String {
    match m {
        Moment::Date(d) => d.format("%a %-d %b %Y").to_string(),
        Moment::Time(t) => match t.zone {
            Zone::Tz(zone) => format!("{} {zone}", t.wall.format("%a %-d %b %Y %H:%M")),
            Zone::Floating => t.wall.format("%a %-d %b %Y %H:%M").to_string(),
        },
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// The cards of a message, built as the reader builds them.
async fn message_cards(jolta: &Jolta, message: jolta_core::MessageId) -> Result<Vec<Card>> {
    let (account, sender): (jolta_core::AccountId, String) =
        jolta.store.read(|c| Ok(c.query_row("SELECT account_id, from_addr FROM message WHERE id = ?1", [message], |r| Ok((r.get(0)?, r.get(1)?)))?))?;
    let raw = jolta_core::imap::message_source(jolta, message).await?;
    Ok(jolta.store.read(|c| {
        let me: Vec<String> = store::accounts(c)?.into_iter().map(|a| a.address).collect();
        let found = invitation::find(&raw, &me);
        let cx = card::Context { message, account, me: &me, sender: &sender, now: Utc::now(), here: tz::here(), blobs: &jolta.blobs };
        card::cards(c, &found, &cx)
    })?)
}

fn print_card(card: &Card) {
    let edit = |e: &Edit| {
        let old = if e.old.is_empty() { String::new() } else { format!("~~{}~~ ", e.old) };
        let new = if e.new.is_empty() { String::new() } else { format!("[{}]", e.new) };
        format!("{}{old}{new}{}", e.lead, e.tail)
    };
    match card {
        Card::Reply(r) => println!("{} {}", r.line, r.about),
        Card::Event(e) => {
            println!("{}{}: {}", e.label, if e.outdated { " (outdated)" } else { "" }, e.title);
            let line = |name: &str, changed: &Option<Edit>, now: &Option<String>| match (changed, now) {
                (Some(c), _) => println!("  {name}: {}", edit(c)),
                (None, Some(t)) => println!("  {name}: {t}"),
                (None, None) => {}
            };
            line("when", &e.changes.when, &e.when);
            line("repeat", &e.changes.repeat, &e.repeat);
            line("place", &e.changes.location, &e.location);
            line("link", &e.changes.join, &e.join.as_ref().map(|j| j.provider.to_string()));
            for n in &e.notes {
                println!("  note: {}", n.text);
            }
            let answer = e.answer.map_or("none", card::answer_word);
            println!("  answer: {answer}{}", if e.answering.is_some() { ", Accept, Maybe and Decline offered" } else { "" });
            if let Some(r) = &e.removing {
                println!("  Remove from calendar offered{}", r.occurrence.as_ref().map(|(_, d)| format!(" for {d}")).unwrap_or_default());
            }
        }
    }
}
