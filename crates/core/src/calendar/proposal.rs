//! The organizer's side of a guest's proposal for a new time. Decline
//! proposal mails the guest an iMIP DECLINECOUNTER from the account the
//! proposal came to, once. Accept proposal opens the editor on the
//! meeting in the account's own calendar at the time proposed; saving it
//! sends the update through Send an update? like any change of time.

use chrono::Utc;
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension};

use super::card::{self, Card, Proposed};
use super::draft::{self, EventDraft};
use super::{Bounds, Span, invitation, rsvp, series, tz};
use crate::model::{AccountId, MessageId};
use crate::{Error, Jolta, Result, imap, store};

/// What the card checked of a message's proposal, with the account it came to.
struct Checked {
    account: AccountId,
    /// The account's address.
    from: String,
    raw: Vec<u8>,
    proposal: Proposed,
    title: String,
}

/// The proposal on a message's card, as the card offers it.
async fn checked(jolta: &Jolta, message: MessageId) -> Result<Checked> {
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
    let found = raw.clone();
    jolta
        .store
        .read_async(move |c| {
            let every: Vec<String> = store::accounts(c)?.into_iter().map(|a| a.address).collect();
            let from = store::account(c, account)?.address;
            let invitations = invitation::find(&found, &every);
            let cx = card::Context { message, account, me: &every, sender: &sender, now: Utc::now(), here: tz::here(), blobs: &blobs };
            let (proposal, title) = card::cards(c, &invitations, &cx)?
                .into_iter()
                .find_map(|card| match card {
                    Card::Event(e) => e.proposal.map(|p| (p, e.title)),
                    Card::Reply(_) => None,
                })
                .ok_or_else(|| Error::Other("This message has no proposal for a new time.".into()))?;
            Ok(Checked { account, from, raw: found, proposal, title })
        })
        .await
}

/// The editor's draft for accepting a guest's proposal.
#[derive(Clone, Debug, PartialEq)]
pub struct Accepting {
    /// The meeting in the account's own calendar at the time proposed: for
    /// a proposal for one occurrence, that occurrence, which the scope
    /// prompt saves for This event first.
    pub draft: EventDraft,
    /// Above the fields: "Bo Nilsson proposed this time: \"Thursday is full\"".
    pub header: String,
}

/// Accept proposal on a message's card: the organizer's own copy of the
/// meeting (check 1) in the editor at the time the guest proposed, for the
/// occurrence a COUNTER with a RECURRENCE-ID names (check 2), offered only
/// when the card counts the proposal as a guest's (check 3). Nothing is
/// saved here. A proposal already accepted isn't offered again (check 4).
pub async fn accept_draft(jolta: &Jolta, message: MessageId) -> Result<Accepting> {
    let Checked { account, proposal, .. } = checked(jolta, message).await?;
    let first = first_name(&proposal);
    if proposal.accepted {
        return Err(Error::Other(format!("{first}'s proposal is accepted already: the meeting is at that time.")));
    }
    if proposal.declined {
        return Err(Error::Other(format!("{first}'s proposal was declined already.")));
    }
    if proposal.accepting.is_none() {
        return Err(Error::Other(match proposal.declining {
            Some(_) => "Jolta can't find this meeting in this account's own calendar, where accepting the proposal changes it.".into(),
            None => "Only the meeting's organizer can accept this proposal.".into(),
        }));
    }
    let here = tz::here();
    jolta.store.read_async(move |c| accepted_draft(c, account, &proposal, here)).await
}

