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
use jolta_core::calendar::availability::{self, Availability, Busy};
use jolta_core::calendar::zones::{self, ZoneGuess};
use jolta_core::calendar::{add, away, freebusy, invitation, proposal, remove, respond};
use jolta_core::calendar::rsvp::{self, Answer, Rsvp, Scope};
use jolta_core::calendar::{Bounds, CalendarId, EventData, EventId, EventKind, Moment, Zone, ical, sync, synth, tz};
use jolta_core::store::{self, OccurrenceRow};

use crate::{pct, truncate};

/// Find a time's lookup for the CLI, with `rooms` after the people as the
/// panel adds them; calendar off is an error here.
async fn find_time_lookup(jolta: &Jolta, account: &str, addresses: &[String], rooms: &[String], days: u32) -> Result<freebusy::Found> {
    let a = jolta.store.read(|c| store::find_account(c, account))?;
    match freebusy::look_up(jolta, a.id, addresses, Utc::now(), days).await? {
        freebusy::Lookup::Found(mut found) => {
            if let (Some(first), Some(last)) = (found.windows.first(), found.windows.last()) {
                found.others.extend(freebusy::room_schedules(jolta, a.id, rooms, first.start, last.end).await?);
            }
            Ok(found)
        }
        freebusy::Lookup::CalendarOff { address, .. } => bail!("calendar is off for {address}; run `jolta cal enable {address}` first"),
    }
}

