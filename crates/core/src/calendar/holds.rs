//! Holds (C4 U9): tentative events that keep the times a mail offers free
//! until someone books one.
//!
//! Each hold event carries a hidden marker with its set's id, which every
//! source reads back during sync (`EventData::mark`), so any Mac finds the
//! set's events and the user's other clients leave them alone. `decide` is
//! pure: from what a sync round left in the store and the mail, it says
//! what happened to each held time. U10 creates and deletes the events.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::CalendarId;
use crate::AccountId;
use crate::model::text_enum;

pub(crate) const GOOGLE_HOLD: &str = "joltaHold";
pub(crate) const GOOGLE_MESSAGE: &str = "joltaHoldMessage";
/// Graph's ids for the two properties, under a GUID of Jolta's own.
pub(crate) const GRAPH_HOLD: &str = "String {9d3b2d55-5a0e-4a6e-9b4e-6a6f6c746130} Name JoltaHold";
pub(crate) const GRAPH_MESSAGE: &str = "String {9d3b2d55-5a0e-4a6e-9b4e-6a6f6c746130} Name JoltaHoldMessage";
pub(crate) const CALDAV_HOLD: &str = "X-JOLTA-HOLD";
pub(crate) const CALDAV_MESSAGE: &str = "X-JOLTA-HOLD-MESSAGE";
/// The marker's value on an out-of-office event Jolta made (C4 U4).
const AWAY: &str = "away";

/// A hold set's id: a UUID, lowercase, also written on the server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HoldSetId(String);

impl HoldSetId {
    pub fn parse(text: &str) -> Option<HoldSetId> {
        let text = text.trim().to_ascii_lowercase();
        let uuid = text.len() == 36
            && text.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() });
        uuid.then_some(HoldSetId(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HoldSetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Jolta's hidden marker on an event it made for a feature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "is", rename_all = "lowercase")]
pub enum Mark {
    /// One of a hold set's events, with the Message-ID of the mail that
    /// offers its times once that mail left.
    Hold {
        set: HoldSetId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// An out-of-office event (C4 U4).
    Away,
}

impl Mark {
    /// The marker from the two values a source keeps. A value that is
    /// neither a set id nor `away` is no marker of Jolta's.
    fn read(hold: Option<&str>, message: Option<&str>) -> Option<Mark> {
        match hold?.trim() {
            AWAY => Some(Mark::Away),
            value => HoldSetId::parse(value).map(|set| Mark::Hold {
                set,
                message: message.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string),
            }),
        }
    }

    /// From Google's `extendedProperties.private`.
    pub(crate) fn from_google(private: &HashMap<String, String>) -> Option<Mark> {
        Mark::read(private.get(GOOGLE_HOLD).map(String::as_str), private.get(GOOGLE_MESSAGE).map(String::as_str))
    }

    /// From Graph's `singleValueExtendedProperties`, id and value. Graph
    /// may give the id in another case than it was written in.
    pub(crate) fn from_graph<'a>(properties: impl Iterator<Item = (&'a str, &'a str)> + Clone) -> Option<Mark> {
        let value = |id: &str| properties.clone().find(|(i, _)| i.eq_ignore_ascii_case(id)).map(|(_, v)| v);
        Mark::read(value(GRAPH_HOLD), value(GRAPH_MESSAGE))
    }

    /// From the values of a VEVENT's `X-JOLTA-HOLD` and `X-JOLTA-HOLD-MESSAGE`.
    pub(crate) fn from_lines(hold: Option<&str>, message: Option<&str>) -> Option<Mark> {
        Mark::read(hold, message)
    }

    fn values(&self) -> (&str, Option<&str>) {
        match self {
            Mark::Hold { set, message } => (set.as_str(), message.as_deref()),
            Mark::Away => (AWAY, None),
        }
    }

    /// Google's `extendedProperties.private`.
    pub fn google_private(&self) -> Value {
        let (hold, message) = self.values();
        let mut private = json!({ GOOGLE_HOLD: hold });
        if let Some(m) = message {
            private[GOOGLE_MESSAGE] = m.into();
        }
        private
    }

    /// Graph's `singleValueExtendedProperties`.
    pub fn graph_properties(&self) -> Value {
        let (hold, message) = self.values();
        let mut properties = vec![json!({ "id": GRAPH_HOLD, "value": hold })];
        properties.extend(message.map(|m| json!({ "id": GRAPH_MESSAGE, "value": m })));
        Value::Array(properties)
    }

    /// The VEVENT's property lines, values escaped as TEXT.
    pub fn caldav_lines(&self) -> Vec<String> {
        let (hold, message) = self.values();
        let escape = |v: &str| v.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n");
        let mut lines = vec![format!("{CALDAV_HOLD}:{}", escape(hold))];
        lines.extend(message.map(|m| format!("{CALDAV_MESSAGE}:{}", escape(m))));
        lines
    }

    pub fn hold_set(&self) -> Option<&HoldSetId> {
        match self {
            Mark::Hold { set, .. } => Some(set),
            Mark::Away => None,
        }
    }
}

/// `$expand` for Graph's event requests, so they return the two markers.
pub(crate) fn graph_expand() -> String {
    let filter = format!("id eq '{GRAPH_HOLD}' or id eq '{GRAPH_MESSAGE}'");
    let encoded: String = filter
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'\'' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("$expand=singleValueExtendedProperties($filter={encoded})")
}

text_enum!(SlotState {
    Pending => "pending",
    Held => "held",
    Booked => "booked",
    Taken => "taken",
    Released => "released",
    Gone => "gone",
});

/// One time a mail offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldSlot {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// The line of the mail that offers it.
    pub line: String,
    pub state: SlotState,
    /// The hold event's remote id, once it was made.
    pub remote_id: Option<String>,
}

