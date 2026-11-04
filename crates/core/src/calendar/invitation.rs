//! Invitations in mail: the calendar parts of a message (iMIP, RFC 6047),
//! the iTIP method each one carries (RFC 5546) and the events it names.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use mail_parser::{MessageParser, MessagePart, MessageRef};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use super::{EventData, ical};
use crate::model::text_enum;

text_enum!(Method {
    Request => "REQUEST",
    Add => "ADD",
    Cancel => "CANCEL",
    Reply => "REPLY",
    Counter => "COUNTER",
    DeclineCounter => "DECLINECOUNTER",
    Refresh => "REFRESH",
    Publish => "PUBLISH",
});

/// One event of a calendar part, with the method of the calendar it came in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invitation {
    pub method: Method,
    pub event: EventData,
    /// False when the part names the event without a DTSTART, which only
    /// `Method::names_without_start` allows. `event.span` is then a
    /// stand-in, and the time comes from the calendar's copy.
    pub dated: bool,
    /// The DTSTAMP, which orders two versions with the same SEQUENCE.
    pub stamp: Option<DateTime<Utc>>,
}

impl Method {
    /// RFC 5546 leaves DTSTART out of REFRESH and makes it optional in
    /// CANCEL, REPLY and DECLINECOUNTER.
    fn names_without_start(self) -> bool {
        matches!(self, Method::Refresh | Method::Cancel | Method::Reply | Method::DeclineCounter)
    }
}

/// A calendar part bigger than this is not an invitation anyone wrote by hand.
const MAX_PART: usize = 2 << 20;

/// Every event in a raw message's calendar parts, read through S/MIME.
/// `me` marks the user's own attendee entries.
pub fn find(raw: &[u8], me: &[String]) -> Vec<Invitation> {
    let raw = crate::smime::readable(raw);
    match MessageParser::new().parse(&raw[..]) {
        Some(msg) => in_message(&msg.root(), me),
        None => Vec::new(),
    }
}

/// Every event in the `text/calendar` and `application/ics` parts of a
/// message and its attachments named `.ics`; a forwarded message inside it
/// keeps its own. The same event sent twice, as Google sends the inline
/// part and an `invite.ics`, counts once. A calendar without a METHOD is
/// read as PUBLISH: an event shared without asking anything.
pub fn in_message(msg: &MessageRef, me: &[String]) -> Vec<Invitation> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in msg.parts().filter(is_calendar) {
        let bytes = part.decoded();
        if bytes.len() > MAX_PART {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let method = match ical::method(&text) {
            Some(m) => match m.parse::<Method>() {
                Ok(m) => m,
                Err(_) => continue,
            },
            None => Method::Publish,
        };
        for ical::Sent { event, dated, stamp } in ical::read_invitation(&text, me).unwrap_or_default() {
            if (dated || method.names_without_start()) && seen.insert((event.uid.clone(), event.recurrence_id)) {
                out.push(Invitation { method, event, dated, stamp });
            }
        }
    }
    out
}

/// The calendar part of a raw message that names the event `uid`, as sent,
/// for the reply to it.
pub fn calendar_text(raw: &[u8], uid: &str) -> Option<String> {
    let raw = crate::smime::readable(raw);
    let msg = MessageParser::new().parse(&raw[..])?;
    msg.parts()
        .filter(is_calendar)
        .map(|part| String::from_utf8_lossy(&part.decoded()).into_owned())
        .find(|text| ical::read_invitation(text, &[]).unwrap_or_default().iter().any(|s| s.event.uid == uid))
}

/// Whether a raw message may hold a calendar part, without parsing it: its
/// bytes name a calendar type or an `.ics` file, or S/MIME hides its parts.
pub fn may_hold(raw: &[u8]) -> bool {
    const MARKS: [&[u8]; 3] = [b"text/calendar", b"application/ics", b".ics"];
    MARKS.iter().any(|m| raw.windows(m.len()).any(|w| w.eq_ignore_ascii_case(m))) || crate::smime::is_smime(raw)
}

