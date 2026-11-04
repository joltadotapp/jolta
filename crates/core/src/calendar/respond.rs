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

use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension};

use super::card::{self, Card};
use super::rsvp::{self, Answer, ImipReply, Rsvp, Scope};
use super::{Bounds, Calendar, CalendarId, CalendarSource, EventData, EventId, Moment, Person, Response, invitation, tz};
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
}

/// What one answer does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Answer the calendar's copy through its provider; `Some(true)` lets
    /// the provider tell the organizer.
    pub provider: Option<bool>,
    /// Jolta sends the iMIP reply mail itself.
    pub mail: bool,
}

/// Who tells the organizer, from the source of the calendar's copy of the
/// event (None without one).
pub fn plan(copy: Option<CalendarSource>, notify: bool) -> Result<Plan> {
    match copy {
        Some(CalendarSource::Google | CalendarSource::Graph) => Ok(Plan { provider: Some(notify), mail: false }),
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
    /// The account whose outbox holds the reply mail; its engine sends it.
    pub mailed_from: Option<AccountId>,
    /// The calendar whose copy changed.
    pub calendar: Option<CalendarId>,
}

/// Whether an event's detail offers answers: an invitation to the user
/// from an organizer other than the calendar's own account, in a calendar
/// Jolta answers through.
pub fn can_answer(account: &str, calendar: &Calendar, e: &EventData) -> bool {
    let theirs = e.organizer.as_ref().is_some_and(|o| !o.address.trim().eq_ignore_ascii_case(account));
    theirs && e.my_response().is_some() && plan(Some(calendar.source), true).is_ok_and(|p| p.provider.is_some())
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
    let r = match &req.subject {
        Subject::Message(id) => from_message(jolta, *id, req.occurrence_only, now, here).await?,
        Subject::Event { id, at } => {
            let (id, at, only) = (*id, *at, req.occurrence_only);
            let mut r = jolta.store.read_async(move |c| from_event(c, id, at.filter(|_| only), here)).await?;
            let blob = jolta.store.read_async(move |c| Ok(c.query_row("SELECT blob FROM event WHERE id = ?1", [id], |r| r.get::<_, Option<String>>(0))?)).await?;
            r.text = blob.map(|hash| jolta.blobs.get(&hash)).transpose()?.map(|raw| String::from_utf8_lossy(&raw).into_owned());
            r
        }
    };
    let plan = plan(r.copy.map(|c| c.source), req.notify)?;
    let rid = r.occurrence.as_ref().map(|(rid, _)| *rid);

    let (account, uid) = (r.account, r.uid.clone());
    let given = store::GivenAnswer { answer: req.answer.response(), note: req.note.clone().unwrap_or_default(), notify: req.notify };
    let before = jolta.store.read_async(move |c| store::given_answer(c, account, &uid, rid.as_ref())).await?;

    let mail = if plan.mail {
        let text = r.text.as_deref().ok_or_else(|| Error::Other("Jolta has no copy of this invitation to reply to.".into()))?;
        let reply = ImipReply { invitation: text, from: &r.from, me: &r.me, answer: req.answer, note: req.note.as_deref(), occurrence: rid };
        Some(rsvp::imip_reply(&reply, here)?)
    } else {
        None
    };
    if let (Some(notify), Some(copy)) = (plan.provider, r.copy)
        && !again(&given, r.shown, before.as_ref())
    {
        let scope = rid.map_or(Scope::Series, Scope::Occurrence);
        rsvp::answer(jolta, &Rsvp { event: copy.event, answer: req.answer, note: req.note.clone(), notify, scope }).await?;
    }

    let blob = mail.as_ref().map(|m| jolta.blobs.put(&m.raw)).transpose()?;
    let uid = r.uid.clone();
    let shown = r.shown;
    jolta
        .store
        .write_async(move |tx| {
            // Read again where the outbox can't change underneath.
            let before = store::given_answer(tx, account, &uid, rid.as_ref())?;
            let again = again(&given, shown, before.as_ref());
            let reply = match before.and_then(|(_, reply)| reply) {
                Some(store::ReplyMail::Waiting(id)) if again => {
                    store::send_now(tx, id)?;
                    Some(id)
                }
                Some(store::ReplyMail::Sent(id)) if again => Some(id),
                waiting => {
                    if let Some(store::ReplyMail::Waiting(id)) = waiting {
                        store::cancel_send(tx, id)?;
                    }
                    match (&mail, &blob) {
                        (Some(m), Some(blob)) => Some(queue(tx, account, m, blob)?),
                        _ => None,
                    }
                }
            };
            store::remember_answer(tx, account, &uid, rid.as_ref(), &given, reply)
        })
        .await?;
    let calendar = r.copy.filter(|_| plan.provider.is_some()).map(|c| c.calendar);
    Ok(Outcome { text: told(req, &r, plan), mailed_from: plan.mail.then_some(r.account), calendar })
}

/// The answer kept, given again the same way, to an invitation that still
/// shows it: an answer changed elsewhere since isn't the same.
fn again(given: &store::GivenAnswer, shown: Option<Response>, before: Option<&(store::GivenAnswer, Option<store::ReplyMail>)>) -> bool {
    shown == Some(given.answer) && before.is_some_and(|(b, _)| b == given)
}

/// The reply goes out at once: there is no undo for an answer yet.
fn queue(conn: &Connection, account: AccountId, m: &Built, blob: &str) -> Result<i64> {
    store::queue_outbox(conn, account, None, blob, &m.message_id, &m.from, &m.recipients, None, 0)
}

/// "Accepted. Google told Ann.", "Declined Thu 5 Nov. Jolta emailed Ann.",
/// "Answered maybe without telling Ann."
fn told(req: &Request, r: &Resolved, plan: Plan) -> String {
    let mut done = match req.answer {
        Answer::Accept => "Accepted".to_string(),
        Answer::Maybe => "Answered maybe".to_string(),
        Answer::Decline => "Declined".to_string(),
    };
    if let Some((_, day)) = &r.occurrence {
        done = format!("{done} for {day}");
    }
    let who = if r.organizer.is_empty() { "the organizer" } else { &r.organizer };
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
    jolta
        .store
        .read_async(move |c| {
            let every: Vec<String> = store::accounts(c)?.into_iter().map(|a| a.address).collect();
            let from = store::account(c, account)?.address;
            let invitations = invitation::find(&raw, &every);
            let cx = card::Context { message, account, me: &every, sender: &sender, now, here };
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
            // to: another account's may be the organizer's own.
            let copy = match store::stored_event(c, &a.uid, None, Some(account), here)? {
                None => sent
                    .and_then(|i| i.event.recurrence_id)
                    .map(|rid| store::stored_event(c, &a.uid, Some(&rid), Some(account), here))
                    .transpose()?
                    .flatten(),
                found => found,
            };
            let me = addresses(&from, copy.as_ref().map(|s| &s.event));
            let copy = copy
                .map(|s| Ok::<_, Error>(Copy { event: s.event_id, calendar: s.calendar_id, source: store::calendar(c, s.calendar_id)?.source }))
                .transpose()?;
            Ok(Resolved {
                account,
                from,
                text: invitation::calendar_text(&raw, &a.uid),
                uid: a.uid,
                me,
                copy,
                occurrence: a.occurrence.filter(|o| o.alone || occurrence_only).map(|o| (o.recurrence_id, o.day)),
                organizer: a.organizer,
                shown,
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
    let me = addresses(&from, Some(&data));
    let repeats = data.recurrence.is_some() || data.recurrence_id.is_some();
    let occurrence = match at.filter(|_| repeats) {
        Some(at) => {
            let rid = rsvp::occurrence_at(conn, event, at, here)?;
            Some((rid, day(&rid, here)))
        }
        None => None,
    };
    Ok(Resolved {
        account,
        from,
        me,
        copy: Some(Copy { event, calendar: calendar.id, source: calendar.source }),
        text: None,
        occurrence,
        organizer: data.organizer.as_ref().map(short_name).unwrap_or_default(),
        shown: data.my_response(),
        uid: data.uid,
    })
}

/// The account's address, then the aliases its calendar's copy of the
/// event marks as the user's: each sync marks only its own account's.
fn addresses(account: &str, copy: Option<&EventData>) -> Vec<String> {
    let aliases = copy.into_iter().flat_map(|e| &e.attendees).filter(|a| a.is_me).map(|a| &a.person.address);
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
mod tests {
    use chrono::TimeZone;
    use mail_parser::MessageParser;

    use super::*;
    use crate::calendar::Response;
    use crate::calendar::invitation::tests::{import, jolta, mail};

    #[test]
    fn who_tells_the_organizer() {
        use CalendarSource::*;
        let row = |copy, notify| plan(copy, notify).map(|p| (p.provider, p.mail)).map_err(|e| e.to_string());
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

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    fn request(message: MessageId, answer: Answer, note: Option<&str>, notify: bool, occurrence_only: bool) -> Request {
        Request { subject: Subject::Message(message), answer, note: note.map(str::to_string), notify, occurrence_only }
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
        let cx = card::Context { message, account: AccountId(1), me: &me, sender: "ann@example.com", now: now(), here: tz::here() };
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
        assert_eq!(done, Outcome { text: "Answered maybe. Jolta emailed Ann.".into(), mailed_from: Some(AccountId(1)), calendar: None });

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
        assert_eq!(done, Outcome { text: "Declined without telling Ann.".into(), mailed_from: None, calendar: None });
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
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The account's copy, synced with the aliases its server knows, lets
    /// the reply answer as an alias of that account.
    #[test]
    fn the_reply_may_answer_as_an_alias_the_accounts_copy_knows() {
        let raw = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:alias\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\n\
                   ORGANIZER:mailto:ann@example.com\r\nATTENDEE:mailto:me2@example.net\r\nATTENDEE:mailto:alias@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let copy = crate::calendar::ical::read_invitation(raw, &["alias@example.com".to_string()]).unwrap().remove(0).0;
        let me = addresses("me@example.com", Some(&copy));
        assert_eq!(me, ["me@example.com", "alias@example.com"], "the other account isn't one of this account's addresses");
        let reply = rsvp::imip_reply(&ImipReply { invitation: raw, from: "me@example.com", me: &me, answer: Answer::Accept, note: None, occurrence: None }, tz::here()).unwrap();
        let ics = reply_ics(&reply.raw);
        assert!(ics.contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:alias@example.com") && !ics.contains("me2@"), "{ics}");
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
}
