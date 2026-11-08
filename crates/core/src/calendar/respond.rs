//! Answering an invitation from Jolta: from the card on its mail, or from
//! the calendar's event detail. `plan` decides who tells the organizer,
//! from where the calendar's copy of the event is; `answer` carries it out.
//!
//! Every path tells the organizer at most once. Google and Microsoft 365
//! reply themselves. iCloud doesn't reply to an outside organizer (the
//! calendar plan's Facts table), so a CalDAV copy is answered quietly, with
//! SCHEDULE-AGENT=CLIENT, and Jolta mails the reply; other CalDAV servers
//! get the same, so none of them sends a second one. Without a copy in a
//! calendar Jolta answers through, the reply mail is all there is.
//!
//! A new time goes with a Maybe or a Decline, for the occurrence the card
//! shows. Microsoft 365 sends it with the answer (`proposedNewTime`). Google
//! has no field for it, so Google sends the answer and Jolta mails an iMIP
//! COUNTER; a CalDAV copy is answered quietly and Jolta mails the COUNTER,
//! which carries the answer too; without a copy the COUNTER is all there is.

use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension};

use super::card::{self, Card, NewTime};
use super::rsvp::{self, Answer, ImipReply, Proposal, Rsvp, Scope};
use super::{Bounds, Calendar, CalendarId, CalendarSource, EventData, EventId, Moment, Person, Response, invitation, series, tz};
use crate::compose::Built;
use crate::model::{AccountId, MessageId};
use crate::{Error, Jolta, Result, imap, store};

/// What the answer is about.
#[derive(Clone, Debug)]
pub enum Subject {
    /// The invitation card on this message.
    Message(MessageId),
    /// An event in a calendar, at one occurrence's time when it repeats.
    Event { id: EventId, at: Option<Bounds> },
}

#[derive(Clone, Debug)]
pub struct Request {
    pub subject: Subject,
    pub answer: Answer,
    pub note: Option<String>,
    /// False for "Don't send a reply": the answer is kept and nobody is told.
    pub notify: bool,
    /// Only the occurrence the card or the detail shows. A single event
    /// ignores it.
    pub occurrence_only: bool,
    /// A new time for the event, or for a series the occurrence the card
    /// shows, with a Maybe or a Decline. Only from a card.
    pub proposal: Option<Proposal>,
}

/// What one answer does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Answer the calendar's copy through its provider; `Some(true)` lets
    /// the provider tell the organizer.
    pub provider: Option<bool>,
    /// Jolta sends the iMIP mail itself: the reply, or with a new time the
    /// COUNTER.
    pub mail: bool,
}