fn is_calendar(part: &MessagePart) -> bool {
    part.is_content_type("text", "calendar")
        || part.is_content_type("application", "ics")
        || part.attachment_name().is_some_and(|n| n.len() > 4 && n.as_bytes()[n.len() - 4..].eq_ignore_ascii_case(b".ics"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::calendar::{Local, Moment, Response, Span, Zone};

    macro_rules! mail {
        ($name:literal) => {
            include_bytes!(concat!("../../tests/fixtures/invitations/", $name))
        };
    }
    pub(crate) use mail;

    pub(crate) const ME: &str = "me@example.com";

    /// A data folder with the user's account, its inbox and a shown
    /// calendar. Removed with `std::fs::remove_dir_all(&jolta.data_dir)`.
    pub(crate) fn jolta(name: &str) -> (crate::Jolta, crate::calendar::CalendarId) {
        let dir = std::env::temp_dir().join(format!("jolta-invite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jolta = crate::Jolta::open(&dir).unwrap();
        let calendar = jolta
            .store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO account (id, kind, address, username, secret_ref, created, calendar)
                     VALUES (1, 'imap', 'me@example.com', 'me', 'a1', 0, 1);
                     INSERT INTO mailbox (id, account_id, remote_id, name, role) VALUES (1, 1, 'INBOX', 'Inbox', 'inbox');",
                )?;
                let work = crate::store::RemoteCalendar {
                    remote_id: "/cal/work/".into(),
                    name: "Work".into(),
                    color: None,
                    zone: None,
                    access: crate::calendar::Access::Owner,
                    primary: true,
                    refresh: None,
                };
                crate::store::sync_calendar_list(tx, crate::AccountId(1), crate::calendar::CalendarSource::CalDav, &[work])
            })
            .unwrap()[0]
            .id;
        (jolta, calendar)
    }

    pub(crate) fn import(jolta: &crate::Jolta, raw: &[u8]) -> crate::MessageId {
        crate::import::import_raw(jolta, crate::AccountId(1), crate::MailboxId(1), raw).unwrap()
    }

    fn one(raw: &[u8]) -> Invitation {
        let mut found = find(raw, &[ME.into()]);
        assert_eq!(found.len(), 1, "{found:#?}");
        found.remove(0)
    }

    fn brussels(s: &str) -> Moment {
        let wall = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        Moment::Time(Local { wall, zone: Zone::Tz(chrono_tz::Europe::Brussels) })
    }

    #[test]
    fn google_sends_the_same_event_inline_and_as_invite_ics() {
        let i = one(mail!("google-request.eml"));
        assert_eq!(i.method, Method::Request);
        assert_eq!(i.event.uid, "7kq2m4p9r1s3t5v7x9z0b2d4f6@google.com");
        assert_eq!(i.event.organizer.as_ref().unwrap().address, "ann@example.com");
        assert_eq!(i.event.my_response(), Some(Response::NeedsAction));
        assert_eq!(i.event.attendees.len(), 7);
    }

    #[test]
    fn outlook_and_icloud_invitations() {
        let outlook = one(mail!("outlook-request.eml"));
        assert_eq!(outlook.method, Method::Request);
        assert!(outlook.event.uid.starts_with("040000008200E00074C5B7101A82E008"));
        let Span::Timed { start, .. } = outlook.event.span else { panic!("timed") };
        assert_eq!(start.zone, Zone::Tz(chrono_tz::America::New_York), "a Windows zone name");
        assert_eq!(outlook.event.meeting.as_ref().unwrap().provider, crate::calendar::MeetingProvider::Teams);

        let icloud = one(mail!("icloud-request.eml"));
        assert_eq!(icloud.method, Method::Request, "from an attachment named .ics");
        assert_eq!(icloud.event.organizer.as_ref().unwrap().address, "pat@example.com", "Apple's EMAIL parameter");
        assert_eq!(icloud.event.span.start(), brussels("2026-11-06 12:00"));
    }

    #[test]
    fn cancellations_replies_and_series() {
        let cancel = one(mail!("cancel.eml"));
        assert_eq!((cancel.method, cancel.event.sequence), (Method::Cancel, 1));

        let reply = one(mail!("reply.eml"));
        assert_eq!(reply.method, Method::Reply);
        assert_eq!(reply.event.attendees[0].response, Response::Accepted);
        assert_eq!(reply.event.attendees[0].comment, "See you there");

        let series = find(mail!("series.eml"), &[ME.into()]);
        let ids: Vec<_> = series.iter().map(|i| (i.event.uid.as_str(), i.event.recurrence_id)).collect();
        assert_eq!(
            ids,
            [("standup-series@example.com", None), ("standup-series@example.com", Some(brussels("2026-11-05 09:30")))]
        );
        assert!(series[0].event.recurrence.is_some());
    }

    #[test]
    fn plain_mail_and_odd_calendars() {
        assert!(find(b"Subject: hi\r\n\r\nNo calendar here.\r\n", &[]).is_empty());
        let shared = "Subject: s\r\nContent-Type: text/calendar\r\n\r\nBEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:u\r\n\
            DTSTART:20261105T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(one(shared.as_bytes()).method, Method::Publish, "no METHOD is an event shared without asking");
        let unknown = shared.replace("BEGIN:VCALENDAR\r\n", "BEGIN:VCALENDAR\r\nMETHOD:X-POLL\r\n");
        assert!(find(unknown.as_bytes(), &[]).is_empty(), "a method Jolta doesn't know");
    }

    #[test]
    fn may_hold_tells_without_parsing() {
        for raw in [&mail!("google-request.eml")[..], mail!("icloud-request.eml"), mail!("series.eml")] {
            assert!(may_hold(raw));
        }
        assert!(!may_hold(b"Subject: hi\r\nContent-Type: text/plain\r\n\r\nNothing.\r\n"));
    }

    #[test]
    fn an_attachment_name_ending_inside_a_character() {
        let raw = "Subject: s\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
            --b\r\nContent-Type: application/gzip\r\nContent-Disposition: attachment; filename=\"café.gz\"\r\n\r\nx\r\n--b--\r\n";
        assert!(find(raw.as_bytes(), &[]).is_empty());
    }
}
