//! The organizer's side of a guest's proposal for a new time. Decline
//! proposal mails the guest an iMIP DECLINECOUNTER from the account the
//! proposal came to, once. Accepting a proposal changes the meeting itself,
//! which belongs to the calendar's editor; the card says to do it there.

use chrono::Utc;
use rusqlite::OptionalExtension;

use super::card::{self, Card};
use super::{invitation, rsvp, tz, update};
use crate::model::{AccountId, MessageId};
use crate::{Error, Jolta, Result, imap, store};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declined {
    /// For the toast: "Declined Bo's proposal. Jolta emailed Bo."
    pub text: String,
    /// The account whose outbox holds the mail; its engine sends it.
    pub mailed_from: AccountId,
}

/// Declines the proposal on a message's card, when the card offers it: a
/// guest's proposal for a meeting the receiving account organizes. The no
/// goes to the proposer only and answers the proposal's message, so the
/// outbox keeps it as that message's answer. Declining again sends nothing
/// new: a no still waiting goes now.
pub async fn decline(jolta: &Jolta, message: MessageId) -> Result<Declined> {
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
    let (from, proposal, built) = jolta
        .store
        .read_async(move |c| {
            let every: Vec<String> = store::accounts(c)?.into_iter().map(|a| a.address).collect();
            let from = store::account(c, account)?.address;
            let invitations = invitation::find(&raw, &every);
            let cx = card::Context { message, account, me: &every, sender: &sender, now: Utc::now(), here: tz::here(), blobs: &blobs };
            let (uid, proposal) = card::cards(c, &invitations, &cx)?
                .into_iter()
                .find_map(|card| match card {
                    Card::Event(e) => e.proposal.map(|p| (invitations.iter().find(|i| i.method == invitation::Method::Counter).map(|i| i.event.clone()), p)),
                    Card::Reply(_) => None,
                })
                .ok_or_else(|| Error::Other("This message has no proposal to decline.".into()))?;
            if proposal.declining.is_none() && !proposal.declined {
                return Err(Error::Other("Only the meeting's organizer can decline this proposal.".into()));
            }
            let counter = uid.ok_or_else(|| Error::Other("This message has no proposal to decline.".into()))?;
            let to = update::proposer(&counter, &sender).map(|a| a.person.address.clone()).ok_or_else(|| Error::Other("The proposal names no guest.".into()))?;
            let text = invitation::calendar_text(&raw, &counter.uid).ok_or_else(|| Error::Other("Jolta has no copy of this proposal to answer.".into()))?;
            Ok((from.clone(), proposal, rsvp::imip_decline_counter(&text, &from, &to)?))
        })
        .await?;

    let blob = jolta.blobs.put(&built.raw)?;
    jolta
        .store
        .write_async(move |tx| {
            match store::calendar_answer_to(tx, message)? {
                Some(store::ReplyMail::Waiting(id)) => store::send_now(tx, id),
                Some(store::ReplyMail::Sent(_)) => Ok(()),
                None => store::queue_outbox(tx, account, None, &blob, &built.message_id, &from, &built.recipients, Some(message), 0).map(|_| ()),
            }
        })
        .await?;
    let first = proposal.proposer.split_whitespace().next().unwrap_or("the guest").to_string();
    let text = if proposal.declined { format!("{first}'s proposal was declined already.") } else { format!("Declined {first}'s proposal. Jolta emailed {first}.") };
    Ok(Declined { text, mailed_from: account })
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
            Some(Proposed { now: Some("Thu 12 Nov · 16:00 to 17:00".into()), proposer: "Bo Nilsson".into(), declining: Some(message), declined: false }),
            "set against the meeting's time now"
        );
        let a = c.availability.expect("the proposed time's conflicts");
        assert_eq!((a.text.as_str(), a.conflict), ("Overlaps with Dentist", true));
        assert!(a.strip.is_some_and(|s| s.busy.iter().all(|b| b.title == "Dentist")), "the meeting itself isn't its own conflict");

        let done = run(decline(&j, message)).unwrap();
        assert_eq!(done, Declined { text: "Declined Bo's proposal. Jolta emailed Bo.".into(), mailed_from: AccountId(1) });
        assert_eq!(outbox(&j), [(ME.to_string(), "bo@example.com".to_string(), Some(message.0), "DECLINECOUNTER".to_string())], "from the organizer to the proposer");
        let c = card_of(&j, message, &raw);
        assert_eq!(c.proposal.as_ref().map(|p| (p.declining, p.declined)), Some((None, true)), "the card says it was handled");
        assert_eq!(c.availability, None, "a declined time needs no conflicts");

        assert_eq!(run(decline(&j, message)).unwrap().text, "Bo's proposal was declined already.");
        j.store.write(|tx| Ok(tx.execute_batch("UPDATE outbox SET state = 'sent'")?)).unwrap();
        run(decline(&j, message)).unwrap();
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
        assert_eq!((p.declining, p.declined), (None, false), "the user is a guest here, not the organizer");
        assert!(run(decline(&j, message)).unwrap_err().to_string().contains("Only the meeting's organizer"));
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
}
