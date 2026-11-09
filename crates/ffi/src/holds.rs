//! Holds for the app (C4 U10): Hold these times in Insert my availability,
//! the line on a conversation, Book, Release all and Undo. Times cross as
//! unix seconds.

use chrono::{DateTime, Utc};
use jolta_core::calendar::holds::act::{self, Notice};
use jolta_core::calendar::holds::{HoldSetId, SlotState};
use jolta_core::calendar::ops::CalendarOpId;
use jolta_core::calendar::series::Horizon;
use jolta_core::calendar::{draft, tz};
use jolta_core::{AccountId, ThreadId, store};

use crate::calendar::TimeSlot;
use crate::editor::EventDraft;
use crate::{Core, CoreError, Result};

/// Whether Hold these times can hold an account's times, and where.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct HoldChoice {
    /// The calendar the holds go to.
    pub calendar: Option<String>,
    /// Why times can't be held, shown under the switch.
    pub reason: Option<String>,
}

/// A toast about holds: Undo takes back the releases in `undo`, Release
/// releases the set.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct HoldNotice {
    pub set_id: String,
    pub text: String,
    pub undo: Vec<i64>,
    pub release: bool,
}

impl From<Notice> for HoldNotice {
    fn from(n: Notice) -> Self {
        HoldNotice { set_id: n.set.to_string(), text: n.text, undo: n.undo.iter().map(|o| o.0).collect(), release: n.release }
    }
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldState {
    Held,
    Booked,
    /// No longer free: something else is there now.
    Taken,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct HoldSlotRow {
    pub start: i64,
    pub end: i64,
    /// "Tue 10 Nov 16:00".
    pub label: String,
    pub state: HoldState,
}

/// The line above a conversation's newest message.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct HoldLine {
    pub set_id: String,
    /// "Holding 3 times for this conversation · Ann replied".
    pub title: String,
    /// "Ann booked Wed 11 Nov 15:00. Release the holds?"
    pub ask: Option<String>,
    pub slots: Vec<HoldSlotRow>,
}

fn set_id(text: &str) -> Result<HoldSetId> {
    HoldSetId::parse(text).ok_or_else(|| CoreError::Query { message: format!("{text} is no hold set") })
}

fn at(s: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp(s, 0).ok_or_else(|| CoreError::Failed { message: format!("{s} isn't a time") })
}

/// The message's text for the times, and each time with its line.
fn picked(slots: &[TimeSlot], zone: Option<String>) -> Result<(String, Vec<(DateTime<Utc>, DateTime<Utc>, String)>)> {
    let here = tz::here();
    let theirs = match zone {
        Some(name) => tz::zone(&name).ok_or_else(|| CoreError::Query { message: format!("{name} is no time zone") })?,
        None => here,
    };
    let times = slots.iter().map(|s| Ok((at(s.start)?, at(s.end)?))).collect::<Result<Vec<_>>>()?;
    let (text, lines) = act::text(&times, theirs, here);
    Ok((text, times.into_iter().zip(lines).map(|((s, e), l)| (s, e, l)).collect()))
}

#[uniffi::export]
impl Core {
    /// Where Hold these times would hold the account's times, or why it can't.
    pub fn hold_choice(&self, account_id: i64) -> Result<HoldChoice> {
        Ok(match self.jolta.store.read(|c| act::calendar_for(c, AccountId(account_id)))? {
            Ok(calendar) => HoldChoice { calendar: Some(calendar.name), reason: None },
            Err(reason) => HoldChoice { calendar: None, reason: Some(reason) },
        })
    }

    /// The text Insert my availability puts in the message for picked
    /// times: one line per time on `zone`'s clock (an IANA name), else this
    /// Mac's.
    pub fn hold_text(&self, slots: Vec<TimeSlot>, zone: Option<String>) -> Result<String> {
        Ok(picked(&slots, zone)?.0)
    }

    /// Holds the picked times once the draft is sent; its body has the
    /// lines `hold_text` gave. Replaces what the draft held before.
    pub fn hold_times(&self, draft_id: i64, account_id: i64, slots: Vec<TimeSlot>, zone: Option<String>) -> Result<()> {
        let (_, picked) = picked(&slots, zone)?;
        Ok(self.jolta.store.write(|tx| act::pending(tx, draft_id, AccountId(account_id), &picked, Utc::now()).map(|_| ()))?)
    }

    /// The holds line for a conversation whose sent mail still holds times.
    pub fn holds_for_thread(&self, thread_id: i64) -> Result<Option<HoldLine>> {
        let line = self.jolta.store.read(|c| act::line(c, &self.jolta.blobs, ThreadId(thread_id), Utc::now(), tz::here()))?;
        Ok(line.map(|l| HoldLine {
            set_id: l.set.to_string(),
            title: l.title,
            ask: l.ask,
            slots: l
                .slots
                .into_iter()
                .map(|s| HoldSlotRow {
                    start: s.start.timestamp(),
                    end: s.end.timestamp(),
                    label: s.label,
                    state: match s.state {
                        SlotState::Booked => HoldState::Booked,
                        SlotState::Taken => HoldState::Taken,
                        _ => HoldState::Held,
                    },
                })
                .collect(),
        }))
    }

    /// Book: the editor's draft for a meeting at the held time `start`
    /// with the recipients, titled after the conversation.
    pub fn book_hold(&self, set_id: String, start: i64) -> Result<EventDraft> {
        let (id, start, here) = (self::set_id(&set_id)?, at(start)?, tz::here());
        let d = self.jolta.store.read(|c| draft::draft_from_prefill(c, &act::book(c, &id, start, here)?, Utc::now(), here))?;
        Ok(d.into())
    }

    /// Release all: deletes the set's holds after the undo window.
    pub fn release_holds(&self, set_id: String) -> Result<HoldNotice> {
        let id = self::set_id(&set_id)?;
        let now = Utc::now();
        let (calendar, notice) = self.jolta.store.write(|tx| Ok((store::hold_set(tx, &id)?.calendar, act::release(tx, &id, now, &Horizon::around(now, tz::here()))?)))?;
        self.edited(calendar);
        Ok(notice.into())
    }

    /// Takes back the releases of a toast's Undo; returns the toast's text.
    pub fn undo_hold_release(&self, set_id: String, ops: Vec<i64>) -> Result<String> {
        let id = self::set_id(&set_id)?;
        let now = Utc::now();
        let ops: Vec<CalendarOpId> = ops.into_iter().map(CalendarOpId).collect();
        let (calendar, text) = self.jolta.store.write(|tx| Ok((store::hold_set(tx, &id)?.calendar, act::undo(tx, &id, &ops, now, &Horizon::around(now, tz::here()))?)))?;
        self.edited(calendar);
        Ok(text)
    }
}