/// The holds for one mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldSet {
    pub id: HoldSetId,
    /// The account that sends the mail.
    pub account: AccountId,
    /// The sent mail's Message-ID, once it left.
    pub message_id: Option<String>,
    /// The draft, while the mail isn't sent.
    pub draft: Option<i64>,
    pub recipients: Vec<String>,
    pub subject: String,
    pub calendar: CalendarId,
    /// When the mail left.
    pub sent: Option<DateTime<Utc>>,
    pub slots: Vec<HeldSlot>,
}

impl HoldSet {
    /// The set ends when no slot is held.
    pub fn is_open(&self) -> bool {
        self.slots.iter().any(|s| s.state == SlotState::Held)
    }
}

/// What Jolta knows after a sync round, as `decide` reads it.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    /// The sending account's address and its aliases.
    pub me: Vec<String>,
    /// Occurrences that block the user's time, from every shown calendar
    /// that counts as busy, on the offered days and the 7 after them. An
    /// occurrence of a series counts on its own.
    pub busy: Vec<Busy>,
    /// Requests that came to the sending account, one per occurrence.
    pub invitations: Vec<Invited>,
    /// The conversation's messages from other people.
    pub replies: Vec<Reply>,
    /// The set's own hold events as stored.
    pub holds: Vec<HoldEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// The organizer and the guests, without the attendee the source marks
    /// as the user.
    pub people: Vec<String>,
    /// Any set's hold event, which books nothing and takes nothing.
    pub hold: bool,
    /// When the provider says the event was made.
    pub created: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invited {
    /// The message's sender.
    pub from: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub received: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub from: String,
    pub date: DateTime<Utc>,
}

/// A hold event of the set, as the last sync stored it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldEvent {
    pub remote_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Someone besides the user is on it.
    pub guests: bool,
}

/// What happened, for one slot or for the set. Slots are named by their
/// start as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Rule 9: the user moved the hold in another client; it stays held there.
    Moved { slot: DateTime<Utc>, start: DateTime<Utc>, end: DateTime<Utc> },
    /// A held slot leaves `Held`.
    Slot { slot: DateTime<Utc>, outcome: Outcome },
    /// Something the conversation line says; nothing changes by itself.
    Notice(Notice),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Booked(Booking),
    /// Rule 3: something without a recipient is there now.
    Taken,
    Released(Release),
    /// Rule 8: the hold was deleted elsewhere.
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Booking {
    /// Rule 1: an event with a recipient on it.
    Event { by: String },
    /// Rule 2: a recipient's invitation in mail.
    Invitation { by: String },
    /// Rule 5: the user added people to the hold.
    Meeting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Another slot was booked.
    OtherBooked,
    /// Rule 7: its start passed unbooked.
    Passed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Rule 4: "Ann booked Wed 15:00. Release the holds?"
    BookedElsewhere { by: String, start: DateTime<Utc>, end: DateTime<Utc> },
    /// Rule 6: "Ann replied".
    Replied { by: String },
}