/// The draft of `proposal`'s meeting in the account's own calendar, moved
/// to the time proposed.
fn accepted_draft(conn: &Connection, account: AccountId, proposal: &Proposed, here: Tz) -> Result<Accepting> {
    let receiver = store::account(conn, account)?.address;
    let own = |c: &super::Calendar| c.is_own(&receiver);
    let missing = || Error::Other("The meeting is no longer in your calendar.".into());
    let master = store::stored_event_in(conn, &proposal.uid, None, Some(account), here, own)?.ok_or_else(missing)?;
    let (event, at) = match &proposal.recurrence_id {
        None => (master.event_id, None),
        Some(rid) => match store::stored_event_in(conn, &proposal.uid, Some(rid), Some(account), here, own)? {
            Some(changed) => (changed.event_id, None),
            None => {
                let occurrence = series::occurrence(std::slice::from_ref(&master.event), rid, here).ok_or_else(missing)?;
                (master.event_id, Some(occurrence.span.bounds(here)))
            }
        },
    };
    let mut d = draft::draft_for_event(conn, event, at, here)?;
    if d.locks.contains(&draft::DraftField::Time) {
        return Err(Error::Other(d.notice.unwrap_or_else(|| "The meeting's time can't change here.".into())));
    }
    let zone = match d.span {
        Span::Timed { start, .. } => start.zone,
        Span::AllDay { .. } => super::Zone::Tz(here),
    };
    d.span = match proposal.time {
        Some(Bounds::Utc { start, end }) => draft::timed_span(start.timestamp(), end.timestamp(), zone, here),
        Some(Bounds::Days { .. }) | None => return Err(Error::Other("Jolta accepts proposals with a start and an end time; change this meeting's days in the editor.".into())),
    };
    let said = format!("{} proposed this time", proposal.proposer);
    let header = if proposal.note.is_empty() { said } else { format!("{said}: \"{}\"", proposal.note) };
    Ok(Accepting { draft: d, header })
}

