//! `jolta cal holds`: hold sets with their slots, the hold events sync
//! found for sets this Mac doesn't know, and with `--check` what `decide`
//! makes of each open set now; `--release` and `--undo` act as the app's
//! Release all and Undo. `jolta cal hold` and `jolta send --hold` make
//! holds.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use jolta_core::Jolta;
use jolta_core::calendar::holds::act::{self, Notice as Said};
use jolta_core::calendar::holds::{self, Booking, Decision, HoldSetId, Mark, Notice, Outcome, Release};
use jolta_core::calendar::ops::CalendarOpId;
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{Span, tz};
use jolta_core::compose::{self, Compose};
use jolta_core::store;

/// The times `jolta send --hold` offers.
pub struct Offered {
    pub times: Vec<String>,
    pub minutes: i64,
}

fn times(texts: &[String], minutes: i64) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
    let here = tz::here();
    texts
        .iter()
        .map(|t| {
            let wall = chrono::NaiveDateTime::parse_from_str(t.trim(), "%Y-%m-%d %H:%M").with_context(|| format!("{t:?} is not a time like \"2026-11-10 16:00\""))?;
            let start = tz::instant(wall, here);
            Ok((start, start + Duration::minutes(minutes)))
        })
        .collect()
}

/// Puts the offered times in the message as Insert my availability does,
/// one line each, and makes its draft's pending set.
pub fn offer(jolta: &Jolta, c: &mut Compose, offered: &Offered) -> Result<()> {
    let account = c.account.context("the message has no account")?;
    let times = times(&offered.times, offered.minutes)?;
    let here = tz::here();
    let (text, lines) = act::text(&times, here, here);
    c.body = format!("{}\n\n{text}\n", c.body.trim_end());
    let picked: Vec<_> = times.into_iter().zip(lines).map(|((s, e), l)| (s, e, l)).collect();
    let draft = jolta.store.write(|tx| {
        let draft = store::save_draft(tx, c)?;
        act::pending(tx, draft, account, &picked, Utc::now())?;
        Ok(draft)
    })?;
    c.draft = Some(draft);
    println!("draft {draft} offers {} times; they are held once it left", picked.len());
    Ok(())
}

/// `jolta cal hold`: as if a message to `to` offering these times had just left.
pub fn hold(jolta: &Jolta, account: &str, to: &str, subject: &str, slots: &[String], minutes: i64, message_id: Option<String>) -> Result<()> {
    let a = jolta.store.read(|c| store::find_account(c, account))?;
    let mut c = Compose { account: Some(a.id), to: to.into(), subject: subject.into(), ..Compose::default() };
    offer(jolta, &mut c, &Offered { times: slots.to_vec(), minutes })?;
    let built = compose::build(&a, &c, true)?;
    let now = Utc::now();
    let item = store::OutboxItem {
        id: 0,
        account: a.id,
        draft: c.draft,
        blob: String::new(),
        message_id: message_id.unwrap_or(built.message_id),
        sender: built.from,
        recipients: built.recipients,
        replying_to: None,
        attempts: 0,
    };
    let made = jolta.store.write(|tx| {
        let made = act::sent(tx, &item, &built.raw, now, &Horizon::around(now, tz::here()))?;
        if let Some(d) = c.draft {
            store::discard_draft(tx, d)?;
        }
        Ok(made)
    })?;
    match made {
        Some(m) => println!("{}", Said::made(&m).text),
        None => println!("Nothing to hold."),
    }
    Ok(())
}

fn set_id(text: &str) -> Result<HoldSetId> {
    HoldSetId::parse(text).with_context(|| format!("{text} is no hold set id"))
}

pub fn release(jolta: &Jolta, set: &str) -> Result<()> {
    let (id, now) = (set_id(set)?, Utc::now());
    let notice = jolta.store.write(|tx| act::release(tx, &id, now, &Horizon::around(now, tz::here())))?;
    let ops: Vec<String> = notice.undo.iter().map(|o| o.0.to_string()).collect();
    println!("{}  (undo: jolta cal holds --undo {id} --op {})", notice.text, ops.join(" --op "));
    Ok(())
}

pub fn undo(jolta: &Jolta, set: &str, ops: &[i64]) -> Result<()> {
    let (id, now) = (set_id(set)?, Utc::now());
    let ops: Vec<CalendarOpId> = ops.iter().copied().map(CalendarOpId).collect();
    println!("{}", jolta.store.write(|tx| act::undo(tx, &id, &ops, now, &Horizon::around(now, tz::here())))?);
    Ok(())
}