/// Who tells the organizer, from the source of the calendar's copy of the
/// event (None without one), and whether a new time goes with the answer.
pub fn plan(copy: Option<CalendarSource>, notify: bool, proposing: bool) -> Result<Plan> {
    if proposing && !notify {
        return Err(Error::Other("A new time goes to the organizer, so it can't go with Don't send a reply.".into()));
    }
    match copy {
        Some(CalendarSource::Graph) => Ok(Plan { provider: Some(notify), mail: false }),
        // Google's API has no proposals: Google sends the answer, Jolta the new time.
        Some(CalendarSource::Google) => Ok(Plan { provider: Some(notify), mail: proposing }),
        Some(CalendarSource::CalDav) => Ok(Plan { provider: Some(false), mail: notify }),
        Some(CalendarSource::EventKit) => {
            Err(Error::Other("Answer this invitation in the Calendar app: macOS doesn't let other apps answer for its calendars.".into()))
        }
        // A feed is read-only and a calendar on this Mac has no server; the
        // mail is the answer.
        Some(CalendarSource::Webcal | CalendarSource::Local) | None => Ok(Plan { provider: None, mail: notify }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// For the toast: "Accepted. Jolta emailed Ann."
    pub text: String,
    /// The account whose outbox holds a reply mail to send; its engine
    /// sends it. None when the organizer has it already.
    pub mailed_from: Option<AccountId>,
    /// The calendar whose copy changed.
    pub calendar: Option<CalendarId>,
    /// The provider couldn't be reached, or a change to the copy still
    /// waits: the answer is in the calendar op queue.
    pub queued: bool,
}

/// Whether an event's detail offers answers: an invitation to the user
/// from an organizer other than the calendar's own account, in a calendar
/// of the account's own (`Calendar::is_own`) that Jolta answers through.
pub fn can_answer(account: &str, calendar: &Calendar, e: &EventData) -> bool {
    let theirs = e.organizer.as_ref().is_some_and(|o| !o.address.trim().eq_ignore_ascii_case(account));
    calendar.is_own(account) && theirs && e.my_response().is_some() && plan(Some(calendar.source), true, false).is_ok_and(|p| p.provider.is_some())
}

#[derive(Clone, Copy, Debug)]
struct Copy {
    event: EventId,
    calendar: CalendarId,
    source: CalendarSource,
}

/// Everything an answer needs, read before anything is sent.
struct Resolved {
    account: AccountId,
    from: String,
    /// The addresses the reply answers as: the account's, and those its
    /// calendar's copy marks as the user's, never another account's.
    me: Vec<String>,
    uid: String,
    /// The calendar's copy: the series or single event, and its calendar.
    copy: Option<Copy>,
    /// The invitation as sent, or the CalDAV resource, for the reply mail.
    text: Option<String>,
    /// The occurrence answered alone, and its day: "Thu 5 Nov".
    occurrence: Option<(Moment, String)>,
    /// "Ann".
    organizer: String,
    /// The answer the card or the detail shows now.
    shown: Option<Response>,
    /// The version answered, as `store::remember_answer` keeps it.
    version: Vec<EventData>,
    /// Whether the card offers a new time; None from the calendar's detail.
    new_time: Option<NewTime>,
}

/// Answers, tells the organizer the way `plan` says, and keeps the answer
/// for the card. The reply mail is built before the provider is asked, so
/// an invitation Jolta can't reply to changes nothing anywhere. The same
/// answer again, said the same way, tells the organizer nothing new: a
/// reply still waiting goes now, and one sent stays the only one. Any
/// other answer takes back a reply still waiting.
pub async fn answer(jolta: &Jolta, req: &Request) -> Result<Outcome> {
    answer_at(jolta, req, Utc::now()).await
}

/// `answer` as at `now`, which picks the occurrence a series' card shows.
async fn answer_at(jolta: &Jolta, req: &Request, now: DateTime<Utc>) -> Result<Outcome> {
    let here = tz::here();
    let proposing = req.proposal.is_some();
    if proposing && req.answer == Answer::Accept {
        return Err(Error::Other("A new time goes with Maybe or Decline, not with Accept.".into()));
    }
    let r = match &req.subject {
        // A series gets a new time for the occurrence shown, never as a whole.
        Subject::Message(id) => from_message(jolta, *id, req.occurrence_only || proposing, now, here).await?,
        Subject::Event { .. } if proposing => return Err(Error::Other("Propose a new time from the invitation's card in mail.".into())),
        Subject::Event { id, at } => {
            let (id, at, only) = (*id, *at, req.occurrence_only);
            let mut r = jolta.store.read_async(move |c| from_event(c, id, at.filter(|_| only), here)).await?;
            let blob = jolta.store.read_async(move |c| Ok(c.query_row("SELECT blob FROM event WHERE id = ?1", [id], |r| r.get::<_, Option<String>>(0))?)).await?;
            r.text = blob.map(|hash| jolta.blobs.get(&hash)).transpose()?.map(|raw| String::from_utf8_lossy(&raw).into_owned());
            r
        }
    };
    if proposing {
        match r.new_time {
            Some(NewTime::Open { .. }) => {}
            Some(NewTime::Refused) => return Err(Error::Other(format!("{} doesn't take proposals for a new time for this meeting.", who(&r)))),
            None => return Err(Error::Other("This meeting lasts all day; Jolta proposes new times for meetings with a start and an end time.".into())),
        }
    }
    let plan = plan(r.copy.map(|c| c.source), req.notify, proposing)?;
    let rid = r.occurrence.as_ref().map(|(rid, _)| *rid);

    let (account, uid) = (r.account, r.uid.clone());
    let given = store::GivenAnswer { answer: req.answer.response(), note: req.note.clone().unwrap_or_default(), notify: req.notify, proposal: req.proposal };
    let before = jolta.store.read_async(move |c| store::given_answer(c, account, &uid, rid.as_ref())).await?;

    let mail = if plan.mail {
        let text = r.text.as_deref().ok_or_else(|| Error::Other("Jolta has no copy of this invitation to reply to.".into()))?;
        let reply = ImipReply { invitation: text, from: &r.from, me: &r.me, answer: req.answer, note: req.note.as_deref(), occurrence: rid };
        Some(match &req.proposal {
            Some(p) => rsvp::imip_counter(&reply, p, here)?,
            None => rsvp::imip_reply(&reply, here)?,
        })
    } else {
        None
    };
    // The provider tells the organizer the answer, and the new time when
    // Jolta doesn't mail it, only when they changed.
    let carried = store::GivenAnswer { proposal: given.proposal.filter(|_| !plan.mail), ..given.clone() };
    let mut queued = false;
    if let (Some(notify), Some(copy)) = (plan.provider, r.copy) {
        // A series answer goes again over an answer to one occurrence still
        // waiting, which it replaces.
        let (calendar, uid) = (copy.calendar, r.uid.clone());
        let over_occurrence = rid.is_none() && jolta.store.read_async(move |c| store::answer_waiting(c, calendar, &uid, true)).await?;
        if again(&carried, r.shown, before.as_ref()) && !over_occurrence {
            // The same answer is still on its way while its op waits.
            let (calendar, uid) = (copy.calendar, r.uid.clone());
            queued = jolta.store.read_async(move |c| store::answer_waiting(c, calendar, &uid, false)).await?;
        } else {
            let scope = rid.map_or(Scope::Series, Scope::Occurrence);
            queued = rsvp::answer_or_queue(jolta, &Rsvp { event: copy.event, answer: req.answer, note: req.note.clone(), notify, scope, proposal: req.proposal }).await?;
        }
    }

    let blob = mail.as_ref().map(|m| jolta.blobs.put(&m.raw)).transpose()?;
    let uid = r.uid.clone();
    let shown = r.shown;
    let version = r.version.clone();
    let waiting = jolta
        .store
        .write_async(move |tx| {
            // Read again where the outbox can't change underneath.
            let before = store::given_answer(tx, account, &uid, rid.as_ref())?;
            let again = !proposing && again(&given, shown, before.as_ref());
            // An answer that tells nothing new keeps the new time it repeats.
            let proposal = given.proposal.or(before.as_ref().and_then(|(b, _)| b.proposal).filter(|_| again));
            let kept = before.and_then(|(_, reply)| reply);
            // A proposal is the same mail each time it is built.
            let same = match mail.as_ref().filter(|_| proposing) {
                Some(m) => store::outbox_mail(tx, account, &m.message_id)?,
                None => None,
            };
            // The reply mail, and whether it waits to go.
            let (reply, waiting) = match (kept, same) {
                (kept, Some(same)) => {
                    if let Some(store::ReplyMail::Waiting(other)) = kept.filter(|k| *k != same) {
                        store::cancel_send(tx, other)?;
                    }
                    match same {
                        store::ReplyMail::Waiting(id) => {
                            store::send_now(tx, id)?;
                            (Some(id), true)
                        }
                        store::ReplyMail::Sent(id) => (Some(id), false),
                    }
                }
                (Some(store::ReplyMail::Waiting(id)), None) if again => {
                    store::send_now(tx, id)?;
                    (Some(id), true)
                }
                (Some(store::ReplyMail::Sent(id)), None) if again => (Some(id), false),
                (waiting, None) => {
                    if let Some(store::ReplyMail::Waiting(id)) = waiting {
                        store::cancel_send(tx, id)?;
                    }
                    match (&mail, &blob) {
                        (Some(m), Some(blob)) => (Some(queue(tx, account, m, blob)?), true),
                        _ => (None, false),
                    }
                }
            };
            store::remember_answer(tx, account, &uid, rid.as_ref(), &store::GivenAnswer { proposal, ..given }, &version, reply)?;
            Ok(waiting)
        })
        .await?;
    let mut calendar = r.copy.filter(|_| plan.provider.is_some()).map(|c| c.calendar);
    let mut text = told(req, &r, plan, queued);
    // A copy the user added elsewhere follows the answer, telling nobody.
    if r.copy.is_none() {
        match super::add::follow(jolta, r.account, &r.uid, rid, req.answer).await {
            Ok(Some(id)) => calendar = Some(id),
            Ok(None) => {}
            Err(e) => text = format!("{text} The copy in your calendar didn't change: {e}"),
        }
    }
    Ok(Outcome { text, mailed_from: waiting.then_some(r.account), calendar, queued })
}

/// The answer kept, given again the same way, to an invitation that still
/// shows it: an answer changed elsewhere since isn't the same. A new time
/// is the same only as the new time kept; an answer without one repeats
/// the answer a new time went with.
fn again(given: &store::GivenAnswer, shown: Option<Response>, before: Option<&(store::GivenAnswer, Option<store::ReplyMail>)>) -> bool {
    let same = |b: &store::GivenAnswer| (b.answer, &b.note, b.notify) == (given.answer, &given.note, given.notify) && given.proposal.is_none_or(|p| b.proposal == Some(p));
    shown == Some(given.answer) && before.is_some_and(|(b, _)| same(b))
}

/// The reply goes out at once: there is no undo for an answer yet.
fn queue(conn: &Connection, account: AccountId, m: &Built, blob: &str) -> Result<i64> {
    store::queue_outbox(conn, account, None, blob, &m.message_id, &m.from, &m.recipients, None, 0)
}

fn who(r: &Resolved) -> &str {
    if r.organizer.is_empty() { "the organizer" } else { &r.organizer }
}

/// "Accepted. Google told Ann.", "Declined Thu 5 Nov. Jolta emailed Ann.",
/// "Answered maybe without telling Ann."; with a new time "Proposed Fri 6
/// Nov, 14:00 to 14:45 with a maybe. Microsoft 365 told Ann."; queued
/// "Accepted. Google tells Ann as soon as Jolta reaches it."
fn told(req: &Request, r: &Resolved, plan: Plan, queued: bool) -> String {
    let text = told_now(req, r, plan);
    if !queued {
        return text;
    }
    let provider = match r.copy.map(|c| c.source) {
        Some(CalendarSource::Google) => "Google",
        Some(CalendarSource::Graph) => "Microsoft 365",
        _ => "your calendar",
    };
    let soon = format!("as soon as Jolta reaches {}", if provider == "your calendar" { "it" } else { provider });
    for said in [" Google told ", " Microsoft 365 told "] {
        if let Some(at) = text.find(said) {
            let rest = &text[at + said.len()..];
            return format!("{}{}tells {} {soon}.", &text[..at], &said[..said.len() - "told ".len()], rest.trim_end_matches('.'));
        }
    }
    format!("{text} It reaches {provider} {soon}.")
}

fn told_now(req: &Request, r: &Resolved, plan: Plan) -> String {
    if let Some(p) = &req.proposal {
        let here = tz::here();
        let (start, end) = (p.start.with_timezone(&here), p.end.with_timezone(&here));
        let end = if end.date_naive() == start.date_naive() { end.format("%H:%M").to_string() } else { end.format("%a %-d %b %H:%M").to_string() };
        let when = format!("{}, {} to {end}", start.format("%a %-d %b"), start.format("%H:%M"));
        let moved = r.occurrence.as_ref().map(|(_, day)| format!(" moving {day} to")).unwrap_or_default();
        let with = if req.answer == Answer::Decline { "and declined" } else { "with a maybe" };
        let done = format!("Proposed{moved} {when} {with}");
        return match r.copy.map(|c| c.source) {
            Some(CalendarSource::Graph) => format!("{done}. Microsoft 365 told {}.", who(r)),
            Some(CalendarSource::Google) => format!("{done}. Google told {} your answer and Jolta emailed the new time.", who(r)),
            _ => format!("{done}. Jolta emailed {}.", who(r)),
        };
    }
    let mut done = match req.answer {
        Answer::Accept => "Accepted".to_string(),
        Answer::Maybe => "Answered maybe".to_string(),
        Answer::Decline => "Declined".to_string(),
    };
    if let Some((_, day)) = &r.occurrence {
        done = format!("{done} for {day}");
    }
    let who = who(r);
    match (plan.provider, r.copy.map(|c| c.source)) {
        (Some(true), Some(CalendarSource::Google)) => format!("{done}. Google told {who}."),
        (Some(true), Some(CalendarSource::Graph)) => format!("{done}. Microsoft 365 told {who}."),
        _ if plan.mail => format!("{done}. Jolta emailed {who}."),
        _ => format!("{done} without telling {who}."),
    }
}

/// The card's invitation on a message: its event, the occurrence it shows,
/// and the calendar's copy.
async fn from_message(jolta: &Jolta, message: MessageId, occurrence_only: bool, now: DateTime<Utc>, here: Tz) -> Result<Resolved> {
    let (account, sender): (AccountId, String) = jolta
        .store
        .read_async(move |c| {
            c.query_row("SELECT account_id, from_addr FROM message WHERE id = ?1", [message], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("message {message}")))
        })
        .await?;
    let raw = imap::message_source(jolta, message).await?;
    let blobs = jolta.blobs.clone();
    jolta
        .store
        .read_async(move |c| {
            let every: Vec<String> = store::accounts(c)?.into_iter().map(|a| a.address).collect();
            let from = store::account(c, account)?.address;
            let invitations = invitation::find(&raw, &every);
            let cx = card::Context { message, account, me: &every, sender: &sender, now, here, blobs: &blobs };
            let a = card::cards(c, &invitations, &cx)?
                .into_iter()
                .find_map(|card| match card {
                    Card::Event(e) => e.answering.map(|a| (a, e.answer)),
                    Card::Reply(_) => None,
                })
                .ok_or_else(|| Error::Other("This message has no invitation to answer.".into()))?;
            let (a, shown) = a;
            let sent = invitations.iter().find(|i| i.event.uid == a.uid && i.event.recurrence_id.is_none()).or(invitations.iter().find(|i| i.event.uid == a.uid));
            // The copy in the calendars of the account the invitation came
            // to: another account's may be the organizer's own. A copy in a
            // calendar shared with the account is its owner's to answer, so
            // without one in the account's own the reply mail is the answer.
            let own = |calendar: &Calendar| calendar.is_own(&from);
            let copy = match store::stored_event_in(c, &a.uid, None, Some(account), here, own)? {
                None => sent
                    .and_then(|i| i.event.recurrence_id)
                    .map(|rid| store::stored_event_in(c, &a.uid, Some(&rid), Some(account), here, own))
                    .transpose()?
                    .flatten(),
                found => found,
            };
            let calendar = copy.as_ref().map(|s| store::calendar(c, s.calendar_id)).transpose()?;
            let me = addresses(&from, calendar.as_ref().zip(copy.as_ref().map(|s| &s.event)));
            let copy = copy.map(|s| Copy { event: s.event_id, calendar: s.calendar_id, source: s.source });
            let occurrence = a.occurrence.filter(|o| o.alone || occurrence_only).map(|o| (o.recurrence_id, o.day));
            let events: Vec<EventData> = invitations.iter().filter(|i| i.event.uid == a.uid).map(|i| i.event.clone()).collect();
            let version = answered(events, occurrence.as_ref().map(|(rid, _)| rid), here);
            Ok(Resolved {
                account,
                from,
                text: invitation::calendar_text(&raw, &a.uid),
                uid: a.uid,
                me,
                copy,
                occurrence,
                organizer: a.organizer,
                new_time: a.new_time,
                shown,
                version,
            })
        })
        .await
}

/// An event in one of the user's calendars. `at` names the occurrence to
/// answer alone.
fn from_event(conn: &Connection, event: EventId, at: Option<Bounds>, here: Tz) -> Result<Resolved> {
    let (calendar, data): (super::CalendarId, String) = conn
        .query_row("SELECT calendar_id, data FROM event WHERE id = ?1", [event], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("event {event}")))?;
    let data: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {event}: {e}")))?;
    let calendar = store::calendar(conn, calendar)?;
    let account = calendar.account_id.ok_or_else(|| Error::Other("This calendar belongs to no account, so Jolta has nowhere to answer from.".into()))?;
    let from = store::account(conn, account)?.address;
    rsvp::own_calendar(&calendar, &from)?;
    let me = addresses(&from, Some((&calendar, &data)));
    let repeats = data.recurrence.is_some() || data.recurrence_id.is_some();
    let occurrence = match at.filter(|_| repeats) {
        Some(at) => {
            let rid = rsvp::occurrence_at(conn, event, at, here)?;
            Some((rid, day(&rid, here)))
        }
        None => None,
    };
    let version = answered(vec![data.clone()], occurrence.as_ref().map(|(rid, _)| rid), here);
    Ok(Resolved {
        account,
        from,
        me,
        copy: Some(Copy { event, calendar: calendar.id, source: calendar.source }),
        text: None,
        occurrence,
        organizer: data.organizer.as_ref().map(short_name).unwrap_or_default(),
        shown: data.my_response(),
        version,
        new_time: None,
        uid: data.uid,
    })
}