fn first_name(p: &Proposed) -> String {
    p.proposer.split_whitespace().next().unwrap_or("the guest").to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declined {
    /// For the toast: "Declined Bo's proposal. Jolta emailed Bo."
    pub text: String,
    /// The account whose outbox holds a mail to send; None when it went
    /// already.
    pub mailed_from: Option<AccountId>,
}

/// Declines the proposal on a message's card, when the card offers it: a
/// guest's proposal for a meeting the receiving account organizes. The no
/// goes to the proposer only and answers the proposal's message, so the
/// outbox keeps it as that message's answer. Declining again sends nothing
/// new: a no still waiting goes now.
pub async fn decline(jolta: &Jolta, message: MessageId) -> Result<Declined> {
    let Checked { account, from, raw, proposal, title } = checked(jolta, message).await.map_err(|e| match e {
        Error::Other(t) if t.starts_with("This message has no proposal") => Error::Other("This message has no proposal to decline.".into()),
        e => e,
    })?;
    if proposal.accepted {
        return Err(Error::Other(format!("{}'s proposal is accepted already: the meeting is at that time.", first_name(&proposal))));
    }
    if proposal.declining.is_none() && !proposal.declined {
        return Err(Error::Other("Only the meeting's organizer can decline this proposal.".into()));
    }
    if proposal.address.is_empty() {
        return Err(Error::Other("The proposal names no guest.".into()));
    }
    // The proposal the card checked, never another one in the mail.
    let here = tz::here();
    let rid = proposal.recurrence_id.as_ref();
    let text = invitation::part_text(&raw, invitation::Method::Counter, &proposal.uid, rid, here)
        .ok_or_else(|| Error::Other("Jolta has no copy of this proposal to answer.".into()))?;
    let built = rsvp::imip_decline_counter(&text, &proposal.uid, rid, &from, &proposal.address, &title, here)?;

    let blob = jolta.blobs.put(&built.raw)?;
    let waiting = jolta
        .store
        .write_async(move |tx| match store::calendar_answer_to(tx, message)? {
            Some(store::ReplyMail::Waiting(id)) => store::send_now(tx, id).map(|_| true),
            Some(store::ReplyMail::Sent(_)) => Ok(false),
            None => store::queue_outbox(tx, account, None, &blob, &built.message_id, &from, &built.recipients, Some(message), 0).map(|_| true),
        })
        .await?;
    let first = first_name(&proposal);
    let text = if proposal.declined { format!("{first}'s proposal was declined already.") } else { format!("Declined {first}'s proposal. Jolta emailed {first}.") };
    Ok(Declined { text, mailed_from: waiting.then_some(account) })
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::card::{EventCard, Proposed};
    use crate::calendar::invitation::tests::{ME, import, jolta};
    use crate::calendar::series::Horizon;
    use crate::calendar::{CalendarId, EventData};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    /// The user's own meeting on Thursday 12 November, 16:00 to 17:00 here,
    /// with Bo as its guest, in the account's calendar.
    fn my_meeting(j: &Jolta, calendar: CalendarId) {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Planning\r\n\
                   ORGANIZER;CN=Me:mailto:me@example.com\r\nATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events: Vec<EventData> = crate::calendar::ical::read(ics, &[ME.to_string()]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "plan.ics", None, &events, None, &Horizon::around(now(), Brussels))).unwrap();
    }

    /// A proposal mail From `from`, naming `organizer` and the guest `guest`.
    fn counter(id: &str, from: &str, organizer: &str, guest: &str, start: &str) -> String {
        format!(
            "From: {from}\r\nTo: {organizer}\r\nSubject: New time proposed: Planning\r\nMessage-ID: <{id}@example.com>\r\n\
             Content-Type: text/calendar; method=COUNTER\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:COUNTER\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\n\
             DTSTAMP:20261104T100000Z\r\nDTSTART:{start}\r\nDTEND:{}\r\nSUMMARY:Planning\r\nORGANIZER;CN=Me:mailto:{organizer}\r\n\
             ATTENDEE;PARTSTAT=TENTATIVE;CN=Bo Nilsson:mailto:{guest}\r\nCOMMENT:Thursday is full\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            start.replace("T14", "T15")
        )
    }

    fn card_of(j: &Jolta, message: MessageId, raw: &str) -> EventCard {
        let me = [ME.to_string()];
        let found = invitation::find(raw.as_bytes(), &me);
        let sender = raw.lines().next().unwrap().trim_start_matches("From: ").to_string();
        let cx = card::Context { message, account: AccountId(1), me: &me, sender: &sender, now: now(), here: Brussels, blobs: &j.blobs };
        match j.store.read(|c| card::cards(c, &found, &cx)).unwrap().remove(0) {
            Card::Event(e) => e,
            Card::Reply(_) => panic!("an event card"),
        }
    }

    fn notes(c: &EventCard) -> Vec<&str> {
        c.notes.iter().map(|n| n.text.as_str()).collect()
    }

    fn run<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// (sender, recipients, replying to, METHOD) of each outbox row.
    fn outbox(j: &Jolta) -> Vec<(String, String, Option<i64>, String)> {
        let rows: Vec<(String, String, Option<i64>, String)> = j
            .store
            .read(|c| {
                let mut stmt = c.prepare("SELECT sender, recipients, replying_to, blob FROM outbox ORDER BY id")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap();
        rows.into_iter()
            .map(|(s, r, to, blob)| {
                let raw = j.blobs.get(&blob).unwrap();
                let msg = mail_parser::MessageParser::default().parse(&raw).unwrap();
                let method = msg.parts().find(|p| p.is_content_type("text", "calendar")).and_then(|p| p.content_type()?.attribute("method").map(str::to_string));
                (s, r, to, method.unwrap_or_default())
            })
            .collect()
    }

    #[test]
    fn a_guests_proposal_shows_both_times_and_is_declined_once() {
        let (j, calendar) = jolta("proposal-decline");
        my_meeting(&j, calendar);
        let busy = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:dentist\r\nDTSTART:20261113T143000Z\r\nDTEND:20261113T153000Z\r\nSUMMARY:Dentist\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let busy = crate::calendar::ical::read(busy, &[]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "dentist.ics", None, &busy, None, &Horizon::around(now(), Brussels))).unwrap();

        let raw = counter("c1", "bo@example.com", ME, "bo@example.com", "20261113T140000Z");
        let message = import(&j, raw.as_bytes());
        let c = card_of(&j, message, &raw);
        assert_eq!((c.label, c.when.as_deref()), ("New time proposed", Some("Fri 13 Nov · 15:00 to 16:00")), "the time proposed");
        assert_eq!(notes(&c), ["Bo Nilsson proposed this new time: \"Thursday is full\""]);
        assert_eq!(
            c.proposal,
            Some(Proposed {
                now: Some("Thu 12 Nov · 16:00 to 17:00".into()),
                proposer: "Bo Nilsson".into(),
                note: "Thursday is full".into(),
                time: Some(Bounds::Utc { start: at("2026-11-13 14:00"), end: at("2026-11-13 15:00") }),
                accepting: Some(message),
                accepted: false,
                declining: Some(message),
                declined: false,
                uid: "plan".into(),
                recurrence_id: None,
                address: "bo@example.com".into()
            }),
            "set against the meeting's time now"
        );
        let a = c.availability.expect("the proposed time's conflicts");
        assert_eq!((a.text.as_str(), a.conflict), ("Overlaps with Dentist", true));
        assert!(a.strip.is_some_and(|s| s.busy.iter().all(|b| b.title == "Dentist")), "the meeting itself isn't its own conflict");

        let done = run(decline(&j, message)).unwrap();
        assert_eq!(done, Declined { text: "Declined Bo's proposal. Jolta emailed Bo.".into(), mailed_from: Some(AccountId(1)) });
        assert_eq!(outbox(&j), [(ME.to_string(), "bo@example.com".to_string(), Some(message.0), "DECLINECOUNTER".to_string())], "from the organizer to the proposer");
        let c = card_of(&j, message, &raw);
        assert_eq!(c.proposal.as_ref().map(|p| (p.accepting, p.declining, p.declined)), Some((None, None, true)), "the card says it was handled");
        assert_eq!(c.availability, None, "a declined time needs no conflicts");

        assert_eq!(run(decline(&j, message)).unwrap().text, "Bo's proposal was declined already.");
        j.store.write(|tx| Ok(tx.execute_batch("UPDATE outbox SET state = 'sent'")?)).unwrap();
        assert_eq!(run(decline(&j, message)).unwrap().mailed_from, None, "nothing left to send");
        assert_eq!(outbox(&j).len(), 1, "declined twice, one mail");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_proposal_counts_only_from_a_guest_of_the_users_meeting() {
        let (j, calendar) = jolta("proposal-strangers");
        let shown = |id: &str, from: &str, organizer: &str, guest: &str| {
            let raw = counter(id, from, organizer, guest, "20261113T140000Z");
            let c = card_of(&j, import(&j, raw.as_bytes()), &raw);
            (c.proposal.is_some(), notes(&c).iter().filter(|n| n.contains("isn't") || n.contains("no copy")).map(|n| n.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(shown("u0", "bo@example.com", ME, "bo@example.com"), (false, vec!["Jolta has no copy of this meeting to check this proposal against".to_string()]));

        my_meeting(&j, calendar);
        assert_eq!(shown("g1", "bo@example.com", ME, "bo@example.com"), (true, vec![]), "Bo is a guest and sent it");
        let not_a_guest = (false, vec!["This isn't from one of the meeting's guests".to_string()]);
        assert_eq!(shown("s1", "eve@example.com", ME, "bo@example.com"), not_a_guest, "Eve sends a proposal in Bo's name");
        assert_eq!(shown("s2", "eve@example.com", ME, "eve@example.com"), not_a_guest, "Eve isn't invited");
        assert_eq!(shown("s3", "bo@example.com", "ann@example.com", "bo@example.com"), not_a_guest, "another organizer's meeting with the same UID");

        let stranger = counter("s4", "eve@example.com", ME, "eve@example.com", "20261113T140000Z");
        let m = import(&j, stranger.as_bytes());
        assert!(run(decline(&j, m)).unwrap_err().to_string().contains("no proposal"));
        assert!(outbox(&j).is_empty());
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// The user proposed a new time to Ann's meeting; only Ann's no counts.
    #[test]
    fn a_declinecounter_counts_only_from_the_organizer() {
        let (j, _) = jolta("proposal-declined");
        let request = "From: ann@example.com\r\nTo: me@example.com\r\nSubject: Planning\r\nMessage-ID: <r1@example.com>\r\n\
                       Content-Type: text/calendar; method=REQUEST\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:annplan\r\nSEQUENCE:0\r\n\
                       DTSTAMP:20261104T080000Z\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Planning\r\n\
                       ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        import(&j, request.as_bytes());
        let no = |id: &str, from: &str| {
            format!(
                "From: {from}\r\nTo: me@example.com\r\nSubject: New time declined\r\nMessage-ID: <{id}@example.com>\r\n\
                 Content-Type: text/calendar; method=DECLINECOUNTER\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:DECLINECOUNTER\r\nBEGIN:VEVENT\r\nUID:annplan\r\n\
                 SEQUENCE:0\r\nDTSTAMP:20261104T110000Z\r\nORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            )
        };
        let theirs = no("d1", "ann@example.com");
        let c = card_of(&j, import(&j, theirs.as_bytes()), &theirs);
        assert_eq!((c.label, notes(&c)[0]), ("Proposal declined", "The organizer kept the original time"));
        assert!(!notes(&c).contains(&"This isn't from the meeting's organizer"), "{:?}", notes(&c));
        let forged = no("d2", "eve@example.com");
        let c = card_of(&j, import(&j, forged.as_bytes()), &forged);
        assert!(notes(&c).contains(&"This isn't from the meeting's organizer"), "{:?}", notes(&c));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Bo's mail holds a COUNTER naming Eve before Bo's own: for a meeting
    /// the account doesn't know, or for an occurrence of Bo's meeting, as an
    /// event of the same calendar part or as a calendar part of its own. The
    /// no answers the proposal the card checked, Bo's.
    #[test]
    fn decline_proposal_answers_the_proposal_the_card_checked() {
        let (j, calendar) = jolta("proposal-checked");
        my_meeting(&j, calendar);
        let event = |uid: &str, guest: &str| {
            format!(
                "BEGIN:VEVENT\r\nUID:{uid}\r\nSEQUENCE:1\r\nDTSTAMP:20261104T100000Z\r\nDTSTART:20261113T140000Z\r\nDTEND:20261113T150000Z\r\n\
                 SUMMARY:Planning\r\nORGANIZER;CN=Me:mailto:me@example.com\r\nATTENDEE;PARTSTAT=TENTATIVE:mailto:{guest}\r\nEND:VEVENT\r\n"
            )
        };
        let counter = |events: &str| format!("BEGIN:VCALENDAR\r\nMETHOD:COUNTER\r\n{events}END:VCALENDAR\r\n");
        let head = |id: &str, kind: &str| {
            format!("From: bo@example.com\r\nTo: me@example.com\r\nSubject: New time proposed: Planning\r\nMessage-ID: <{id}@example.com>\r\nMIME-Version: 1.0\r\nContent-Type: {kind}\r\n\r\n")
        };
        let part = |ics: String| format!("--b\r\nContent-Type: text/calendar; method=COUNTER\r\n\r\n{ics}\r\n");
        let bos = event("plan", "bo@example.com");
        let elsewhere = event("other", "eve@example.com");
        let occurrence = event("plan", "eve@example.com").replace("SEQUENCE:1\r\n", "SEQUENCE:1\r\nRECURRENCE-ID:20261112T150000Z\r\n");
        for (id, eves) in [("other", elsewhere), ("occurrence", occurrence)] {
            let one_part = format!("{}{}", head(&format!("{id}-1"), "text/calendar; method=COUNTER"), counter(&format!("{eves}{bos}")));
            let two_parts = format!("{}{}{}--b--\r\n", head(&format!("{id}-2"), "multipart/mixed; boundary=b"), part(counter(&eves)), part(counter(&bos)));
            for raw in [one_part, two_parts] {
                let message = import(&j, raw.as_bytes());
                run(decline(&j, message)).unwrap();
            }
        }
        let sent: Vec<(String, bool)> = j
            .store
            .read(|c| {
                let mut stmt = c.prepare("SELECT recipients, blob FROM outbox ORDER BY id")?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
            .into_iter()
            .map(|(to, blob)| {
                let raw = String::from_utf8_lossy(&j.blobs.get(&blob).unwrap()).into_owned();
                (to, raw.contains("UID:plan") && !raw.contains("RECURRENCE-ID"))
            })
            .collect();
        assert_eq!(sent, vec![("bo@example.com".to_string(), true); 4], "to Bo, about Bo's proposal");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    fn save(j: &Jolta, d: &EventDraft) -> Option<crate::calendar::ops::CalendarOpId> {
        let horizon = Horizon::around(now(), Brussels);
        j.store.write(|tx| draft::save_draft(tx, d, None, Some(crate::calendar::ops::Notify::All), None, now().timestamp(), &horizon, Brussels)).unwrap().op
    }

    fn at(s: &str) -> DateTime<Utc> {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc()
    }

    /// Check 4: Accept proposal opens the meeting at the proposed time and
    /// saving sends one update; the card then reads Accepted and offers
    /// nothing, also once the server's newer version is back, so accepting
    /// again sends nothing. Undo offers the proposal again.
    #[test]
    fn a_proposal_is_accepted_through_the_editor_once() {
        let (j, calendar) = jolta("proposal-accept");
        my_meeting(&j, calendar);
        let raw = counter("a1", "bo@example.com", ME, "bo@example.com", "20261113T140000Z");
        let message = import(&j, raw.as_bytes());
        let p = card_of(&j, message, &raw).proposal.expect("Bo's proposal");
        assert_eq!((p.accepting, p.declining, p.accepted), (Some(message), Some(message), false));
        assert_eq!(p.note, "Thursday is full");

        let a = run(accept_draft(&j, message)).unwrap();
        assert_eq!(a.header, "Bo Nilsson proposed this time: \"Thursday is full\"");
        assert_eq!((a.draft.calendar, a.draft.uid.as_str()), (calendar, "plan"), "the organizer's own copy");
        assert_eq!(a.draft.span.bounds(Brussels), Bounds::Utc { start: at("2026-11-13 14:00"), end: at("2026-11-13 15:00") });
        let plan = j.store.read(|c| draft::plan_save(c, &a.draft, Brussels)).unwrap();
        assert!(plan.dialog.is_some() && plan.scopes.is_empty(), "Send an update? for Bo, no scope for a single meeting");
        let op = save(&j, &a.draft).expect("one update queued");

        let c = card_of(&j, message, &raw);
        let p = c.proposal.clone().expect("still shown");
        assert_eq!((p.accepting, p.declining, p.accepted), (None, None, true), "accepted: no buttons");
        assert!(run(accept_draft(&j, message)).unwrap_err().to_string().contains("accepted already"));
        assert!(run(decline(&j, message)).unwrap_err().to_string().contains("accepted already"));
        assert!(outbox(&j).is_empty(), "no DECLINECOUNTER after accepting");

        let horizon = Horizon::around(now(), Brussels);
        j.store.write(|tx| crate::calendar::ops::undo(tx, op, now().timestamp(), &horizon)).unwrap();
        assert_eq!(card_of(&j, message, &raw).proposal.and_then(|p| p.accepting), Some(message), "Undo puts the meeting back and offers the proposal again");
        save(&j, &run(accept_draft(&j, message)).unwrap().draft).expect("accepted again");

        // The server's version after the update: a higher SEQUENCE at the new time.
        let mut moved = j.store.read(|c| store::stored_series(c, "plan", None)).unwrap().remove(0);
        moved.sequence = 2;
        j.store.write(|tx| tx.execute_batch("DELETE FROM calendar_op").map_err(Into::into)).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "plan.ics", Some("\"2\""), &[moved], None, &horizon)).unwrap();
        let c = card_of(&j, message, &raw);
        assert!(c.proposal.as_ref().is_some_and(|p| p.accepted), "{:?}", c.proposal);
        assert!(!notes(&c).iter().any(|n| n.contains("newer version")), "{:?}", notes(&c));
        let ops = j.store.read(|c| store::calendar_ops(c, 10)).unwrap();
        assert!(ops.is_empty(), "nothing more queued");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Check 2: a proposal for one occurrence opens that occurrence, where
    /// it is now, and the scope prompt starts at This event.
    #[test]
    fn a_proposal_for_an_occurrence_edits_that_occurrence() {
        use crate::calendar::ops::Scope;
        let (j, calendar) = jolta("proposal-accept-occurrence");
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\nDTSTART;TZID=Europe/Brussels:20261110T100000\r\n\
                   DTEND;TZID=Europe/Brussels:20261110T103000\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Planning\r\nORGANIZER;CN=Me:mailto:me@example.com\r\n\
                   ATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\n\
                   RECURRENCE-ID;TZID=Europe/Brussels:20261117T100000\r\nDTSTART;TZID=Europe/Brussels:20261118T140000\r\n\
                   DTEND;TZID=Europe/Brussels:20261118T143000\r\nSUMMARY:Planning\r\nORGANIZER;CN=Me:mailto:me@example.com\r\n\
                   ATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = crate::calendar::ical::read(ics, &[ME.to_string()]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "plan.ics", None, &events, None, &Horizon::around(now(), Brussels))).unwrap();
        let accept = |id: &str, rid: &str| {
            let raw = counter(id, "bo@example.com", ME, "bo@example.com", "20261124T140000Z")
                .replace("SEQUENCE:1\r\n", &format!("SEQUENCE:1\r\nRECURRENCE-ID:{rid}\r\n"));
            run(accept_draft(&j, import(&j, raw.as_bytes()))).unwrap().draft
        };
        let d = accept("o1", "20261124T090000Z");
        let base = d.base.as_ref().unwrap();
        let tuesday = crate::calendar::Moment::Time(crate::calendar::Local { wall: at("2026-11-24 10:00").naive_utc(), zone: crate::calendar::Zone::Tz(Brussels) });
        assert!(base.recurrence_id.is_some_and(|r| r.names(&tuesday, Brussels)), "the unchanged occurrence of 24 Nov: {:?}", base.recurrence_id);
        assert_eq!(d.span.bounds(Brussels), Bounds::Utc { start: at("2026-11-24 14:00"), end: at("2026-11-24 15:00") });
        let plan = j.store.read(|c| draft::plan_save(c, &d, Brussels)).unwrap();
        assert_eq!(plan.scopes.first().map(|s| s.scope), Some(Scope::This), "This event first");

        let d = accept("o2", "20261117T090000Z");
        assert_eq!(d.base.as_ref().map(|b| b.span.bounds(Brussels)), Some(Bounds::Utc { start: at("2026-11-18 13:00"), end: at("2026-11-18 13:30") }), "the moved one, where it is now");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Check 1: a meeting the account organizes, held only in a calendar
    /// shared with it, isn't the user's to change: Decline proposal stays,
    /// Accept proposal doesn't show.
    #[test]
    fn accept_needs_the_meeting_in_the_accounts_own_calendar() {
        let (j, _) = jolta("proposal-accept-shared");
        let shared = j
            .store
            .write(|tx| {
                let remote = store::RemoteCalendar { remote_id: "/cal/team/".into(), name: "Team".into(), color: None, zone: None, access: crate::calendar::Access::Write, primary: false, refresh: None, video: None };
                let cals = store::sync_calendar_list(tx, AccountId(1), crate::calendar::CalendarSource::CalDav, &[remote])?;
                let team = cals.iter().find(|c| c.name == "Team").unwrap().id;
                tx.execute("UPDATE calendar SET shown = 1 WHERE id = ?1", [team])?;
                tx.execute("DELETE FROM event", [])?;
                Ok(team)
            })
            .unwrap();
        my_meeting(&j, shared);
        let raw = counter("sh1", "bo@example.com", ME, "bo@example.com", "20261113T140000Z");
        let message = import(&j, raw.as_bytes());
        let p = card_of(&j, message, &raw).proposal.expect("Bo's proposal");
        assert_eq!((p.accepting, p.declining.is_some()), (None, true));
        assert!(run(accept_draft(&j, message)).unwrap_err().to_string().contains("own calendar"));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A guest's proposal for a meeting someone else organizes, copied to
    /// the user, shows but offers no Decline: only the organizer answers it.
    #[test]
    fn only_the_organizers_account_declines() {
        let (j, calendar) = jolta("proposal-not-mine");
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Planning\r\n\
                   ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = crate::calendar::ical::read(ics, &[ME.to_string()]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "plan.ics", None, &events, None, &Horizon::around(now(), Brussels))).unwrap();
        let raw = counter("n1", "bo@example.com", "ann@example.com", "bo@example.com", "20261113T140000Z");
        let message = import(&j, raw.as_bytes());
        let p = card_of(&j, message, &raw).proposal.expect("Bo's proposal to Ann's meeting");
        assert_eq!((p.accepting, p.declining, p.declined), (None, None, false), "the user is a guest here, not the organizer");
        assert!(run(decline(&j, message)).unwrap_err().to_string().contains("Only the meeting's organizer"));
        assert!(run(accept_draft(&j, message)).unwrap_err().to_string().contains("Only the meeting's organizer"));
        assert!(outbox(&j).is_empty());
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// A proposal for one occurrence is set against that occurrence: where
    /// the series has it, or where it was moved to.
    #[test]
    fn a_proposal_for_an_occurrence_shows_that_occurrences_time() {
        let (j, calendar) = jolta("proposal-occurrence");
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\nDTSTART;TZID=Europe/Brussels:20261110T100000\r\n\
                   DTEND;TZID=Europe/Brussels:20261110T103000\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Planning\r\nORGANIZER;CN=Me:mailto:me@example.com\r\n\
                   ATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\n\
                   RECURRENCE-ID;TZID=Europe/Brussels:20261117T100000\r\nDTSTART;TZID=Europe/Brussels:20261118T140000\r\n\
                   DTEND;TZID=Europe/Brussels:20261118T143000\r\nSUMMARY:Planning\r\nORGANIZER;CN=Me:mailto:me@example.com\r\n\
                   ATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = crate::calendar::ical::read(ics, &[ME.to_string()]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "plan.ics", None, &events, None, &Horizon::around(now(), Brussels))).unwrap();
        let now_for = |id: &str, rid: &str| {
            let raw = counter(id, "bo@example.com", ME, "bo@example.com", "20261124T140000Z")
                .replace("SEQUENCE:1\r\n", &format!("SEQUENCE:1\r\nRECURRENCE-ID:{rid}\r\n"));
            card_of(&j, import(&j, raw.as_bytes()), &raw).proposal.and_then(|p| p.now)
        };
        assert_eq!(now_for("o1", "20261124T090000Z").as_deref(), Some("Tue 24 Nov · 10:00 to 10:30"), "an unchanged occurrence, named in UTC");
        assert_eq!(now_for("o2", "20261117T090000Z").as_deref(), Some("Wed 18 Nov · 14:00 to 14:30"), "the moved one, where it is now");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Exchange's COUNTER names no ORGANIZER and puts "New Time Proposed:"
    /// before the title (seen on the Microsoft 365 test account's proposal
    /// to the Google test account, 2026-11-05). It counts for the user's own
    /// meeting only.
    #[test]
    fn exchanges_proposal_without_an_organizer() {
        let exchange = |id: &str| {
            counter(id, "bo@example.com", ME, "bo@example.com", "20261113T140000Z")
                .replace("SUMMARY:Planning", "SUMMARY:New Time Proposed: Planning")
                .replace("ORGANIZER;CN=Me:mailto:me@example.com\r\n", "")
        };
        let (j, calendar) = jolta("proposal-exchange");
        my_meeting(&j, calendar);
        let raw = exchange("x1");
        let message = import(&j, raw.as_bytes());
        let c = card_of(&j, message, &raw);
        assert_eq!(c.title, "Planning", "the meeting's own title");
        assert_eq!(c.proposal.and_then(|p| p.declining), Some(message), "the user's meeting: Decline proposal");
        std::fs::remove_dir_all(&j.data_dir).unwrap();

        let (j, calendar) = jolta("proposal-exchange-theirs");
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:plan\r\nSEQUENCE:1\r\nDTSTART:20261112T150000Z\r\nDTEND:20261112T160000Z\r\nSUMMARY:Planning\r\n\
                   ORGANIZER;CN=Ann:mailto:ann@example.com\r\nATTENDEE:mailto:me@example.com\r\nATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = crate::calendar::ical::read(ics, &[ME.to_string()]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "plan.ics", None, &events, None, &Horizon::around(now(), Brussels))).unwrap();
        let raw = exchange("x2");
        let c = card_of(&j, import(&j, raw.as_bytes()), &raw);
        assert!(c.proposal.is_none() && notes(&c).contains(&"This isn't from one of the meeting's guests"), "Ann's meeting, not the user's: {:?}", notes(&c));
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