pub fn holds(jolta: &Jolta, check: bool) -> Result<()> {
    let (here, now) = (tz::here(), Utc::now());
    let adopted = jolta.store.write(|tx| holds::adopt(tx, now, here))?;
    let sets = jolta.store.read(store::hold_sets)?;
    let at = |t: DateTime<Utc>| t.with_timezone(&here).format("%a %-d %b %H:%M").to_string();
    let times = |s: DateTime<Utc>, e: DateTime<Utc>| format!("{} to {}", at(s), e.with_timezone(&here).format("%H:%M"));
    if sets.is_empty() {
        println!("No hold sets.");
    }
    for set in &sets {
        let sent = set.sent.map(|s| format!("sent {}", at(s))).unwrap_or_else(|| format!("draft {}", set.draft.unwrap_or_default()));
        let found = if adopted.contains(&set.id) { ", built here from its sent mail" } else { "" };
        println!("Set {}  \"{}\" to {}, account {}, calendar {}, {sent}{found}", set.id, set.subject, set.recipients.join(", "), set.account, set.calendar);
        for s in &set.slots {
            println!("  {:<9} {}  {}  {}", s.state.as_str(), times(s.start, s.end), s.remote_id.as_deref().unwrap_or("-"), s.line);
        }
        let thread = set.message_id.as_deref().map(|m| jolta.store.read(|c| store::sent_mail(c, m))).transpose()?.flatten().map(|m| m.thread);
        if let Some(t) = thread
            && let Some(l) = jolta.store.read(|c| act::line(c, &jolta.blobs, t, now, here))?
            && l.set == set.id
        {
            let ask = l.ask.map(|a| format!("  {a}")).unwrap_or_default();
            println!("  line on conversation {}: {}{ask}", t.0, l.title);
        }
        if !check || !set.is_open() {
            continue;
        }
        let seen = jolta.store.read(|c| holds::seen(c, &jolta.blobs, set, here))?;
        let decisions = holds::decide(set, &seen, now);
        if decisions.is_empty() {
            println!("  check: nothing changed");
        }
        for d in decisions {
            println!("  check: {}", decision(&d, &at));
        }
    }
    let known: Vec<_> = sets.iter().map(|s| s.id.clone()).collect();
    let mut strays = jolta.store.read(|c| store::hold_events(c, None))?;
    strays.retain(|m| m.event.mark.as_ref().and_then(Mark::hold_set).is_some_and(|s| !known.contains(s)));
    if !strays.is_empty() {
        println!("Hold events of sets this Mac doesn't know (no sent mail with their Message-ID is stored here):");
    }
    for m in strays {
        let Some(Mark::Hold { set, message }) = &m.event.mark else { continue };
        let when = match &m.event.span {
            Span::Timed { start, end } => times(start.instant(here), end.instant(here)),
            Span::AllDay { start, end } => format!("{start} to {end}, all day"),
        };
        println!("  set {set}  calendar {}  {when}  {}  {:?}  message {}", m.calendar, m.remote_id, m.event.title, message.as_deref().unwrap_or("none"));
    }
    Ok(())
}

fn decision(d: &Decision, at: &impl Fn(DateTime<Utc>) -> String) -> String {
    match d {
        Decision::Moved { slot, start, .. } => format!("moved     {} is held at {} now", at(*slot), at(*start)),
        Decision::Slot { slot, outcome } => {
            let why = match outcome {
                Outcome::Booked(Booking::Event { by }) => format!("by {by}, a meeting there"),
                Outcome::Booked(Booking::Invitation { by }) => format!("by {by}, an invitation in mail"),
                Outcome::Booked(Booking::Meeting) => "the hold has guests now".into(),
                Outcome::Taken => "no longer free".into(),
                Outcome::Released(Release::OtherBooked) => "another time was booked".into(),
                Outcome::Released(Release::Passed) => "its time started unbooked".into(),
                Outcome::Gone => "deleted elsewhere".into(),
            };
            let delete = if outcome.deletes_hold() { "; delete its hold" } else { "" };
            format!("{:<9} {}  {why}{delete}", outcome.state().as_str(), at(*slot))
        }
        Decision::Notice(Notice::BookedElsewhere { by, start, .. }) => format!("ask       {by} booked {}. Release the holds?", at(*start)),
        Decision::Notice(Notice::Replied { by }) => format!("say       {by} replied"),
    }
}