/// The version an answer goes to, from the events of one UID: the
/// occurrence `rid` as they place it, else all of them.
fn answered(events: Vec<EventData>, rid: Option<&Moment>, here: Tz) -> Vec<EventData> {
    match rid.and_then(|rid| series::occurrence(&events, rid, here)) {
        Some(occurrence) => vec![occurrence],
        None => events,
    }
}

/// The account's address, then the aliases its calendar's copy of the
/// event marks as the user's: each sync marks only its own account's. A
/// copy in a calendar the account doesn't own (`Calendar::is_own`) adds
/// none, since Google marks that calendar's owner there.
pub(crate) fn addresses(account: &str, copy: Option<(&Calendar, &EventData)>) -> Vec<String> {
    let own = copy.filter(|(calendar, _)| calendar.is_own(account)).map(|(_, e)| e);
    let aliases = own.into_iter().flat_map(|e| &e.attendees).filter(|a| a.is_me).map(|a| &a.person.address);
    let mut me = vec![account.to_string()];
    for alias in aliases {
        if !me.iter().any(|m| m.eq_ignore_ascii_case(alias)) {
            me.push(alias.clone());
        }
    }
    me
}

/// "Thu 5 Nov" in the Mac's zone.
fn day(rid: &Moment, here: Tz) -> String {
    let d = match rid {
        Moment::Date(d) => *d,
        Moment::Time(t) => t.instant(here).with_timezone(&here).date_naive(),
    };
    if d.year() == Utc::now().with_timezone(&here).year() { d.format("%a %-d %b").to_string() } else { d.format("%a %-d %b %Y").to_string() }
}