/// `jolta cal rooms`: asks for the room lists when told to, reads them
/// again, and prints each room, free or busy at `free` when given.
async fn rooms_command(jolta: &Jolta, account: &str, consent: bool, port: u16, free: Option<&str>, minutes: i64, typed: &[String]) -> Result<()> {
    use jolta_core::calendar::rooms::{self as rooms, Access};
    let a = jolta.store.read(|c| store::find_account(c, account))?;
    if consent {
        let (a, pending) = rooms::begin_consent(jolta, a.id, port).await?;
        println!("Jolta asks for {}'s room lists (Place.Read.All). Only an admin may be able to allow it.", a.address);
        let answer = crate::browser_consent(pending, port, a.provider_label()).await?;
        rooms::finish_consent(jolta, &a, answer).await?;
    }
    let listing = rooms::refresh(jolta, a.id).await?;
    let access = match &listing.access {
        Access::NotOffered => "not offered by this provider; recent rooms only".to_string(),
        Access::NotAsked => "not asked yet (run with --consent)".to_string(),
        Access::Refused(reason) => format!("refused: {reason}"),
        Access::Granted if listing.none_listed() => "allowed, but the organization lists no rooms".to_string(),
        Access::Granted => "allowed".to_string(),
    };
    println!("{}: room lists {access}", a.address);
    let mut shown: Vec<(String, String, String)> = listing
        .rooms
        .iter()
        .map(|r| {
            let size = r.capacity.map_or_else(|| "size unknown".to_string(), |c| format!("{c} people"));
            let place = [r.building.clone(), r.floor.clone().map(|f| format!("floor {f}"))].into_iter().flatten().collect::<Vec<_>>().join(", ");
            let list = r.list.as_deref().map_or(String::new(), |l| format!(" [{l}]"));
            (r.address.clone(), r.name.clone(), format!("{size}{}{list}, {:?}", if place.is_empty() { String::new() } else { format!(", {place}") }, r.source))
        })
        .collect();
    for t in typed {
        if !shown.iter().any(|(address, ..)| address.eq_ignore_ascii_case(t)) {
            shown.push((t.to_ascii_lowercase(), t.clone(), "typed".into()));
        }
    }
    if shown.is_empty() {
        println!("No rooms: none listed, and none on this account's meetings of the last 180 days.");
        return Ok(());
    }
    let states = match free {
        None => Vec::new(),
        Some(when) => {
            let wall = chrono::NaiveDateTime::parse_from_str(when.trim(), "%Y-%m-%d %H:%M").with_context(|| format!("{when:?} is not a time like \"2026-11-10 14:00\""))?;
            let (start, end) = (tz::instant(wall, tz::here()), tz::instant(wall, tz::here()) + Duration::minutes(minutes));
            let addresses: Vec<String> = shown.iter().map(|(address, ..)| address.clone()).collect();
            let found = freebusy::room_schedules(jolta, a.id, &addresses, start, end).await?;
            found
                .iter()
                .map(|s| match &s.availability {
                    Availability::Known(busy) if busy.iter().any(|b| b.kind.blocks() && b.start < end && b.end > start) => "busy".to_string(),
                    Availability::Known(_) => "free".to_string(),
                    Availability::Unknown(why) => format!("unknown, {}", why.reason()),
                })
                .collect()
        }
    };
    for (i, (address, name, detail)) in shown.iter().enumerate() {
        let state = states.get(i).map_or(String::new(), |s| format!("  {s}"));
        println!("  {name} <{address}>  {detail}{state}");
    }
    Ok(())
}

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
    /// Read the invitations in stored bodies not read yet, then count the
    /// conversations by the state their list row shows, and what
    /// `is:invite` finds.
    Invitations,
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
    /// Propose a new time for the invitation on a message, as the card's
    /// panel does: for a series, for the occurrence the card shows. Waits
    /// until a COUNTER mail has left.
    #[command(group = clap::ArgGroup::new("with").required(true))]
    Propose {
        message: i64,
        #[arg(long, group = "with")]
        maybe: bool,
        #[arg(long, group = "with")]
        decline: bool,
        /// The new start on this Mac's clock: "2026-11-13 15:00".
        #[arg(long)]
        start: String,
        /// The new end; the meeting keeps its length without one.
        #[arg(long)]
        end: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Decline the guest's proposal on a message, as its card's Decline
    /// proposal does. Waits until the mail has left.
    DeclineProposal { message: i64 },
    /// Email guests, as the event's detail does: print the mail it opens,
    /// to everyone on the event but you and rooms; with --to, to that one
    /// person; with --send, send it with that text first.
    EmailGuests {
        /// The event's id, as `jolta cal list` shows it after the title.
        event: i64,
        /// One occurrence of a series, by its day: "2026-11-24".
        #[arg(long)]
        occurrence: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        send: Option<String>,
    },
    /// Accept the guest's proposal on a message, as its card's Accept
    /// proposal and the editor's Save do: the meeting moves to the time
    /// proposed and Send an update? tells the guests as --notify says.
    AcceptProposal(crate::cal_edit::AcceptProposalArgs),
    /// Print the invitation cards of a message as the reader shows them,
    /// with what an update struck through as ~~old~~.
    Card { message: i64 },
    /// Turn File responses automatically on or off, as Settings › Calendar
    /// does, or print whether it is on. `jolta watch` files the replies.
    FileResponses {
        #[arg(value_enum)]
        state: Option<OnOff>,
    },
    /// Take the meeting a cancellation names out of the calendar, as its
    /// card's Remove from calendar does.
    Remove { message: i64 },
    /// Put the invitation on a message into a calendar, as its card's Add to
    /// calendar does, telling nobody. Without a calendar, the menu's default;
    /// with `--list`, print the menu.
    Add {
        message: i64,
        calendar: Option<i64>,
        #[arg(long)]
        list: bool,
    },
    /// Write the newer invitation on a message over the copy added to a
    /// calendar, as its card's Update in your calendar does.
    Update { message: i64 },
    /// Each person's busy blocks over the next working days, looked up
    /// through the account's provider, or why they aren't known. Reads only.
    Freebusy {
        account: String,
        addresses: Vec<String>,
        /// A room's address, looked up beside the people.
        #[arg(long = "room")]
        rooms: Vec<String>,
        /// Working days to look at.
        #[arg(long, default_value_t = 5)]
        days: u32,
    },
    /// Each Google and Microsoft 365 account's automatic replies as the
    /// provider says now, or one account's; stores what it read. With
    /// --from and --until, sets out of office for the account as the sheet
    /// does; with --off, ends what is on now, else the next one set.
    Away {
        account: Option<String>,
        /// The first day ("2026-11-11") or moment ("2026-11-11 14:00").
        #[arg(long, requires = "until", requires = "account")]
        from: Option<String>,
        /// The last day, included, or the moment it ends.
        #[arg(long, requires = "from")]
        until: Option<String>,
        #[arg(long, default_value = "")]
        message: String,
        /// What Google's out-of-office event declines.
        #[arg(long, value_enum, default_value_t = DeclineArg::New)]
        decline: DeclineArg,
        /// Who outside the organization gets Microsoft 365's reply.
        #[arg(long, value_enum, default_value_t = OutsideArg::All)]
        outside: OutsideArg,
        #[arg(long, requires = "account", conflicts_with = "from")]
        off: bool,
    },
    /// The personal Zoom link Add video call offers, as Settings › Calendar
    /// saves it: shown without arguments, saved, or cleared.
    ZoomLink {
        link: Option<String>,
        #[arg(long, conflicts_with = "link")]
        clear: bool,
    },
    /// Focus time in a calendar, as the app's sheet adds it: Google's own
    /// focus time in a Google account's primary calendar where its edition
    /// offers it, else a busy event titled Focus time. Sent at once.
    Focus {
        calendar: i64,
        /// When it starts on this Mac's clock, as "2026-11-18 14:00".
        #[arg(long)]
        start: String,
        #[arg(long, default_value_t = 120)]
        minutes: i64,
        /// What Google's focus time declines.
        #[arg(long, value_enum, default_value_t = DeclineArg::New)]
        decline: DeclineArg,
        /// Google Chat's Do not disturb during it.
        #[arg(long)]
        dnd: bool,
    },
    /// The clock Insert my availability writes in for a recipient: the zone
    /// picked for them, their Microsoft 365 working hours' zone through
    /// `account`, or a zone with their last mail's offset. `--set` picks one.
    ZoneOf {
        account: String,
        address: String,
        /// An IANA zone such as America/New_York, remembered for the address.
        #[arg(long)]
        set: Option<String>,
    },
    /// Every hold set with its slots and their states, after building the
    /// sets whose hold events sync found from their sent mail, and the hold
    /// events of sets this Mac doesn't know. `--check` prints what Jolta
    /// makes of each open set now, without acting.
    Holds {
        #[arg(long)]
        check: bool,
        /// Release every held time of this set, as Release all does.
        #[arg(long, value_name = "SET")]
        release: Option<String>,
        /// Take back the releases of these calendar ops, as a toast's Undo does.
        #[arg(long, value_name = "SET", requires = "ops")]
        undo: Option<String>,
        #[arg(long = "op")]
        ops: Vec<i64>,
    },
    /// Hold times as if a message offering them had just left, for checks
    /// without mail: one tentative, private "Hold: <subject>" event per
    /// time in the account's calendar. Sent at once.
    Hold {
        account: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "")]
        subject: String,
        /// "2026-11-10 16:00" on this Mac's clock.
        #[arg(long = "slot", required = true)]
        slots: Vec<String>,
        #[arg(long, default_value_t = 30)]
        minutes: i64,
        /// The Message-ID of a stored sent mail the holds belong to, so its
        /// conversation shows them.
        #[arg(long)]
        message_id: Option<String>,
    },
    /// Times everyone known is free for a meeting, as Find a time suggests
    /// them. Reads only.
    FindTime {
        account: String,
        addresses: Vec<String>,
        /// A room's address: a busy room blocks a time like a busy person.
        #[arg(long = "room")]
        rooms: Vec<String>,
        #[arg(long, default_value_t = 30)]
        minutes: i64,
        #[arg(long, default_value_t = 5)]
        days: u32,
        /// Also suggest times outside other people's working hours.
        #[arg(long)]
        any_hours: bool,
    },
    /// The rooms an account offers for meetings: on Microsoft 365 its
    /// organization's room lists once allowed, and on every account the
    /// rooms of its meetings of the last 180 days. Reads the room lists again.
    Rooms {
        account: String,
        /// Ask Microsoft 365 for the room lists (Place.Read.All) in the browser.
        #[arg(long)]
        consent: bool,
        /// Listen for the consent on this port, for an SSH tunnel.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Say whether each room is free from this moment ("2026-11-10 14:00").
        #[arg(long)]
        free: Option<String>,
        #[arg(long, default_value_t = 30, requires = "free")]
        minutes: i64,
        /// A typed room address to look up beside the rest.
        #[arg(long = "room")]
        rooms: Vec<String>,
    },
    /// Check one provider fact from the calendar plan against an account.
    Fact {
        #[arg(value_enum)]
        fact: crate::cal_fact::Fact,
        /// The account (id or address); the Google fact needs none.
        account: Option<String>,
    },
    /// Make a calendar on this Mac only.
    NewLocal {
        name: String,
        /// "#rrggbb".
        #[arg(long)]
        color: Option<String>,
    },
    /// Create an event through the op queue, telling nobody.
    Create(crate::cal_edit::CreateArgs),
    /// Change an event's title, place or time through the op queue.
    Edit(crate::cal_edit::EditArgs),
    /// Copy an event as ⌥-drag and ⌘D do: a new event without its people, repeat or meeting link.
    Copy(crate::cal_edit::CopyArgs),
    /// Delete an event, or a series with its changed occurrences, through the op queue.
    Delete(crate::cal_edit::DeleteArgs),
    /// Move an event, with its whole series, to another calendar: within an
    /// account by the server's own move, else a copy and then a delete.
    /// Refused, with the reason, where the guests would hear of it.
    Move(crate::cal_edit::MoveArgs),
    /// The calendar op queue, oldest first, with each op's state.
    Ops {
        /// Send the ops that are due first, as the app's worker does.
        #[arg(long)]
        send: bool,
    },
    /// Take back a calendar op: drop it if it was never sent, else queue its opposite.
    Undo { op: i64 },
    /// Settle a conflict the user's way: queue the edit's lost fields over
    /// the server's version, or the delete again.
    ApplyMine {
        op: i64,
        /// Wait until the new op is settled.
        #[arg(long)]
        wait: bool,
    },
    /// What Jolta can do with a calendar's events.
    Caps { calendar: i64 },
    /// The account a calendar on this Mac sends its meetings' invitations from.
    SendFrom { calendar: i64, account: String },
    /// The event as the editor opens it: its fields, what is locked and
    /// why, and what is wrong with it for its calendar.
    ///
    /// With --save or --delete it saves or deletes through the editor's own
    /// path, telling nobody; without an event it is a new event in
    /// --calendar, else the default calendar.
    Draft(crate::cal_edit::DraftArgs),
    /// Read a line as quick create does, "Lunch with Jan fri 12:30 at Mok",
    /// and print what it became and the editor's draft. Nothing is saved.
    Quick(crate::cal_edit::QuickArgs),
    /// The day G goes to for a typed date: "fri", "14 dec", "morgen".
    Go {
        text: Vec<String>,
        /// Today as another day, "2026-11-07".
        #[arg(long)]
        today: Option<String>,
    },
    /// The calendar new events go to, as Settings › Calendar sets it: print
    /// it, set it, or with --clear go back to the user's own calendar.
    Default {
        calendar: Option<i64>,
        #[arg(long, conflicts_with = "calendar")]
        clear: bool,
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
        CalCommand::NewLocal { name, color } => crate::cal_edit::new_local(jolta, &name, color.as_deref()),
        CalCommand::Holds { release: Some(set), .. } => {
            crate::cal_holds::release(jolta, &set)?;
            send_calendar_ops(jolta).await
        }
        CalCommand::Holds { undo: Some(set), ops, .. } => {
            crate::cal_holds::undo(jolta, &set, &ops)?;
            send_calendar_ops(jolta).await
        }
        CalCommand::Holds { check, .. } => crate::cal_holds::holds(jolta, check),
        CalCommand::Hold { account, to, subject, slots, minutes, message_id } => {
            crate::cal_holds::hold(jolta, &account, &to, &subject, &slots, minutes, message_id)?;
            send_calendar_ops(jolta).await?;
            crate::cal_holds::holds(jolta, false)
        }
        CalCommand::Create(a) => crate::cal_edit::create(jolta, a).await,
        CalCommand::Edit(a) => crate::cal_edit::edit(jolta, a).await,
        CalCommand::Copy(a) => crate::cal_edit::copy(jolta, a).await,
        CalCommand::Delete(a) => crate::cal_edit::delete(jolta, a).await,
        CalCommand::Move(a) => crate::cal_edit::move_event(jolta, a).await,
        CalCommand::Ops { send } => crate::cal_edit::list_ops(jolta, send).await,
        CalCommand::Undo { op } => crate::cal_edit::undo(jolta, op).await,
        CalCommand::ApplyMine { op, wait } => crate::cal_edit::apply_mine(jolta, op, wait).await,
        CalCommand::Caps { calendar } => crate::cal_edit::print_caps(jolta, calendar),
        CalCommand::SendFrom { calendar, account } => crate::cal_edit::send_from(jolta, calendar, &account),
        CalCommand::Draft(a) => crate::cal_edit::draft(jolta, a).await,
        CalCommand::Quick(a) => crate::cal_edit::quick(jolta, a).await,
        CalCommand::Go { text, today } => crate::cal_edit::go(&text.join(" "), today.as_deref()),
        CalCommand::Default { calendar, clear } => crate::cal_edit::default_calendar(jolta, calendar, clear),
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
        CalCommand::ZoomLink { link, clear } => {
            if link.is_some() || clear {
                jolta.store.write(|tx| store::set_zoom_link(tx, link.as_deref()))?;
            }
            match jolta.store.read(store::zoom_link)? {
                Some(link) => println!("Zoom personal link: {link}"),
                None => println!("No Zoom personal link is saved."),
            }
            Ok(())
        }
        CalCommand::Focus { calendar, start, minutes, decline, dnd } => add_focus(jolta, calendar, &start, minutes, decline, dnd).await,
        CalCommand::Away { account: Some(name), from: Some(from), until: Some(until), message, decline, outside, .. } => {
            set_away(jolta, &name, &from, &until, message, decline, outside).await
        }
        CalCommand::Away { account: Some(name), off: true, .. } => {
            let a = jolta.store.read(|c| store::find_account(c, &name))?;
            let here = tz::here();
            let (ended, _) = away::plan::end(jolta, a.id, away::plan::Reach::NowOrNext, &Horizon::around(Utc::now(), here), here).await?;
            println!(
                "{}: {}; {} out-of-office events ended",
                a.address,
                if ended.replies { "automatic replies turned off" } else { "automatic replies unchanged" },
                ended.events
            );
            send_calendar_ops(jolta).await
        }
        CalCommand::Away { account, .. } => {
            let accounts = match account {
                Some(name) => vec![jolta.store.read(|c| store::find_account(c, &name))?],
                None => jolta.store.read(store::accounts)?,
            };
            let here = tz::here();
            let at = |t: chrono::DateTime<Utc>| t.with_timezone(&here).format("%a %-d %b %Y %H:%M %Z").to_string();
            for a in accounts {
                let fetched = Utc::now().timestamp_millis();
                let line = match away::read(jolta, &a).await {
                    Ok(None) if a.kind == jolta_core::AccountKind::Graph => "unknown: calendar is off, so Jolta has no mailbox settings grant".to_string(),
                    Ok(None) => continue,
                    Err(e) => format!("not read: {e}"),
                    Ok(Some(w)) => {
                        jolta.store.write(|tx| store::save_away(tx, &w, fetched))?;
                        let state = match w.reply {
                            away::AutoReply::Off => "off".to_string(),
                            away::AutoReply::On { from, until } => {
                                let now = if w.reply.on_at(Utc::now()) { "on" } else { "on, not now" };
                                let from = from.map(|f| format!(" from {}", at(f))).unwrap_or_default();
                                let until = until.map_or(" with no end".to_string(), |u| format!(" until {}", at(u)));
                                format!("{now}{from}{until}")
                            }
                        };
                        let change = if w.editable { "Jolta can turn them off" } else { "changed in the provider's settings" };
                        format!("{state}; outside replies to {}; {change}", w.audience)
                    }
                };
                println!("{}  {line}", a.address);
            }
            Ok(())
        }
        CalCommand::Freebusy { account, addresses, rooms, days } => {
            let found = find_time_lookup(jolta, &account, &addresses, &rooms, days).await?;
            let here = tz::here();
            let hours_text = |h: &availability::WorkingHours| {
                let weekdays: Vec<String> = std::iter::successors(Some(chrono::Weekday::Mon), |d| Some(d.succ()))
                    .take(7)
                    .filter(|d| h.days.has(*d))
                    .map(|d| format!("{d}"))
                    .collect();
                format!("{} {} to {} ({})", weekdays.join(" "), h.start.format("%H:%M"), h.end.format("%H:%M"), h.zone)
            };
            println!("Working hours: {}", hours_text(&found.hours));
            let block = |b: &Busy| {
                let (s, e) = (b.start.with_timezone(&here), b.end.with_timezone(&here));
                let end = if e.date_naive() == s.date_naive() { e.format("%H:%M").to_string() } else { e.format("%a %-d %b %H:%M").to_string() };
                format!("  {} to {end}  {:?}", s.format("%a %-d %b %H:%M"), b.kind)
            };
            println!("{} (you, {} blocks)", found.me, found.own.len());
            found.own.iter().for_each(|b| println!("{}", block(b)));
            for s in &found.others {
                match &s.availability {
                    Availability::Known(busy) => {
                        println!("{} ({} blocks)", s.person, busy.len());
                        let hours = s.hours.as_ref().map_or_else(|| "not known".to_string(), hours_text);
                        println!("  working hours: {hours}");
                        if let Some(note) = availability::note(s, &found.windows, &found.hours) {
                            println!("  {note}");
                        }
                        busy.iter().for_each(|b| println!("{}", block(b)));
                    }
                    Availability::Unknown(why) => println!("{}: unknown, {} ({why:?})", s.person, why.reason()),
                }
            }
            Ok(())
        }
        CalCommand::FindTime { account, addresses, rooms, minutes, days, any_hours } => {
            let found = find_time_lookup(jolta, &account, &addresses, &rooms, days).await?;
            let here = tz::here();
            let options = availability::SuggestOptions { respect_others_hours: !any_hours };
            let slots = availability::suggest(&found.own, &found.others, &found.hours, Duration::minutes(minutes), Utc::now(), days, options);
            if slots.is_empty() {
                println!("No time in the next {days} working days suits everyone known.");
            }
            for s in &slots {
                let missing = if s.missing.is_empty() { String::new() } else { format!("  (unknown: {})", s.missing.join(", ")) };
                println!("{} to {}{missing}", s.start.with_timezone(&here).format("%a %-d %b %H:%M"), s.end.with_timezone(&here).format("%H:%M"));
            }
            Ok(())
        }
        CalCommand::ZoneOf { account, address, set } => {
            if let Some(name) = set {
                let zone = tz::zone(&name).with_context(|| format!("{name} is no time zone"))?;
                jolta.store.write(|tx| store::set_person_zone(tx, &address, zone, Utc::now().timestamp()))?;
            }
            let a = jolta.store.read(|c| store::find_account(c, &account))?;
            let here = tz::here();
            let names = |zones: &[chrono_tz::Tz]| zones.iter().map(|z| z.name()).collect::<Vec<_>>().join(", ");
            match zones::guess(jolta, a.id, &address, here).await? {
                None => println!("{} (yours): nothing says otherwise", here.name()),
                Some(ZoneGuess::Picked(zone)) => println!("{}: picked for {address}", zone.name()),
                Some(ZoneGuess::WorkingHours(h)) => println!("{}: their Outlook working hours, {} to {}", h.zone.name(), h.start.format("%H:%M"), h.end.format("%H:%M")),
                Some(ZoneGuess::LastMail { offset, zone, others }) => {
                    println!("{}: {} on their last mail", zone.name(), zones::offset_label(offset));
                    if !others.is_empty() {
                        println!("  also {}", names(&others));
                    }
                }
            }
            Ok(())
        }
        CalCommand::Rooms { account, consent, port, free, minutes, rooms } => rooms_command(jolta, &account, consent, port, free.as_deref(), minutes, &rooms).await,
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
        CalCommand::FileResponses { state } => {
            if let Some(state) = state {
                let now = chrono::Utc::now().timestamp();
                jolta.store.write(|tx| store::set_file_responses(tx, matches!(state, OnOff::On), now))?;
            }
            match jolta.store.read(store::file_responses_since)? {
                Some(since) => println!("File responses automatically is on, for replies arriving since {}.", chrono::DateTime::from_timestamp(since, 0).unwrap_or_default()),
                None => println!("File responses automatically is off."),
            }
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
                    titled(o, 40),
                    crate::cal_edit::event_label(o.event_id, o.not_synced),
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
                video: None,
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
        CalCommand::Invitations => {
            let t = Instant::now();
            let found = jolta_core::engine::scan_invitations(jolta)?;
            println!("{found} bodies read now held invitations ({:.1} s).", t.elapsed().as_secs_f64());
            let (now, here) = (Utc::now(), tz::here());
            let (threads, states, ms) = jolta.store.read(|c| {
                let mut stmt = c.prepare("SELECT DISTINCT m.thread_id FROM invitation i JOIN message m ON m.id = i.message_id")?;
                let threads: Vec<jolta_core::ThreadId> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
                let t = Instant::now();
                let states = store::thread_invitations(c, &threads, now, here)?;
                Ok((threads.len(), states, t.elapsed().as_secs_f64() * 1000.0))
            })?;
            let each = ms / threads.max(1) as f64;
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for s in states.values() {
                *counts.entry(format!("{s:?}")).or_default() += 1;
            }
            println!("{threads} conversations with an invitation, {each:.3} ms each:");
            for (state, n) in counts {
                println!("  {n:>6}  {state}");
            }
            let q = jolta_core::search::parse("is:invite")?;
            let t = Instant::now();
            let invites = jolta.store.read(|c| jolta_core::search::query_count(c, &q))?;
            println!("is:invite finds {invites} conversations ({:.2} ms).", t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            let change = jolta.store.read(|c| store::next_invitation_change(c, now, here))?;
            println!("The next state changes by time at {change:?} ({:.2} ms).", t.elapsed().as_secs_f64() * 1000.0);
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
            let engine = Outgoing::start(jolta)?;
            let request = respond::Request {
                subject: respond::Subject::Message(jolta_core::MessageId(message)),
                answer,
                note,
                notify: !no_reply,
                occurrence_only: occurrence,
                proposal: None,
            };
            let t = Instant::now();
            let done = respond::answer(jolta, &request).await?;
            println!("{} ({:.2} s)", done.text, t.elapsed().as_secs_f64());
            if done.queued {
                println!("Queued in the calendar ops; `jolta cal ops --send` sends it once the server can be reached.");
            }
            match done.mailed_from {
                Some(account) => engine.send(account, t).await,
                None => Ok(()),
            }
        }
        CalCommand::Card { message } => {
            for card in message_cards(jolta, jolta_core::MessageId(message)).await? {
                print_card(&card);
            }
            Ok(())
        }
        CalCommand::Propose { message, maybe: _, decline, start, end, note } => {
            let answer = if decline { Answer::Decline } else { Answer::Maybe };
            let at = |s: &str| -> Result<DateTime<Utc>> {
                let wall = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M").with_context(|| format!("{s} is not a time like 2026-11-13 15:00"))?;
                Ok(tz::instant(wall, tz::here()))
            };
            let start = at(&start)?;
            let end = match end {
                Some(e) => at(&e)?,
                None => {
                    let shown = message_cards(jolta, jolta_core::MessageId(message)).await?.into_iter().find_map(|c| match c {
                        Card::Event(e) => e.answering.and_then(|a| a.new_time),
                        Card::Reply(_) => None,
                    });
                    match shown {
                        Some(card::NewTime::Open { start: s, end: e }) => start + (e - s),
                        _ => bail!("the card offers no new time; give --end"),
                    }
                }
            };
            let engine = Outgoing::start(jolta)?;
            let request = respond::Request {
                subject: respond::Subject::Message(jolta_core::MessageId(message)),
                answer,
                note,
                notify: true,
                occurrence_only: true,
                proposal: Some(rsvp::Proposal { start, end }),
            };
            let t = Instant::now();
            let done = respond::answer(jolta, &request).await?;
            println!("{} ({:.2} s)", done.text, t.elapsed().as_secs_f64());
            match done.mailed_from {
                Some(account) => engine.send(account, t).await,
                None => Ok(()),
            }
        }
        CalCommand::AcceptProposal(a) => crate::cal_edit::accept_proposal(jolta, a).await,
        CalCommand::EmailGuests { event, occurrence, to, send } => {
            let at = crate::cal_edit::occurrence_bounds(jolta, event, occurrence.as_deref())?;
            let mut c = jolta.store.read(|conn| jolta_core::calendar::emailing::prefill(conn, jolta_core::calendar::EventId(event), at, to.as_deref(), tz::here()))?;
            println!("From account {}\nTo: {}\nSubject: {}\n\n{}", c.account.map_or(0, |a| a.0), c.to, c.subject, c.body);
            match send {
                Some(text) => {
                    c.body = format!("{text}\n\n{}", c.body);
                    crate::send_compose(jolta, &c).await
                }
                None => Ok(()),
            }
        }
        CalCommand::DeclineProposal { message } => {
            let engine = Outgoing::start(jolta)?;
            let t = Instant::now();
            let done = proposal::decline(jolta, jolta_core::MessageId(message)).await?;
            println!("{} ({:.2} s)", done.text, t.elapsed().as_secs_f64());
            match done.mailed_from {
                Some(account) => engine.send(account, t).await,
                None => Ok(()),
            }
        }
        CalCommand::Remove { message } => {
            let t = Instant::now();
            let done = remove::remove(jolta, jolta_core::MessageId(message)).await?;
            println!("{} ({:.2} s)", done.text, t.elapsed().as_secs_f64());
            Ok(())
        }
        CalCommand::Add { message, calendar, list } => {
            if list {
                for c in add::menu(jolta, jolta_core::MessageId(message)).await? {
                    println!("  {:>4}  {} {}  ({})", c.calendar.0, if c.default { "*" } else { " " }, c.name, c.account);
                }
                return Ok(());
            }
            let t = Instant::now();
            let done = add::add(jolta, jolta_core::MessageId(message), calendar.map(CalendarId)).await?;
            println!("{} ({:.2} s)", done.text, t.elapsed().as_secs_f64());
            Ok(())
        }
        CalCommand::Update { message } => {
            let t = Instant::now();
            let done = add::update(jolta, jolta_core::MessageId(message)).await?;
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
            rsvp::answer(jolta, &Rsvp { event, answer, note, notify: !no_reply, scope, proposal: None }).await?;
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

/// The title cut to fit `width` with the kind after it: "Holiday [out of office]".
fn titled(o: &OccurrenceRow, width: usize) -> String {
    let tag = match &o.kind {
        EventKind::OutOfOffice { .. } => " [out of office]".to_string(),
        EventKind::Focus { .. } => " [focus]".into(),
        EventKind::WorkingLocation(place) => format!(" [working: {}]", place.label()),
        EventKind::Default | EventKind::Birthday => String::new(),
    };
    let room = width.saturating_sub(tag.chars().count()).max(8);
    truncate(&title_of(o), room) + &tag
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

/// The engine, started to send what a calendar command queues.
struct Outgoing {
    engine: std::sync::Arc<jolta_core::engine::Engine>,
    events: tokio::sync::mpsc::UnboundedReceiver<jolta_core::engine::Event>,
}

impl Outgoing {
    fn start(jolta: &Jolta) -> Result<Outgoing> {
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let sink: jolta_core::engine::Sink = std::sync::Arc::new(move |e| {
            let _ = tx.send(e);
        });
        Ok(Outgoing { engine: jolta_core::engine::Engine::start(jolta.clone(), sink, tokio::runtime::Handle::current())?, events })
    }

    /// Sends the account's outbox and waits until a mail has left.
    async fn send(mut self, account: jolta_core::AccountId, t: Instant) -> Result<()> {
        self.engine.wake(account);
        let wait = tokio::time::timeout(std::time::Duration::from_secs(120), async {
            while let Some(e) = self.events.recv().await {
                match e {
                    jolta_core::engine::Event::Sent { subject, .. } => return Ok(subject),
                    jolta_core::engine::Event::SendFailed(m) => bail!("the mail failed: {m}"),
                    _ => {}
                }
            }
            bail!("the engine stopped")
        });
        let subject = wait.await.context("the mail didn't leave within two minutes")??;
        println!("sent {subject:?} after {:.1} s", t.elapsed().as_secs_f64());
        // Lets the engine file the Sent copy.
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        Ok(())
    }
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
            if let Some(p) = &e.proposal {
                println!("  proposed by {}, currently {}", p.proposer, p.now.as_deref().unwrap_or("unknown"));
                let offered: Vec<&str> = [(p.accepting.is_some(), "Accept proposal"), (p.declining.is_some(), "Decline proposal")].into_iter().filter(|(on, _)| *on).map(|(_, w)| w).collect();
                println!("  {}", if p.declined { "declined".to_string() } else if p.accepted { "accepted".to_string() } else if offered.is_empty() { "no Accept or Decline proposal".to_string() } else { format!("{} offered", offered.join(" and ")) });
            }
            line("repeat", &e.changes.repeat, &e.repeat);
            line("place", &e.changes.location, &e.location);
            line("link", &e.changes.join, &e.join.as_ref().map(|j| j.provider.to_string()));
            for n in &e.notes {
                println!("  note: {}", n.text);
            }
            let answer = e.answer.map_or("none", card::answer_word);
            println!("  answer: {answer}{}", if e.answering.is_some() { ", Accept, Maybe and Decline offered" } else { "" });
            match e.answering.as_ref().and_then(|a| a.new_time) {
                Some(card::NewTime::Open { .. }) => println!("  Propose new time offered"),
                Some(card::NewTime::Refused) => println!("  the organizer takes no new time proposals"),
                None => {}
            }
            if let Some(a) = &e.availability {
                println!("  {}", a.text);
            }
            if let Some(r) = &e.removing {
                println!("  Remove from calendar offered{}", r.occurrence.as_ref().map(|(_, d)| format!(" for {d}")).unwrap_or_default());
            }
            if e.adding.is_some() {
                println!("  Add to calendar offered");
            }
            if let Some(u) = &e.updating {
                println!("  Update in your calendar offered ({})", u.calendar);
            }
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum DeclineArg {
    New,
    All,
    None,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum OutsideArg {
    All,
    Contacts,
    None,
}

/// Sets out of office for one account, as the sheet does, and sends the
/// event's op at once.
async fn set_away(jolta: &Jolta, name: &str, from: &str, until: &str, message: String, decline: DeclineArg, outside: OutsideArg) -> Result<()> {
    use jolta_core::calendar::away::plan::{self, Absence, Request};
    let here = tz::here();
    let a = jolta.store.read(|c| store::find_account(c, name))?;
    let day = |t: &str| NaiveDate::parse_from_str(t.trim(), "%Y-%m-%d");
    let moment = |t: &str| -> Result<DateTime<Utc>> {
        let wall = chrono::NaiveDateTime::parse_from_str(t.trim(), "%Y-%m-%d %H:%M").with_context(|| format!("{t:?} is not a day like 2026-11-11 or a time like \"2026-11-11 14:00\""))?;
        Ok(tz::instant(wall, here))
    };
    let absence = match (day(from), day(until)) {
        (Ok(first), Ok(last)) => Absence::Days { first, last },
        _ => Absence::Times { from: moment(from)?, until: moment(until)? },
    };
    let request = Request {
        accounts: vec![a.id],
        absence,
        message,
        audience: match outside {
            OutsideArg::All => away::Audience::All,
            OutsideArg::Contacts => away::Audience::Contacts,
            OutsideArg::None => away::Audience::Organization,
        },
        declines: match decline {
            DeclineArg::New => jolta_core::calendar::AutoDecline::New,
            DeclineArg::All => jolta_core::calendar::AutoDecline::All,
            DeclineArg::None => jolta_core::calendar::AutoDecline::None,
        },
    };
    if let Some(problem) = request.problem(Utc::now(), here) {
        bail!("{problem}");
    }
    let targets = jolta.store.read(plan::targets)?;
    let steps: Vec<plan::Step> = plan::plan(&targets, &request, here).into_iter().filter(|s| s.account == a.id).collect();
    for s in &steps {
        println!("{}: {}", a.address, s.line);
        if let Some((title, url)) = &s.link {
            println!("  {title}: {url}");
        }
    }
    let (applied, _) = plan::apply(jolta, &steps, &Horizon::around(Utc::now(), here), here).await;
    for done in applied {
        done.result?;
    }
    send_calendar_ops(jolta).await
}

/// Adds focus time as the sheet does, says what the calendar gets, and
/// sends it at once.
async fn add_focus(jolta: &Jolta, calendar: i64, start: &str, minutes: i64, decline: DeclineArg, dnd: bool) -> Result<()> {
    use jolta_core::calendar::{AutoDecline, CalendarId, focus};
    let here = tz::here();
    let wall = chrono::NaiveDateTime::parse_from_str(start.trim(), "%Y-%m-%d %H:%M").with_context(|| format!("{start:?} is not a time like \"2026-11-18 14:00\""))?;
    let from = tz::instant(wall, here);
    let declines = match decline {
        DeclineArg::New => AutoDecline::New,
        DeclineArg::All => AutoDecline::All,
        DeclineArg::None => AutoDecline::None,
    };
    let request = focus::Request { from, until: from + chrono::Duration::minutes(minutes), declines, do_not_disturb: dnd };
    let now = Utc::now();
    let horizon = Horizon::around(now, here);
    let (c, holds) = jolta.store.read(|conn| {
        let c = store::calendar(conn, CalendarId(calendar))?;
        let holds = focus::holds_in(conn, &c, now)?;
        Ok((c, holds))
    })?;
    println!("{}: {}", c.name, focus::line(holds, &request, &c));
    jolta.store.write(|tx| focus::add(tx, &c, &request, now, &horizon))?;
    send_calendar_ops(jolta).await
}

/// Sends the calendar ops due now, as the app's worker would.
pub(crate) async fn send_calendar_ops(jolta: &Jolta) -> Result<()> {
    for settled in jolta_core::calendar::ops::run_due(jolta, Horizon::around(Utc::now(), tz::here())).await? {
        if let Some(f) = settled.failure {
            println!("{}", f.message);
        }
        if let Some(notice) = settled.notice {
            println!("{notice}");
        }
    }
    Ok(())
}