impl Outcome {
    pub fn state(&self) -> SlotState {
        match self {
            Outcome::Booked(_) => SlotState::Booked,
            Outcome::Taken => SlotState::Taken,
            Outcome::Released(_) => SlotState::Released,
            Outcome::Gone => SlotState::Gone,
        }
    }

    /// Whether the hold event is to be deleted: not when it is gone
    /// already, or is the meeting now.
    pub fn deletes_hold(&self) -> bool {
        !matches!(self, Outcome::Gone | Outcome::Booked(Booking::Meeting))
    }
}

fn overlaps(a: (DateTime<Utc>, DateTime<Utc>), b: (DateTime<Utc>, DateTime<Utc>)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// What happened to `set` by `now`, from what a sync round left in `seen`.
/// Only held slots change, so a decision applied once isn't made again.
pub fn decide(set: &HoldSet, seen: &Seen, now: DateTime<Utc>) -> Vec<Decision> {
    let Some(sent) = set.sent else { return Vec::new() };
    let is_me = |a: &str| seen.me.iter().any(|m| m.eq_ignore_ascii_case(a));
    let recipient = |a: &str| !is_me(a) && set.recipients.iter().any(|r| r.eq_ignore_ascii_case(a));
    let by = |people: &[String]| people.iter().find(|p| recipient(p)).cloned();
    let blocking = || seen.busy.iter().filter(|b| !b.hold);

    let mut moves = Vec::new();
    let mut outcomes: Vec<(DateTime<Utc>, Option<Outcome>, (DateTime<Utc>, DateTime<Utc>))> = Vec::new();
    for slot in set.slots.iter().filter(|s| s.state == SlotState::Held) {
        let event = slot.remote_id.as_deref().map(|id| seen.holds.iter().find(|h| h.remote_id == id));
        let Some(event) = event else {
            outcomes.push((slot.start, None, (slot.start, slot.end)));
            continue;
        };
        let Some(event) = event else {
            outcomes.push((slot.start, Some(Outcome::Gone), (slot.start, slot.end)));
            continue;
        };
        let at = (event.start, event.end);
        if at != (slot.start, slot.end) {
            moves.push(Decision::Moved { slot: slot.start, start: event.start, end: event.end });
        }
        let booking = if event.guests {
            Some(Booking::Meeting)
        } else if let Some(by) = blocking().filter(|b| overlaps((b.start, b.end), at)).find_map(|b| by(&b.people)) {
            Some(Booking::Event { by })
        } else {
            seen.invitations
                .iter()
                .find(|i| i.received >= sent && recipient(&i.from) && overlaps((i.start, i.end), at))
                .map(|i| Booking::Invitation { by: i.from.clone() })
        };
        outcomes.push((slot.start, booking.map(Outcome::Booked), at));
    }

    let booked = set.slots.iter().any(|s| s.state == SlotState::Booked) || outcomes.iter().any(|(_, o, _)| matches!(o, Some(Outcome::Booked(_))));
    for (_, outcome, at) in outcomes.iter_mut() {
        if outcome.is_some() {
            continue;
        }
        *outcome = if booked {
            Some(Outcome::Released(Release::OtherBooked))
        } else if blocking().any(|b| overlaps((b.start, b.end), *at)) {
            Some(Outcome::Taken)
        } else if at.0 <= now {
            Some(Outcome::Released(Release::Passed))
        } else {
            None
        };
    }

    let still_held: Vec<(DateTime<Utc>, DateTime<Utc>)> = outcomes.iter().filter(|(_, o, _)| o.is_none()).map(|(_, _, at)| *at).collect();
    let mut notices = Vec::new();
    if !still_held.is_empty() {
        for b in blocking().filter(|b| b.created.is_some_and(|c| c > sent)) {
            let Some(by) = by(&b.people) else { continue };
            let notice = Notice::BookedElsewhere { by, start: b.start, end: b.end };
            if !still_held.iter().any(|at| overlaps((b.start, b.end), *at)) && !notices.contains(&notice) {
                notices.push(notice);
            }
        }
        let mut replied: Vec<&Reply> = seen.replies.iter().filter(|r| r.date > sent && recipient(&r.from)).collect();
        replied.sort_by_key(|r| r.date);
        for r in replied {
            let notice = Notice::Replied { by: r.from.clone() };
            if !notices.iter().any(|n| matches!(n, Notice::Replied { by } if by.eq_ignore_ascii_case(&r.from))) {
                notices.push(notice);
            }
        }
    }

    moves
        .into_iter()
        .chain(outcomes.into_iter().filter_map(|(slot, o, _)| o.map(|outcome| Decision::Slot { slot, outcome })))
        .chain(notices.into_iter().map(Decision::Notice))
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;

    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc()
    }

    const ANN: &str = "ann@example.com";
    const ME: &str = "me@example.com";
    /// Tue, Wed and Thu at the times the mail offers.
    const SLOTS: [(&str, &str, &str); 3] = [("h1", "2026-11-10 15:00", "2026-11-10 15:30"), ("h2", "2026-11-11 10:00", "2026-11-11 10:30"), ("h3", "2026-11-12 14:00", "2026-11-12 14:30")];

    fn set() -> HoldSet {
        HoldSet {
            id: HoldSetId::parse("5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10").unwrap(),
            account: AccountId(1),
            message_id: Some("<m1@example.com>".into()),
            draft: None,
            recipients: vec![ANN.into()],
            subject: "Planning".into(),
            calendar: CalendarId(1),
            sent: Some(t("2026-11-09 09:00")),
            slots: SLOTS
                .iter()
                .map(|(id, s, e)| HeldSlot { start: t(s), end: t(e), line: format!("{s} to {e}"), state: SlotState::Held, remote_id: Some(id.to_string()) })
                .collect(),
        }
    }

    fn seen() -> Seen {
        Seen {
            me: vec![ME.into()],
            holds: SLOTS.iter().map(|(id, s, e)| HoldEvent { remote_id: id.to_string(), start: t(s), end: t(e), guests: false }).collect(),
            ..Seen::default()
        }
    }

    fn busy(from: &str, to: &str, people: &[&str]) -> Busy {
        Busy { start: t(from), end: t(to), people: people.iter().map(|p| p.to_string()).collect(), hold: false, created: Some(t("2026-11-09 10:00")) }
    }

    fn now() -> DateTime<Utc> {
        t("2026-11-09 12:00")
    }

    /// Each slot's outcome by its hold's remote id.
    fn outcomes(decisions: &[Decision]) -> Vec<(&'static str, Outcome)> {
        decisions
            .iter()
            .filter_map(|d| match d {
                Decision::Slot { slot, outcome } => SLOTS.iter().find(|(_, s, _)| t(s) == *slot).map(|(id, _, _)| (*id, outcome.clone())),
                _ => None,
            })
            .collect()
    }

    fn notices(decisions: &[Decision]) -> Vec<Notice> {
        decisions.iter().filter_map(|d| if let Decision::Notice(n) = d { Some(n.clone()) } else { None }).collect()
    }

    /// The set after the decisions, as the store applies them.
    fn applied(mut set: HoldSet, decisions: &[Decision]) -> HoldSet {
        for d in decisions {
            match d {
                Decision::Slot { slot, outcome } => set.slots.iter_mut().filter(|s| s.start == *slot).for_each(|s| s.state = outcome.state()),
                Decision::Moved { slot, start, end } => set.slots.iter_mut().filter(|s| s.start == *slot).for_each(|s| (s.start, s.end) = (*start, *end)),
                Decision::Notice(_) => {}
            }
        }
        set
    }

    const OTHERS_RELEASED: Outcome = Outcome::Released(Release::OtherBooked);

    #[test]
    fn rule_1_a_meeting_with_a_recipient_on_a_held_time_books_it_and_releases_the_rest() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-11 10:00", "2026-11-11 10:30", &["bo@example.com", "Ann@Example.COM"]));
        let booked = Outcome::Booked(Booking::Event { by: "Ann@Example.COM".into() });
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h1", OTHERS_RELEASED), ("h2", booked.clone()), ("h3", OTHERS_RELEASED)]);
        assert!(booked.deletes_hold(), "the meeting holds the time now");
    }

    #[test]
    fn rule_2_a_recipients_invitation_in_mail_books_and_a_strangers_is_ignored() {
        let mut seen = seen();
        let invited = |from: &str, received: &str| Invited { from: from.into(), start: t("2026-11-10 15:00"), end: t("2026-11-10 16:00"), received: t(received) };
        seen.invitations = vec![invited("eve@example.com", "2026-11-09 11:00"), invited(ANN, "2026-11-08 11:00")];
        assert_eq!(decide(&set(), &seen, now()), [], "a stranger reusing the subject, and Ann's invitation from before the mail");
        seen.invitations.push(invited(ANN, "2026-11-09 11:00"));
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h1", Outcome::Booked(Booking::Invitation { by: ANN.into() })), ("h2", OTHERS_RELEASED), ("h3", OTHERS_RELEASED)]);
    }

    #[test]
    fn rule_3_something_without_a_recipient_takes_only_its_slot() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-12 13:00", "2026-11-12 14:15", &["bo@example.com"]));
        seen.busy.push(busy("2026-11-11 09:00", "2026-11-11 10:00", &[]));
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h3", Outcome::Taken)], "an event that ends as a slot starts takes nothing");
    }

    #[test]
    fn an_all_day_event_over_a_held_slot_takes_it() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-10 23:00", "2026-11-11 23:00", &[]));
        assert_eq!(outcomes(&decide(&set(), &seen, now())), [("h2", Outcome::Taken)]);
    }

    #[test]
    fn rule_4_a_new_meeting_with_a_recipient_at_another_time_only_asks() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-13 15:00", "2026-11-13 16:00", &[ANN]));
        let before = Busy { created: Some(t("2026-11-02 08:00")), ..busy("2026-11-16 15:00", "2026-11-16 16:00", &[ANN]) };
        let unknown = Busy { created: None, ..busy("2026-11-17 15:00", "2026-11-17 16:00", &[ANN]) };
        seen.busy.extend([before, unknown]);
        let decisions = decide(&set(), &seen, now());
        assert_eq!(outcomes(&decisions), []);
        assert_eq!(notices(&decisions), [Notice::BookedElsewhere { by: ANN.into(), start: t("2026-11-13 15:00"), end: t("2026-11-13 16:00") }]);
    }

    #[test]
    fn rule_5_a_hold_with_guests_is_the_meeting() {
        let mut seen = seen();
        seen.holds[1].guests = true;
        let decisions = decide(&set(), &seen, now());
        assert_eq!(outcomes(&decisions), [("h1", OTHERS_RELEASED), ("h2", Outcome::Booked(Booking::Meeting)), ("h3", OTHERS_RELEASED)]);
        assert!(!Outcome::Booked(Booking::Meeting).deletes_hold(), "the hold is the meeting now");
    }

    #[test]
    fn rule_6_a_reply_from_a_recipient_changes_nothing_and_says_so() {
        let mut seen = seen();
        seen.replies = vec![
            Reply { from: "eve@example.com".into(), date: t("2026-11-09 10:00") },
            Reply { from: ANN.into(), date: t("2026-11-08 10:00") },
            Reply { from: ME.into(), date: t("2026-11-09 10:30") },
        ];
        assert_eq!(decide(&set(), &seen, now()), [], "a stranger, Ann before the mail, the user");
        seen.replies.push(Reply { from: ANN.into(), date: t("2026-11-09 11:00") });
        seen.replies.push(Reply { from: "ANN@example.com".into(), date: t("2026-11-09 11:30") });
        assert_eq!(decide(&set(), &seen, now()), [Decision::Notice(Notice::Replied { by: ANN.into() })]);
    }

    #[test]
    fn rule_7_a_slot_whose_start_passed_is_released_and_the_set_ends_with_the_last() {
        let decisions = decide(&set(), &seen(), t("2026-11-10 15:00"));
        assert_eq!(outcomes(&decisions), [("h1", Outcome::Released(Release::Passed))]);
        let later = applied(set(), &decisions);
        assert!(later.is_open());
        let all = decide(&later, &seen(), t("2026-11-12 14:00"));
        assert_eq!(outcomes(&all), [("h2", Outcome::Released(Release::Passed)), ("h3", Outcome::Released(Release::Passed))]);
        assert!(!applied(later, &all).is_open());
    }

    #[test]
    fn rule_8_a_hold_deleted_elsewhere_is_gone_and_nothing_else_changes() {
        let mut seen = seen();
        seen.holds.remove(1);
        let decisions = decide(&set(), &seen, now());
        assert_eq!(outcomes(&decisions), [("h2", Outcome::Gone)]);
        assert!(!Outcome::Gone.deletes_hold());
    }

    #[test]
    fn rule_9_a_moved_hold_is_held_at_its_new_time() {
        let mut seen = seen();
        (seen.holds[2].start, seen.holds[2].end) = (t("2026-11-13 09:00"), t("2026-11-13 09:30"));
        seen.busy.push(busy("2026-11-12 14:00", "2026-11-12 14:30", &[]));
        let decisions = decide(&set(), &seen, now());
        assert_eq!(decisions, [Decision::Moved { slot: t("2026-11-12 14:00"), start: t("2026-11-13 09:00"), end: t("2026-11-13 09:30") }], "the old time is free to take");
        seen.busy.push(busy("2026-11-13 09:00", "2026-11-13 10:00", &[ANN]));
        let booked = Decision::Slot { slot: t("2026-11-13 09:00"), outcome: Outcome::Booked(Booking::Event { by: ANN.into() }) };
        assert!(decide(&applied(set(), &decisions), &seen, now()).contains(&booked));
    }

    #[test]
    fn the_users_alias_among_the_recipients_books_nothing() {
        let mut set = set();
        set.recipients.push("Me.Alias@example.com".into());
        let mut seen = seen();
        seen.me.push("me.alias@example.com".into());
        seen.busy.push(busy("2026-11-10 15:00", "2026-11-10 15:30", &["me.alias@example.com"]));
        assert_eq!(outcomes(&decide(&set, &seen, now())), [("h1", Outcome::Taken)]);
    }

    #[test]
    fn the_same_booking_seen_in_two_rounds_is_applied_once() {
        let mut seen = seen();
        seen.busy.push(busy("2026-11-11 10:00", "2026-11-11 10:30", &[ANN]));
        let first = decide(&set(), &seen, now());
        assert_eq!(decide(&applied(set(), &first), &seen, now()), []);
        let booked_only: Vec<Decision> = first.into_iter().filter(|d| matches!(d, Decision::Slot { outcome: Outcome::Booked(_), .. })).collect();
        let after_a_failure = applied(set(), &booked_only);
        assert_eq!(outcomes(&decide(&after_a_failure, &seen, now())), [("h1", OTHERS_RELEASED), ("h3", OTHERS_RELEASED)], "releases that failed come again");
    }

    #[test]
    fn holds_take_nothing_and_a_set_not_sent_decides_nothing() {
        let mut seen = seen();
        seen.busy.push(Busy { hold: true, ..busy("2026-11-10 15:00", "2026-11-10 15:30", &[]) });
        assert_eq!(decide(&set(), &seen, now()), [], "another set's hold at the same time");
        seen.busy.push(busy("2026-11-10 15:00", "2026-11-10 15:30", &[ANN]));
        assert_eq!(decide(&HoldSet { sent: None, ..set() }, &seen, now()), []);
    }

    #[test]
    fn markers_carry_the_set_and_the_message() {
        let mark = Mark::Hold { set: set().id, message: Some("<m1@example.com>".into()) };
        assert_eq!(mark.google_private(), json!({ "joltaHold": "5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10", "joltaHoldMessage": "<m1@example.com>" }));
        assert_eq!(mark.graph_properties()[1], json!({ "id": GRAPH_MESSAGE, "value": "<m1@example.com>" }));
        assert_eq!(Mark::Away.caldav_lines(), ["X-JOLTA-HOLD:away"]);
        assert_eq!(HoldSetId::parse("5b2f8c1e0d4a4e8b9a613c7e2d9f4a10"), None);
        assert_eq!(serde_json::to_value(&mark).unwrap(), json!({ "is": "hold", "set": "5b2f8c1e-0d4a-4e8b-9a61-3c7e2d9f4a10", "message": "<m1@example.com>" }));
    }
}