fn short_name(p: &Person) -> String {
    match p.name.split_whitespace().next() {
        Some(first) => first.to_string(),
        None => p.address.clone(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::TimeZone;
    use mail_parser::MessageParser;

    use super::*;
    use crate::calendar::{Access, Response};
    use crate::calendar::invitation::tests::{import, jolta, mail};

    #[test]
    fn who_tells_the_organizer() {
        use CalendarSource::*;
        let row = |copy, notify| plan(copy, notify, false).map(|p| (p.provider, p.mail)).map_err(|e| e.to_string());
        assert_eq!(row(Some(Google), true), Ok((Some(true), false)), "Google replies itself");
        assert_eq!(row(Some(Graph), true), Ok((Some(true), false)), "so does Exchange");
        assert_eq!(row(Some(CalDav), true), Ok((Some(false), true)), "iCloud doesn't tell an outside organizer: the copy quietly, then Jolta's mail");
        assert_eq!(row(None, true), Ok((None, true)), "no copy: the mail alone");
        assert_eq!(row(Some(Webcal), true), Ok((None, true)), "a feed can't be answered");
        assert_eq!(row(Some(Local), true), Ok((None, true)));
        for copy in [Some(Google), Some(Graph), Some(CalDav)] {
            assert_eq!(row(copy, false), Ok((Some(false), false)), "{copy:?}: don't send a reply changes the copy and tells nobody");
        }
        for copy in [None, Some(Webcal), Some(Local)] {
            assert_eq!(row(copy, false), Ok((None, false)), "{copy:?}: only the answer is kept");
        }
        assert!(row(Some(EventKit), true).unwrap_err().contains("Calendar app"));
    }

    #[test]
    fn who_sends_a_new_time() {
        use CalendarSource::*;
        let row = |copy| plan(copy, true, true).map(|p| (p.provider, p.mail)).map_err(|e| e.to_string());
        assert_eq!(row(Some(Graph)), Ok((Some(true), false)), "Exchange sends the answer with proposedNewTime");
        assert_eq!(row(Some(Google)), Ok((Some(true), true)), "Google sends the answer, Jolta the COUNTER");
        assert_eq!(row(Some(CalDav)), Ok((Some(false), true)), "the copy quietly, then the COUNTER");
        for copy in [None, Some(Webcal), Some(Local)] {
            assert_eq!(row(copy), Ok((None, true)), "{copy:?}: the COUNTER alone");
        }
        assert!(row(Some(EventKit)).unwrap_err().contains("Calendar app"));
        assert!(plan(Some(Google), false, true).unwrap_err().to_string().contains("Don't send a reply"), "a new time always goes to the organizer");
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    fn request(message: MessageId, answer: Answer, note: Option<&str>, notify: bool, occurrence_only: bool) -> Request {
        Request { subject: Subject::Message(message), answer, note: note.map(str::to_string), notify, occurrence_only, proposal: None }
    }

    fn proposing(message: MessageId, answer: Answer, start: &str, end: &str) -> Request {
        let at = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc();
        Request { proposal: Some(Proposal { start: at(start), end: at(end) }), ..request(message, answer, None, true, false) }
    }

    /// The COUNTER's method, DTSTART and PARTSTAT, from a queued mail.
    fn counter_of(raw: &[u8]) -> (String, String, String) {
        let msg = MessageParser::default().parse(raw).unwrap();
        let part = msg.parts().find(|p| p.is_content_type("text", "calendar")).unwrap();
        let ics = String::from_utf8_lossy(&part.decoded()).into_owned();
        let line = |prefix: &str| ics.lines().find_map(|l| l.strip_prefix(prefix)).unwrap_or_default().to_string();
        let partstat = ics.split("PARTSTAT=").nth(1).and_then(|p| p.split([':', ';']).next()).unwrap_or_default().to_string();
        (part.content_type().unwrap().attribute("method").unwrap_or_default().to_string(), line("DTSTART:"), partstat)
    }

    fn run(j: &Jolta, req: Request) -> Result<Outcome> {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(answer_at(j, &req, now()))
    }

    /// (sender, recipients, seconds until it goes, raw) of each queued mail.
    fn outbox(j: &Jolta) -> Vec<(String, String, i64, Vec<u8>)> {
        let rows: Vec<(String, String, i64, String)> = j
            .store
            .read(|c| {
                let mut stmt = c.prepare("SELECT sender, recipients, send_after - created, blob FROM outbox ORDER BY id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        rows.into_iter().map(|(s, r, d, b)| (s, r, d, j.blobs.get(&b).unwrap())).collect()
    }

    fn card_answer(j: &Jolta, message: MessageId, raw: &[u8]) -> Option<Response> {
        let me = [crate::calendar::invitation::tests::ME.to_string()];
        let found = invitation::find(raw, &me);
        let cx = card::Context { message, account: AccountId(1), me: &me, sender: "ann@example.com", now: now(), here: tz::here(), blobs: &j.blobs };
        match j.store.read(|c| card::cards(c, &found, &cx)).unwrap().remove(0) {
            Card::Event(e) => e.answer,
            Card::Reply(_) => panic!("an event card"),
        }
    }

    #[test]
    fn without_a_calendar_copy_the_reply_is_a_mail_to_the_organizer() {
        let (j, _) = jolta("respond-mail");
        let message = import(&j, mail!("google-request.eml"));
        let done = run(&j, request(message, Answer::Maybe, Some("Ten minutes late"), true, false)).unwrap();
        assert_eq!(done, Outcome { text: "Answered maybe. Jolta emailed Ann.".into(), mailed_from: Some(AccountId(1)), calendar: None, queued: false });

        let sent = outbox(&j);
        assert_eq!(sent.len(), 1, "exactly one reply");
        let (sender, recipients, delay, raw) = &sent[0];
        assert_eq!((sender.as_str(), recipients.as_str(), *delay), ("me@example.com", "ann@example.com", 0), "to the organizer only, at once");
        let msg = MessageParser::default().parse(raw).unwrap();
        assert_eq!(msg.from().and_then(|f| f.first()).and_then(|a| a.address()), Some("me@example.com"));
        let to: Vec<&str> = msg.to().unwrap().mailboxes().filter_map(|a| a.address()).collect();
        assert_eq!(to, ["ann@example.com"]);
        assert!(msg.cc().is_none() && msg.bcc().is_none());
        let part = msg.parts().find(|p| p.is_content_type("text", "calendar")).expect("a calendar part");
        assert_eq!(part.content_type().unwrap().attribute("method"), Some("REPLY"));
        let ics = String::from_utf8_lossy(&part.decoded()).into_owned();
        assert!(ics.contains("PARTSTAT=TENTATIVE") && ics.contains("COMMENT:Ten minutes late"), "{ics}");

        assert_eq!(card_answer(&j, message, mail!("google-request.eml")), Some(Response::Tentative), "the card shows the answer kept");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// (state, seconds until it goes, PARTSTAT and COMMENT) of each outbox row.
    fn replies(j: &Jolta) -> Vec<(String, i64, String)> {
        let rows: Vec<(String, i64, String)> = j
            .store
            .read(|c| {
                let mut stmt = c.prepare("SELECT state, send_after - unixepoch(), blob FROM outbox ORDER BY id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        rows.into_iter()
            .map(|(state, wait, blob)| {
                let ics = reply_ics(&j.blobs.get(&blob).unwrap());
                let said: Vec<&str> = ics.lines().filter_map(|l| l.split_once("PARTSTAT=").map(|(_, p)| p.split([':', ';']).next().unwrap()).or(l.strip_prefix("COMMENT:"))).collect();
                (state, wait.max(0), said.join(" "))
            })
            .collect()
    }

    fn set_outbox(j: &Jolta, sql: &str) {
        j.store.write(|tx| Ok(tx.execute_batch(sql)?)).unwrap();
    }

    /// The same answer again keeps the reply already on its way or sent;
    /// a changed answer, note or reply choice replaces one still waiting.
    #[test]
    fn the_same_answer_again_sends_one_reply() {
        let (j, _) = jolta("respond-again");
        let message = import(&j, mail!("google-request.eml"));
        let again = |answer, note, notify| run(&j, request(message, answer, note, notify, false)).unwrap();

        again(Answer::Accept, None, true);
        again(Answer::Accept, None, true);
        assert_eq!(replies(&j), [("queued".into(), 0, "ACCEPTED".into())], "one reply for two Accepts");

        set_outbox(&j, "UPDATE outbox SET attempts = 1, send_after = unixepoch() + 30");
        again(Answer::Accept, None, true);
        assert_eq!(replies(&j), [("queued".into(), 0, "ACCEPTED".into())], "a retry after a failed send sends the waiting reply now");

        again(Answer::Decline, None, true);
        assert_eq!(replies(&j), [("queued".into(), 0, "DECLINED".into())], "a changed answer replaces the reply still waiting");
        again(Answer::Decline, Some("Away"), true);
        assert_eq!(replies(&j), [("queued".into(), 0, "DECLINED Away".into())], "so does a note");

        set_outbox(&j, "UPDATE outbox SET state = 'sent'");
        again(Answer::Decline, Some("Away"), true);
        assert_eq!(replies(&j).len(), 1, "the organizer has this answer already");
        again(Answer::Accept, None, true);
        assert_eq!(replies(&j)[1], ("queued".into(), 0, "ACCEPTED".into()), "a new answer after a sent one sends");

        again(Answer::Accept, None, false);
        assert_eq!(replies(&j).len(), 1, "Don't send a reply takes back the waiting reply");
        again(Answer::Accept, None, true);
        set_outbox(&j, "UPDATE outbox SET state = 'failed' WHERE state = 'queued'");
        again(Answer::Accept, None, true);
        again(Answer::Accept, None, true);
        let states: Vec<String> = replies(&j).into_iter().map(|(state, ..)| state).collect();
        assert_eq!(states, ["sent", "failed", "queued"], "a reply that failed for good is sent again, once");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn dont_send_a_reply_sends_nothing_and_keeps_the_answer() {
        let (j, _) = jolta("respond-quiet");
        let message = import(&j, mail!("google-request.eml"));
        let done = run(&j, request(message, Answer::Decline, None, false, false)).unwrap();
        assert_eq!(done, Outcome { text: "Declined without telling Ann.".into(), mailed_from: None, calendar: None, queued: false });
        assert!(outbox(&j).is_empty(), "no mail");
        assert_eq!(card_answer(&j, message, mail!("google-request.eml")), Some(Response::Declined));

        run(&j, request(message, Answer::Accept, None, true, false)).unwrap();
        assert_eq!(outbox(&j).len(), 1, "answering again with a reply sends one");
        assert_eq!(card_answer(&j, message, mail!("google-request.eml")), Some(Response::Accepted));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn one_occurrence_of_a_series_by_mail() {
        let (j, _) = jolta("respond-occurrence");
        let message = import(&j, mail!("series.eml"));
        let done = run(&j, request(message, Answer::Decline, None, true, true)).unwrap();
        assert_eq!(done.text, "Declined for Thu 5 Nov. Jolta emailed Ann.", "the occurrence the card shows: Thursday's, moved");
        let raw = &outbox(&j)[0].3;
        let msg = MessageParser::default().parse(raw).unwrap();
        let ics = String::from_utf8_lossy(&msg.parts().find(|p| p.is_content_type("text", "calendar")).unwrap().decoded()).into_owned();
        assert!(ics.contains("RECURRENCE-ID;TZID=Europe/Brussels:20261105T093000"), "{ics}");
        assert!(!ics.contains("RRULE"));

        let series = run(&j, request(message, Answer::Accept, None, true, false)).unwrap();
        assert_eq!(series.text, "Accepted. Jolta emailed Ann.");
        let kept: Vec<(String, String)> = j
            .store
            .read(|c| {
                let mut stmt = c.prepare("SELECT recurrence_id, answer FROM invitation_answer")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        assert_eq!(kept, [(String::new(), "accepted".to_string())], "the series answer replaces the occurrence's");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    fn kept_answers(j: &Jolta) -> Vec<(String, String)> {
        j.store
            .read(|c| {
                let mut stmt = c.prepare("SELECT recurrence_id, answer FROM invitation_answer ORDER BY recurrence_id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap()
    }

    fn reply_ics(raw: &[u8]) -> String {
        let msg = MessageParser::default().parse(raw).unwrap();
        String::from_utf8_lossy(&msg.parts().find(|p| p.is_content_type("text", "calendar")).unwrap().decoded()).into_owned()
    }

    /// An invitation to one occurrence of a series, without the series, is
    /// answered for that occurrence whatever the scope menu says.
    #[test]
    fn an_invitation_to_one_occurrence_answers_only_it() {
        let (j, _) = jolta("respond-one-occurrence");
        let raw = "From: ann@example.com\r\nTo: me@example.com\r\nSubject: s\r\nMessage-ID: <one@example.com>\r\n\
                   Content-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:weekly\r\n\
                   RECURRENCE-ID:20261112T090000Z\r\nDTSTART:20261112T100000Z\r\nDTEND:20261112T110000Z\r\nSUMMARY:Weekly\r\n\
                   ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let message = import(&j, raw.as_bytes());
        let done = run(&j, request(message, Answer::Decline, None, true, false)).unwrap();
        assert_eq!(done.text, "Declined for Thu 12 Nov. Jolta emailed Ann.");
        let ics = reply_ics(&outbox(&j)[0].3);
        assert!(ics.contains("RECURRENCE-ID:20261112T090000Z"), "{ics}");
        assert_eq!(kept_answers(&j), [("20261112T090000 UTC".to_string(), "declined".to_string())], "kept for the occurrence, not the series");
        assert_eq!(card_answer(&j, message, raw.as_bytes()), Some(Response::Declined));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_series_answer_takes_back_an_occurrences_waiting_reply() {
        let (j, _) = jolta("respond-series-over-occurrence");
        let raw = "From: ann@example.com\r\nTo: me@example.com\r\nSubject: s\r\nMessage-ID: <weekly@example.com>\r\n\
                   Content-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:weekly\r\n\
                   DTSTART:20261112T100000Z\r\nDTEND:20261112T110000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Weekly\r\n\
                   ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let message = import(&j, raw.as_bytes());
        run(&j, request(message, Answer::Accept, None, true, true)).unwrap();
        assert_eq!(replies(&j).len(), 1, "the occurrence's Accept is waiting");
        run(&j, request(message, Answer::Decline, None, false, false)).unwrap();
        assert!(replies(&j).is_empty(), "declining the series without a reply takes the occurrence's Accept back");
        assert_eq!(kept_answers(&j), [(String::new(), "declined".to_string())]);
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Two of the user's accounts are invited, the other one listed first.
    /// The reply comes from the account the invitation came to and answers
    /// for that account alone.
    #[test]
    fn the_reply_answers_for_the_receiving_account_only() {
        let (j, _) = jolta("respond-two-invited");
        j.store
            .write(|tx| Ok(tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (2, 'imap', 'me2@example.net', 'me2', 'a2', 0)")?))
            .unwrap();
        let raw = "From: ann@example.com\r\nTo: me2@example.net, me@example.com\r\nSubject: s\r\nMessage-ID: <both@example.com>\r\n\
                   Content-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:both\r\n\
                   DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Planning\r\nORGANIZER;CN=Ann:mailto:ann@example.com\r\n\
                   ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me2@example.net\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\n\
                   END:VEVENT\r\nEND:VCALENDAR\r\n";
        let message = import(&j, raw.as_bytes());
        run(&j, request(message, Answer::Accept, None, true, false)).unwrap();
        let (sender, ..) = &outbox(&j)[0];
        let ics = reply_ics(&outbox(&j)[0].3);
        assert_eq!(sender, "me@example.com");
        let attendees: Vec<&str> = ics.lines().filter(|l| l.starts_with("ATTENDEE")).collect();
        assert_eq!(attendees, ["ATTENDEE;PARTSTAT=ACCEPTED:mailto:me@example.com"], "{ics}");

        run(&j, proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 16:00")).unwrap();
        let (sender, ..) = &outbox(&j)[0];
        let ics = reply_ics(&outbox(&j)[0].3);
        assert!(ics.contains("METHOD:COUNTER") && sender == "me@example.com", "{ics}");
        let attendees: Vec<&str> = ics.lines().filter(|l| l.starts_with("ATTENDEE")).collect();
        assert_eq!(attendees, ["ATTENDEE;PARTSTAT=TENTATIVE:mailto:me@example.com"], "a proposal too speaks for the receiving account alone");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The account's copy, synced with the aliases its server knows, lets
    /// the reply answer as an alias of that account.
    #[test]
    fn the_reply_may_answer_as_an_alias_the_accounts_copy_knows() {
        let raw = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:alias\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\n\
                   ORGANIZER:mailto:ann@example.com\r\nATTENDEE:mailto:me2@example.net\r\nATTENDEE:mailto:alias@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let copy = crate::calendar::ical::read_invitation(raw, &["alias@example.com".to_string()]).unwrap().remove(0).event;
        let me = addresses("me@example.com", Some((&calendar_of(CalendarSource::CalDav, Access::Owner, "/cal/work/"), &copy)));
        assert_eq!(me, ["me@example.com", "alias@example.com"], "the other account isn't one of this account's addresses");
        let reply = rsvp::imip_reply(&ImipReply { invitation: raw, from: "me@example.com", me: &me, answer: Answer::Accept, note: None, occurrence: None }, tz::here()).unwrap();
        let ics = reply_ics(&reply.raw);
        assert!(ics.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:alias@example.com") && !ics.contains("me2@"), "{ics}");
    }

    /// A calendar of account 1.
    pub(crate) fn calendar_of(source: CalendarSource, access: Access, remote_id: &str) -> Calendar {
        Calendar {
            id: CalendarId(1),
            account_id: Some(AccountId(1)),
            source,
            remote_id: remote_id.into(),
            name: "Calendar".into(),
            color: None,
            zone: None,
            access,
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

    /// Google marks a shared calendar's owner as its copies' self, and gives
    /// a calendar shared with "Make changes and manage sharing" the owner
    /// role, so only the primary calendar's copy names an alias.
    #[test]
    fn a_shared_calendars_copy_names_no_alias() {
        let raw = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:bo\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\n\
                   ORGANIZER:mailto:ann@example.com\r\nATTENDEE:mailto:bo@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let copy = crate::calendar::ical::read_invitation(raw, &["bo@example.com".to_string()]).unwrap().remove(0).event;
        for (source, access, remote_id, aliases) in [
            (CalendarSource::Google, Access::Owner, "me@example.com", &["me@example.com", "bo@example.com"][..]),
            (CalendarSource::Google, Access::Owner, "bo@example.com", &["me@example.com"][..]),
            (CalendarSource::Google, Access::Write, "bo@example.com", &["me@example.com"][..]),
            (CalendarSource::Graph, Access::Write, "AAMkBo", &["me@example.com"][..]),
            (CalendarSource::CalDav, Access::Write, "/cal/bo/", &["me@example.com"][..]),
        ] {
            let me = addresses("me@example.com", Some((&calendar_of(source, access, remote_id), &copy)));
            assert_eq!(me, aliases, "{source:?} calendar {remote_id} with {access:?}");
        }
    }

    /// A copy in a calendar shared with the account is its owner's, whom
    /// Google marks as the copy's self, even with the owner role Google
    /// gives "Make changes and manage sharing". From mail the answer goes by
    /// mail from the receiving account, as without a copy; the copy's detail
    /// offers no answer, and answering it is refused before any provider.
    #[test]
    fn a_shared_calendars_copy_is_never_answered_through() {
        use crate::calendar::invitation::tests::ME;
        use crate::calendar::{Attendee, Role, rsvp};
        let bo = Attendee { person: Person { address: "bo@example.com".into(), name: String::new() }, role: Role::Required, response: Response::NeedsAction, comment: String::new(), is_me: true };
        for (source, access, remote_id) in [
            (CalendarSource::Google, Access::Owner, "bo@example.com"),
            (CalendarSource::Google, Access::Write, "bo@example.com"),
            (CalendarSource::Graph, Access::Write, "AAMkBo"),
            (CalendarSource::CalDav, Access::Write, "/cal/bo/"),
        ] {
            let (j, _) = jolta("respond-shared");
            let message = import(&j, mail!("google-request.eml"));
            let mut copy = invitation::find(mail!("google-request.eml"), &[ME.into()]).remove(0).event;
            copy.attendees.iter_mut().for_each(|a| a.is_me = false);
            copy.attendees.push(bo.clone());
            let (calendar, event) = j
                .store
                .write(|tx| {
                    let remote = store::RemoteCalendar { remote_id: remote_id.into(), name: "Bo".into(), color: None, zone: None, access, primary: true, refresh: None, video: None };
                    let calendar = store::sync_calendar_list(tx, AccountId(1), source, &[remote])?[0].id;
                    store::apply_item(tx, calendar, "bo", None, std::slice::from_ref(&copy), None, &series::Horizon::around(now(), tz::here()))?;
                    let event = tx.query_row("SELECT id FROM event WHERE calendar_id = ?1", [calendar], |r| r.get(0))?;
                    Ok((store::calendar(tx, calendar)?, event))
                })
                .unwrap();
            let case = format!("{source:?} calendar {remote_id} with {access:?}");

            let done = run(&j, request(message, Answer::Accept, None, true, false)).unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(done, Outcome { text: "Accepted. Jolta emailed Ann.".into(), mailed_from: Some(AccountId(1)), calendar: None, queued: false }, "{case}");
            assert_eq!(outbox(&j)[0].0, ME, "{case}: from the receiving account");
            assert_eq!(card_answer(&j, message, mail!("google-request.eml")), Some(Response::Accepted), "{case}: the answer given from Jolta, never the owner's");
            set_outbox(&j, "UPDATE outbox SET state = 'sent'");
            run(&j, request(message, Answer::Accept, None, true, false)).unwrap_or_else(|e| panic!("{case}: {e}"));
            let sent: Vec<String> = replies(&j).into_iter().map(|(state, ..)| state).collect();
            assert_eq!(sent, ["sent"], "{case}: the same answer again sends nothing");

            assert!(!can_answer(ME, &calendar, &copy), "{case}: the detail offers no answer");
            let detail = Request { subject: Subject::Event { id: event, at: None }, ..request(message, Answer::Decline, None, true, false) };
            let err = run(&j, detail).unwrap_err().to_string();
            assert!(err.contains("shared with you"), "{case}: {err}");
            let direct = Rsvp { event, answer: Answer::Decline, note: None, notify: true, scope: Scope::Series, proposal: None };
            let err = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(rsvp::answer(&j, &direct)).unwrap_err().to_string();
            assert!(err.contains("shared with you"), "{case}: {err}");
            assert_eq!(outbox(&j).len(), 1, "{case}: nothing more went out");
            std::fs::remove_dir_all(&j.data_dir).unwrap();
        }
    }

    /// One of the user's accounts invites another. The organizer's copy is
    /// in the first account's calendar; the invitation is answered from the
    /// account it came to, here by mail, and never through that copy.
    #[test]
    fn one_of_my_accounts_invites_another() {
        let (j, _) = jolta("respond-self");
        let raw = "From: me2@example.net\r\nTo: me@example.com\r\nSubject: s\r\nMessage-ID: <self@example.net>\r\n\
                   Content-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:self\r\n\
                   DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Planning\r\nORGANIZER;CN=Me Elsewhere:mailto:me2@example.net\r\n\
                   ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        j.store
            .write(|tx| {
                tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created, calendar) VALUES (2, 'gmail', 'me2@example.net', 'me2', 'a2', 0, 1)")?;
                let remote = store::RemoteCalendar {
                    remote_id: "primary".into(),
                    name: "Other".into(),
                    color: None,
                    zone: None,
                    access: crate::calendar::Access::Owner,
                    primary: true,
                    refresh: None,
                    video: None,
                };
                let calendar = store::sync_calendar_list(tx, AccountId(2), CalendarSource::Google, &[remote])?[0].id;
                let organizers = invitation::find(raw.as_bytes(), &["me2@example.net".into()]).remove(0).event;
                store::apply_item(tx, calendar, "self", None, &[organizers], None, &crate::calendar::series::Horizon::around(now(), tz::here()))?;
                Ok(())
            })
            .unwrap();
        let message = import(&j, raw.as_bytes());
        let done = run(&j, request(message, Answer::Accept, None, true, false)).expect("answered by mail, not through the organizer's copy");
        assert_eq!(done.text, "Accepted. Jolta emailed Me.");
        assert_eq!(outbox(&j).len(), 1);
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// An answer sent by mail holds for an update with a new title and
    /// reads "Not answered" on one with a new time, until it is answered.
    #[test]
    fn an_update_that_moves_the_meeting_asks_again() {
        use crate::calendar::card::tests::{V1, version};
        let (j, _) = jolta("respond-update");
        let first = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", V1);
        let m1 = import(&j, first.as_bytes());
        run(&j, request(m1, Answer::Accept, None, true, false)).unwrap();
        set_outbox(&j, "UPDATE outbox SET state = 'sent'");

        let retitled = version("v2", "sync", "REQUEST", 1, "20261104T090000Z", &V1.replace("SUMMARY:Sync", "SUMMARY:Weekly sync"));
        let m2 = import(&j, retitled.as_bytes());
        assert_eq!(card_answer(&j, m2, retitled.as_bytes()), Some(Response::Accepted), "a new title keeps the answer");

        let moved = version("v3", "sync", "REQUEST", 2, "20261104T100000Z", &V1.replace("20261112T15", "20261113T15").replace("20261112T16", "20261113T16"));
        let m3 = import(&j, moved.as_bytes());
        assert_eq!(card_answer(&j, m3, moved.as_bytes()), Some(Response::NeedsAction), "a new day asks again");
        assert_eq!(card_answer(&j, m2, retitled.as_bytes()), None, "an older version shows no answer");
        assert!(run(&j, request(m2, Answer::Decline, None, true, false)).is_err(), "nor answers");

        run(&j, request(m3, Answer::Accept, None, true, false)).unwrap();
        assert_eq!(card_answer(&j, m3, moved.as_bytes()), Some(Response::Accepted));
        let sent: Vec<String> = replies(&j).into_iter().map(|(state, ..)| state).collect();
        assert_eq!(sent, ["sent", "queued"], "the same answer to the moved meeting is a new reply");
        assert!(reply_ics(&outbox(&j)[1].3).contains("SEQUENCE:2"));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// "Don't send a reply" keeps the version answered as a reply mail
    /// would: the card and the list keep the answer for a new title, and ask
    /// again for a new day or a new repeat rule.
    #[test]
    fn an_answer_without_a_reply_asks_again_after_a_move() {
        use crate::calendar::card::tests::{V1, version};
        let (j, _) = jolta("respond-quiet-update");
        let list = |m: MessageId| {
            j.store
                .read(|c| {
                    let thread = c.query_row("SELECT thread_id FROM message WHERE id = ?1", [m], |r| r.get(0))?;
                    Ok(store::thread_invitations(c, &[thread], now(), tz::here())?.get(&thread).copied())
                })
                .unwrap()
        };
        let both = |m: MessageId, raw: &str| (card_answer(&j, m, raw.as_bytes()), list(m));
        let accepted = (Some(Response::Accepted), Some(store::InvitationState::Answered(Response::Accepted)));
        let asked = (Some(Response::NeedsAction), Some(store::InvitationState::Unanswered));

        let first = version("q1", "quiet", "REQUEST", 0, "20261104T080000Z", V1);
        let m1 = import(&j, first.as_bytes());
        run(&j, request(m1, Answer::Accept, None, false, false)).unwrap();
        assert!(outbox(&j).is_empty(), "no reply");
        let retitled = version("q2", "quiet", "REQUEST", 1, "20261104T090000Z", &V1.replace("SUMMARY:Sync", "SUMMARY:Weekly sync"));
        assert_eq!(both(import(&j, retitled.as_bytes()), &retitled), accepted, "a new title keeps the answer");
        let moved = version("q3", "quiet", "REQUEST", 2, "20261104T100000Z", &V1.replace("20261112T15", "20261113T15").replace("20261112T16", "20261113T16"));
        assert_eq!(both(import(&j, moved.as_bytes()), &moved), asked, "a new day asks again");

        let series = "DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Sync\r\n";
        let weekly = version("w1", "quiet-series", "REQUEST", 0, "20261104T080000Z", &format!("{series}RRULE:FREQ=WEEKLY\r\n"));
        let w1 = import(&j, weekly.as_bytes());
        run(&j, request(w1, Answer::Accept, None, false, false)).unwrap();
        let renamed = version("w2", "quiet-series", "REQUEST", 1, "20261104T090000Z", &format!("{}RRULE:FREQ=WEEKLY\r\n", series.replace("Sync", "Team sync")));
        assert_eq!(both(import(&j, renamed.as_bytes()), &renamed), accepted, "the same rule");
        let daily = version("w3", "quiet-series", "REQUEST", 2, "20261104T100000Z", &format!("{series}RRULE:FREQ=DAILY\r\n"));
        assert_eq!(both(import(&j, daily.as_bytes()), &daily), asked, "weekly became daily");
        assert!(outbox(&j).is_empty());
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The reply mail names no repeat rule: the request it answered does.
    #[test]
    fn a_new_repeat_rule_asks_again() {
        use crate::calendar::card::tests::version;
        let (j, _) = jolta("respond-rule");
        let series = "DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Sync\r\n";
        let weekly = version("v1", "sync", "REQUEST", 0, "20261104T080000Z", &format!("{series}RRULE:FREQ=WEEKLY\r\n"));
        let m1 = import(&j, weekly.as_bytes());
        run(&j, request(m1, Answer::Accept, None, true, false)).unwrap();
        set_outbox(&j, "UPDATE outbox SET state = 'sent'");

        let retitled = version("v2", "sync", "REQUEST", 1, "20261104T090000Z", &format!("{}RRULE:FREQ=WEEKLY;INTERVAL=1\r\n", series.replace("Sync", "Team sync")));
        let m2 = import(&j, retitled.as_bytes());
        assert_eq!(card_answer(&j, m2, retitled.as_bytes()), Some(Response::Accepted), "the same rule written out");
        let daily = version("v3", "sync", "REQUEST", 2, "20261104T100000Z", &format!("{series}RRULE:FREQ=DAILY\r\n"));
        let m3 = import(&j, daily.as_bytes());
        assert_eq!(card_answer(&j, m3, daily.as_bytes()), Some(Response::NeedsAction), "weekly became daily");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The series answer covers an occurrence invited to alone while it
    /// stays where the series has it.
    #[test]
    fn a_moved_occurrence_asks_again_after_its_series_was_answered() {
        use crate::calendar::card::tests::version;
        let (j, _) = jolta("respond-series-occurrence");
        let series = version("s", "standup", "REQUEST", 0, "20261104T080000Z", "DTSTART:20261110T083000Z\r\nDTEND:20261110T084500Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\n");
        let m = import(&j, series.as_bytes());
        run(&j, request(m, Answer::Accept, None, true, false)).unwrap();
        set_outbox(&j, "UPDATE outbox SET state = 'sent'");

        let occurrence = |id: &str, start: &str, title: &str| {
            let lines = format!("RECURRENCE-ID:20261117T083000Z\r\nDTSTART:{start}\r\nDTEND:{}\r\nSUMMARY:{title}\r\n", start.replace("3000Z", "4500Z"));
            version(id, "standup", "REQUEST", 1, "20261104T090000Z", &lines)
        };
        let renamed = occurrence("o1", "20261117T083000Z", "Standup with demo");
        let m1 = import(&j, renamed.as_bytes());
        assert_eq!(card_answer(&j, m1, renamed.as_bytes()), Some(Response::Accepted), "at the series' time");
        let moved = occurrence("o2", "20261118T133000Z", "Standup");
        let m2 = import(&j, moved.as_bytes());
        assert_eq!(card_answer(&j, m2, moved.as_bytes()), Some(Response::NeedsAction), "moved a day and five hours");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn only_invitations_to_the_user_have_answers() {
        let (j, _) = jolta("respond-refused");
        let cancel = import(&j, mail!("cancel.eml"));
        let err = run(&j, request(cancel, Answer::Accept, None, true, false)).unwrap_err();
        assert!(err.to_string().contains("no invitation to answer"), "{err}");

        let own = "Subject: s\r\nMessage-ID: <own@example.com>\r\nContent-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\n\
                   BEGIN:VEVENT\r\nUID:own\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nORGANIZER:mailto:me@example.com\r\n\
                   ATTENDEE:mailto:bo@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let own = import(&j, own.as_bytes());
        assert!(run(&j, request(own, Answer::Accept, None, true, false)).is_err(), "the user's own meeting");
        assert!(outbox(&j).is_empty());
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Without a calendar copy the COUNTER is the answer: it goes to the
    /// organizer once, a changed time replaces one still waiting, and the
    /// card keeps the answer that went with it.
    #[test]
    fn a_new_time_without_a_copy_is_one_counter_mail() {
        let (j, _) = jolta("respond-propose");
        let message = import(&j, mail!("google-request.eml"));
        let done = run(&j, proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45")).unwrap();
        assert_eq!((done.mailed_from, done.calendar), (Some(AccountId(1)), None));
        assert!(done.text.starts_with("Proposed Fri 13 Nov, ") && done.text.ends_with(" with a maybe. Jolta emailed Ann."), "{}", done.text);

        let sent = outbox(&j);
        assert_eq!(sent.len(), 1);
        let (sender, recipients, delay, raw) = &sent[0];
        assert_eq!((sender.as_str(), recipients.as_str(), *delay), ("me@example.com", "ann@example.com", 0), "to the organizer only, at once");
        assert_eq!(counter_of(raw), ("COUNTER".into(), "20261113T150000Z".into(), "TENTATIVE".into()));
        assert_eq!(card_answer(&j, message, mail!("google-request.eml")), Some(Response::Tentative), "the card shows the answer that went with it");

        run(&j, proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45")).unwrap();
        assert_eq!(replies(&j).len(), 1, "the same proposal twice is one mail");
        set_outbox(&j, "UPDATE outbox SET attempts = 1, send_after = unixepoch() + 30");
        run(&j, proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45")).unwrap();
        assert_eq!(replies(&j)[0].1, 0, "a retry after a failed send sends the waiting one now");

        run(&j, proposing(message, Answer::Decline, "2026-11-13 16:00", "2026-11-13 16:45")).unwrap();
        let sent = outbox(&j);
        assert_eq!(sent.len(), 1, "a new time replaces the proposal still waiting");
        assert_eq!(counter_of(&sent[0].3), ("COUNTER".into(), "20261113T160000Z".into(), "DECLINED".into()));

        set_outbox(&j, "UPDATE outbox SET state = 'sent'");
        run(&j, proposing(message, Answer::Decline, "2026-11-13 16:00", "2026-11-13 16:45")).unwrap();
        assert_eq!(outbox(&j).len(), 1, "the organizer has this proposal already");
        run(&j, request(message, Answer::Decline, None, true, false)).unwrap();
        assert_eq!(outbox(&j).len(), 1, "the proposal carried this Decline already");
        run(&j, request(message, Answer::Accept, None, true, false)).unwrap();
        assert_eq!(outbox(&j).len(), 2, "a new answer after a proposal is a reply of its own");
        assert_eq!(counter_of(&outbox(&j)[1].3).0, "REPLY");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A proposal or a reply the organizer has already leaves nothing in the
    /// outbox, so the caller waits for no mail to go.
    #[test]
    fn nothing_is_mailed_once_the_organizer_has_it() {
        let (j, _) = jolta("respond-nothing-left");
        let message = import(&j, mail!("google-request.eml"));
        let mailed = |req: Request| run(&j, req).unwrap().mailed_from;
        let friday = || proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45");
        assert_eq!(mailed(friday()), Some(AccountId(1)), "the COUNTER waits in the outbox");
        assert_eq!(mailed(friday()), Some(AccountId(1)), "and still does");
        set_outbox(&j, "UPDATE outbox SET state = 'sent'");
        assert_eq!(mailed(friday()), None, "the organizer has this proposal");
        assert_eq!(mailed(request(message, Answer::Maybe, None, true, false)), None, "and the maybe it carried");
        assert_eq!(mailed(request(message, Answer::Accept, None, true, false)), Some(AccountId(1)), "a new answer waits");
        set_outbox(&j, "UPDATE outbox SET state = 'sent'");
        assert_eq!(mailed(request(message, Answer::Accept, None, true, false)), None, "the organizer has it");
        assert_eq!(outbox(&j).len(), 2);
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Microsoft 365 sends a new time with the answer. Once Exchange took a
    /// proposal, the same one again sends nothing; a changed time or answer
    /// goes to Exchange again. Exchange can't be reached from a test, so
    /// everything that goes to it fails here.
    #[test]
    fn the_same_proposal_goes_to_microsoft_365_once() {
        use crate::calendar::invitation::tests::ME;
        let (j, calendar) = jolta("respond-propose-graph");
        let message = import(&j, mail!("google-request.eml"));
        let mut copy = invitation::find(mail!("google-request.eml"), &[ME.into()]).remove(0).event;
        let store_copy = |copy: &EventData| {
            let blob = j.blobs.put(br#"{"id":"AAMk"}"#).unwrap();
            j.store.write(|tx| store::apply_item(tx, calendar, "AAMk", None, std::slice::from_ref(copy), Some(&blob), &series::Horizon::around(now(), tz::here()))).unwrap();
        };
        j.store.write(|tx| Ok(tx.execute_batch("UPDATE account SET kind = 'graph'; UPDATE calendar SET source = 'graph'")?)).unwrap();
        store_copy(&copy);

        let at = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc();
        let friday = || proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45");
        assert!(run(&j, friday()).is_err(), "the proposal goes to Exchange");
        assert!(kept_answers(&j).is_empty(), "one Exchange didn't take isn't kept");

        // What Exchange taking the proposal leaves: the copy answered maybe,
        // and the answer kept with its new time.
        copy.attendees.iter_mut().filter(|a| a.is_me).for_each(|a| a.response = Response::Tentative);
        store_copy(&copy);
        let given = store::GivenAnswer {
            answer: Response::Tentative,
            note: String::new(),
            notify: true,
            proposal: Some(Proposal { start: at("2026-11-13 15:00"), end: at("2026-11-13 15:45") }),
        };
        j.store.write(|tx| store::remember_answer(tx, AccountId(1), &copy.uid, None, &given, std::slice::from_ref(&copy), None)).unwrap();

        let again = run(&j, friday()).expect("the same proposal sends nothing");
        assert!(again.text.ends_with("Microsoft 365 told Ann."), "{}", again.text);
        assert!(run(&j, proposing(message, Answer::Maybe, "2026-11-13 16:00", "2026-11-13 16:45")).is_err(), "a changed time goes to Exchange");
        assert!(run(&j, proposing(message, Answer::Decline, "2026-11-13 15:00", "2026-11-13 15:45")).is_err(), "so does a changed answer");
        run(&j, request(message, Answer::Maybe, None, true, false)).expect("Exchange has this maybe, sent with the proposal");
        run(&j, friday()).expect("and still has the proposal");
        assert!(outbox(&j).is_empty(), "Exchange tells the organizer, not Jolta");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_new_time_for_a_series_goes_to_the_occurrence_shown() {
        let (j, _) = jolta("respond-propose-series");
        let message = import(&j, mail!("series.eml"));
        let done = run(&j, proposing(message, Answer::Maybe, "2026-11-05 13:00", "2026-11-05 13:15")).unwrap();
        assert!(done.text.starts_with("Proposed moving Thu 5 Nov to Thu 5 Nov, "), "{}", done.text);
        let ics = reply_ics(&outbox(&j)[0].3);
        assert!(ics.contains("METHOD:COUNTER") && ics.contains("RECURRENCE-ID;TZID=Europe/Brussels:20261105T093000"), "{ics}");
        let event = &ics[ics.find("BEGIN:VEVENT").unwrap()..];
        assert!(!event.contains("RRULE"), "never the whole series: {ics}");
        assert_eq!(kept_answers(&j), [("20261105T093000 Europe/Brussels".to_string(), "tentative".to_string())], "kept for the occurrence");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_new_time_is_refused_before_anything_goes_out() {
        let (j, _) = jolta("respond-propose-refused");
        let message = import(&j, mail!("google-request.eml"));
        let refused = |req: Request| run(&j, req).unwrap_err().to_string();
        assert!(refused(proposing(message, Answer::Accept, "2026-11-13 15:00", "2026-11-13 15:45")).contains("not with Accept"));
        assert!(refused(Request { notify: false, ..proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45") }).contains("Don't send a reply"));
        assert!(refused(proposing(message, Answer::Maybe, "2026-11-13 15:45", "2026-11-13 15:00")).contains("ends before it starts"));
        let detail = Request { subject: Subject::Event { id: EventId(1), at: None }, ..proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45") };
        assert!(refused(detail).contains("card in mail"));

        let raw = "From: ann@example.com\r\nTo: me@example.com\r\nSubject: s\r\nMessage-ID: <fixed@example.com>\r\n\
                   Content-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:fixed\r\n\
                   DTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Board\r\nX-MICROSOFT-DISALLOW-COUNTER:TRUE\r\n\
                   ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let fixed = import(&j, raw.as_bytes());
        assert!(refused(proposing(fixed, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 16:00")).contains("Ann doesn't take proposals"));
        assert!(outbox(&j).is_empty() && kept_answers(&j).is_empty(), "nothing sent, nothing kept");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A copy the user added with Add to calendar, here in the receiving
    /// account's own CalDAV calendar, is never the account's own copy: the
    /// new time goes as a COUNTER from the receiving account, and the copy
    /// quietly takes the answer that went with it, keeping its own time.
    #[tokio::test]
    async fn a_new_time_goes_by_mail_and_the_added_copy_follows() {
        use crate::calendar::sources::caldav::tests::caldav_server;
        crate::secrets::use_memory_store();
        crate::secrets::set_password("a1", "pw").unwrap();
        let (j, calendar) = jolta("respond-propose-added");
        let raw = mail!("google-request.eml");
        let message = import(&j, raw);
        let me = [crate::calendar::invitation::tests::ME.to_string()];
        let sent = invitation::find(raw, &me).remove(0);
        let uid = sent.event.uid.clone();
        let (_, at_server, server) = caldav_server(vec![("204 No Content", Some("\"2\""), None), ("204 No Content", Some("\"3\""), None)]).await;
        let text = invitation::calendar_text(raw, &uid).unwrap();
        let blob = j.blobs.put(text.as_bytes()).unwrap();
        let added = store::AddedCopy { calendar_id: calendar, remote_id: "copy.ics".into(), organizer: "ann@example.com".into(), written: Vec::new() };
        j.store
            .write(|tx| {
                tx.execute("UPDATE calendar SET remote_id = ?2 WHERE id = ?1", rusqlite::params![calendar, at_server.remote_id])?;
                store::set_caldav_url(tx, AccountId(1), Some(&at_server.remote_id.replace("/cal/", "/")))?;
                store::apply_item(tx, calendar, "copy.ics", Some("\"1\""), &[sent.event.clone()], Some(&blob), &series::Horizon::around(now(), tz::here()))?;
                store::remember_added(tx, AccountId(1), &uid, &added)
            })
            .unwrap();

        let friday = || proposing(message, Answer::Maybe, "2026-11-13 15:00", "2026-11-13 15:45");
        let done = answer_at(&j, &friday(), now()).await.unwrap();
        assert_eq!((done.mailed_from, done.calendar), (Some(AccountId(1)), Some(calendar)), "{}", done.text);
        assert!(done.text.ends_with(" with a maybe. Jolta emailed Ann."), "{}", done.text);
        let mailed = outbox(&j);
        assert_eq!(mailed.len(), 1);
        let (sender, recipients, _, counter) = &mailed[0];
        assert_eq!((sender.as_str(), recipients.as_str()), ("me@example.com", "ann@example.com"), "from the account the invitation came to");
        assert_eq!(counter_of(counter), ("COUNTER".into(), "20261113T150000Z".into(), "TENTATIVE".into()));

        let copy = j.store.read(|c| store::stored_event(c, &uid, None, None, tz::here())).unwrap().expect("the added copy");
        assert_eq!(copy.event.my_response(), Some(Response::Tentative), "the copy follows the answer");
        assert_eq!(copy.event.span, sent.event.span, "the proposal doesn't move the copy");

        answer_at(&j, &friday(), now()).await.unwrap();
        assert_eq!(outbox(&j).len(), 1, "the same proposal again is no second mail");
        assert_eq!(server.await.unwrap(), ["PUT /cal/copy.ics If-Match \"1\"", "PUT /cal/copy.ics If-Match \"2\""], "each time quietly, over the version Jolta holds");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Check 4: an answer given while the copy's server can't be reached is
    /// kept and shown at once and waits in the queue, using up no attempt
    /// while the network is gone; the same answer again adds nothing; back
    /// online it reaches the server once.
    #[tokio::test]
    async fn an_answer_given_offline_goes_once_back_online() {
        use crate::calendar::ops::{self, OpState};
        use crate::calendar::sources::caldav::tests::caldav_server;
        crate::secrets::use_memory_store();
        crate::secrets::set_password("a1", "pw").unwrap();
        let (j, calendar) = jolta("respond-offline");
        let raw = mail!("google-request.eml");
        let message = import(&j, raw);
        let me = [crate::calendar::invitation::tests::ME.to_string()];
        let sent = invitation::find(raw, &me).remove(0);
        let uid = sent.event.uid.clone();
        let blob = j.blobs.put(invitation::calendar_text(raw, &uid).unwrap().as_bytes()).unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let point_at = |port: u16| {
            j.store.write(|tx| {
                tx.execute("UPDATE calendar SET remote_id = ?2 WHERE id = ?1", rusqlite::params![calendar, format!("http://127.0.0.1:{port}/cal/")])?;
                store::set_caldav_url(tx, AccountId(1), Some(&format!("http://127.0.0.1:{port}/")))
            })
        };
        point_at(closed).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "invite.ics", Some("\"1\""), &[sent.event.clone()], Some(&blob), &series::Horizon::around(now(), tz::here())).map(|_| ())).unwrap();
        let ops_of = |j: &Jolta| j.store.read(|c| store::calendar_ops(c, 10)).unwrap();

        let done = answer_at(&j, &request(message, Answer::Accept, None, true, false), now()).await.unwrap();
        assert!(done.queued, "{}", done.text);
        assert_eq!(done.text, "Accepted. Jolta emailed Ann. It reaches your calendar as soon as Jolta reaches it.");
        assert_eq!(outbox(&j).len(), 1, "the reply mail waits in the outbox as ever");
        let queued = ops_of(&j);
        assert_eq!(queued.iter().map(|o| (o.write.kind(), o.state, o.attempts)).collect::<Vec<_>>(), [("rsvp", OpState::Pending, 0)]);
        let copy = j.store.read(|c| store::stored_event(c, &uid, None, None, tz::here())).unwrap().expect("the copy");
        assert_eq!(copy.event.my_response(), Some(Response::Accepted), "shown at once");
        let marked: bool = j.store.read(|c| Ok(c.query_row("SELECT changed_locally FROM event WHERE calendar_id = ?1", [calendar], |r| r.get(0))?)).unwrap();
        assert!(marked, "not synced yet");
        assert_eq!(card_answer(&j, message, raw), Some(Response::Accepted));

        let again = answer_at(&j, &request(message, Answer::Accept, None, true, false), now()).await.unwrap();
        assert!(again.queued, "the same answer again says it still waits: {}", again.text);
        assert_eq!((ops_of(&j).len(), outbox(&j).len()), (1, 1), "one op, one mail");

        let at = chrono::Utc::now().timestamp();
        assert!(ops::run_due(&j, series::Horizon::now()).await.unwrap().is_empty(), "nothing settled while offline");
        let waiting = &ops_of(&j)[0];
        assert_eq!((waiting.state, waiting.attempts), (OpState::Pending, 0), "a try that never reached the server isn't an attempt");
        assert!(waiting.send_after >= at + ops::OFFLINE_RETRY_SECONDS, "tried again in a minute");

        let (_, at_server, server) = caldav_server(vec![("204 No Content", Some("\"2\""), None)]).await;
        point_at(reqwest::Url::parse(&at_server.remote_id).unwrap().port().unwrap()).unwrap();
        j.store.write(|tx| Ok(tx.execute_batch("UPDATE calendar_op SET send_after = 0")?)).unwrap();
        let settled = ops::run_due(&j, series::Horizon::now()).await.unwrap();
        assert_eq!(settled.iter().map(|s| s.failure.is_none()).collect::<Vec<_>>(), [true]);
        assert_eq!(ops_of(&j)[0].state, OpState::Done);
        assert!(ops::run_due(&j, series::Horizon::now()).await.unwrap().is_empty(), "sent once");
        assert_eq!(server.await.unwrap(), ["PUT /cal/invite.ics If-Match \"1\""], "one quiet PUT over the version Jolta held");
        let marked: bool = j.store.read(|c| Ok(c.query_row("SELECT changed_locally FROM event WHERE calendar_id = ?1", [calendar], |r| r.get(0))?)).unwrap();
        assert!(!marked, "synced");
        let err = j.store.write(|tx| ops::undo(tx, queued[0].id, 0, &series::Horizon::now())).unwrap_err().to_string();
        assert!(err.contains("can't be undone"), "{err}");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A second, different answer given while the first still waits takes
    /// its place: only the newest answer goes.
    #[tokio::test]
    async fn a_newer_answer_replaces_the_one_waiting() {
        crate::secrets::use_memory_store();
        crate::secrets::set_password("a1", "pw").unwrap();
        let (j, calendar) = jolta("respond-offline-twice");
        let raw = mail!("google-request.eml");
        let message = import(&j, raw);
        let sent = invitation::find(raw, &[crate::calendar::invitation::tests::ME.to_string()]).remove(0);
        let blob = j.blobs.put(invitation::calendar_text(raw, &sent.event.uid).unwrap().as_bytes()).unwrap();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        j.store
            .write(|tx| {
                tx.execute("UPDATE calendar SET remote_id = ?2 WHERE id = ?1", rusqlite::params![calendar, format!("http://127.0.0.1:{closed}/cal/")])?;
                store::set_caldav_url(tx, AccountId(1), Some(&format!("http://127.0.0.1:{closed}/")))?;
                store::apply_item(tx, calendar, "invite.ics", Some("\"1\""), &[sent.event.clone()], Some(&blob), &series::Horizon::around(now(), tz::here())).map(|_| ())
            })
            .unwrap();
        for answer in [Answer::Accept, Answer::Decline] {
            assert!(answer_at(&j, &request(message, answer, None, true, false), now()).await.unwrap().queued);
        }
        let ops = j.store.read(|c| store::calendar_ops(c, 10)).unwrap();
        let answers: Vec<Option<Answer>> = ops.iter().map(|o| match &o.write { crate::calendar::ops::Write::Answer { answered, .. } => Some(answered.answer), _ => None }).collect();
        assert_eq!(answers, [Some(Answer::Decline)], "the decline alone waits");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A CalDAV calendar whose server can't be reached, holding `ics` as
    /// one resource: the rows' ids by recurrence id text, and the port.
    fn offline_copy(name: &str, ics: &str) -> (Jolta, crate::calendar::CalendarId, Vec<(String, EventId)>) {
        crate::secrets::use_memory_store();
        crate::secrets::set_password("a1", "pw").unwrap();
        let (j, calendar) = jolta(name);
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let events = crate::calendar::ical::read(ics, &[crate::calendar::invitation::tests::ME.to_string()]).unwrap();
        let blob = j.blobs.put(ics.as_bytes()).unwrap();
        let rows = j
            .store
            .write(|tx| {
                tx.execute("UPDATE calendar SET remote_id = ?2 WHERE id = ?1", rusqlite::params![calendar, format!("http://127.0.0.1:{closed}/cal/")])?;
                store::set_caldav_url(tx, AccountId(1), Some(&format!("http://127.0.0.1:{closed}/")))?;
                store::apply_item(tx, calendar, "occ.ics", Some("\"1\""), &events, Some(&blob), &series::Horizon::around(now(), tz::here()))?;
                let mut stmt = tx.prepare("SELECT recurrence_id, id FROM event ORDER BY recurrence_id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        (j, calendar, rows)
    }

    fn waiting(j: &Jolta) -> Vec<(Option<String>, Answer)> {
        j.store
            .read(|c| store::calendar_ops(c, 10))
            .unwrap()
            .into_iter()
            .filter_map(|o| match o.write {
                crate::calendar::ops::Write::Answer { answered, .. } => Some((o.recurrence_id.map(|r| store::moment_text(&r)), answered.answer)),
                _ => None,
            })
            .collect()
    }

    const ORGANIZED: &str = "ORGANIZER:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\n";

    /// An invitation to two occurrences without their series: an answer to
    /// the second, queued, stays the second's (check 2).
    #[tokio::test]
    async fn a_queued_answer_keeps_its_standalone_occurrence() {
        let ev = |rid: &str| format!("BEGIN:VEVENT\r\nUID:occ\r\nRECURRENCE-ID:{rid}\r\nDTSTART:{rid}\r\nDTEND:{}\r\nSUMMARY:Sync\r\n{ORGANIZED}END:VEVENT\r\n", rid.replace("T10", "T11"));
        let ics = format!("BEGIN:VCALENDAR\r\n{}{}END:VCALENDAR\r\n", ev("20261112T100000Z"), ev("20261119T100000Z"));
        let (j, _, rows) = offline_copy("respond-offline-alone", &ics);
        let second = rows.iter().find(|(rid, _)| rid.contains("1119")).unwrap().1;
        let rsvp = Rsvp { event: second, answer: Answer::Accept, note: None, notify: true, scope: Scope::Series, proposal: None };
        assert!(rsvp::answer_or_queue(&j, &rsvp).await.unwrap());
        let queued = waiting(&j);
        assert!(queued.len() == 1 && queued[0].0.as_deref().is_some_and(|r| r.contains("1119")), "{queued:?}");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// An answer to the whole series takes the place of one to an
    /// occurrence still waiting: the organizer hears only the newest.
    #[tokio::test]
    async fn a_series_answer_replaces_a_waiting_occurrence_answer() {
        let ics = format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:occ\r\nDTSTART:20261112T100000Z\r\nDTEND:20261112T110000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Sync\r\n{ORGANIZED}END:VEVENT\r\nEND:VCALENDAR\r\n");
        let (j, _, rows) = offline_copy("respond-offline-series", &ics);
        let master = rows[0].1;
        let at = |s: &str| Moment::Time(crate::calendar::Local { wall: chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: crate::calendar::Zone::Tz(chrono_tz::UTC) });
        let one = Rsvp { event: master, answer: Answer::Decline, note: None, notify: true, scope: Scope::Occurrence(at("2026-11-19 10:00")), proposal: None };
        assert!(rsvp::answer_or_queue(&j, &one).await.unwrap());
        let all = Rsvp { scope: Scope::Series, answer: Answer::Accept, ..one };
        assert!(rsvp::answer_or_queue(&j, &all).await.unwrap());
        assert_eq!(waiting(&j), [(None, Answer::Accept)], "the series answer alone");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
