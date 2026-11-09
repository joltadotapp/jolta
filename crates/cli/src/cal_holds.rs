//! `jolta cal holds`: hold sets with their slots, the hold events sync
//! found for sets this Mac doesn't know, and with `--check` what `decide`
//! makes of each open set now.

use anyhow::Result;
use chrono::{DateTime, Utc};
use jolta_core::Jolta;
use jolta_core::calendar::holds::{self, Booking, Decision, Mark, Notice, Outcome, Release};
use jolta_core::calendar::{Span, tz};
use jolta_core::store;

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
