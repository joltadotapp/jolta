//! File responses automatically. With the setting on, a guest's reply to a
//! meeting the receiving account organizes that says nothing beyond its
//! answer goes to the archive through the same op as the E key, the way
//! Outlook files responses. Each reply is settled once, by its Message-ID:
//! filed, or left in the inbox for good.
//!
//! What a reply says beyond its answer, from 58 real replies by other
//! people: Exchange (Microsoft 365, Outlook) sends an empty text and HTML
//! body and puts a note in COMMENT and in the body. Google writes one
//! sentence ("Ann has accepted this invitation.") above the event's details
//! and its footer, and with a note ends it "with a note:" and puts the note
//! in the attendee's X-RESPONSE-COMMENT. Apple's Calendar writes one
//! sentence ("Pat has accepted your invitation to the event: …"), and
//! Jolta's own reply the same sentence as Google's.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use mail_parser::{MessageParser, MessagePart, PartKind};
use rusqlite::Connection;

use super::invitation::{self, Invitation, Method};
use super::{Access, Calendar, CalendarSource, EventData, ical, respond, tz, update};
use crate::blobs::Blobs;
use crate::model::{Account, AccountId};
use crate::store::{self, OpId, UnsettledReply};
use crate::{Jolta, Result, actions};

mod google;

/// Replies filed in one pass, for the toast that offers Undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filed {
    /// The one archive op that moved them.
    pub op: OpId,
    /// "Filed a response from Ann", "Filed 3 responses".
    pub text: String,
}

/// How long after it arrives a reply that isn't yet known as a guest's
/// answer to the account's own meeting is looked at again: the calendar's
/// copy of a meeting made minutes ago may still be on its way.
const KNOWN_WITHIN: Duration = Duration::days(1);

/// What to do with one unsettled reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    File,
    /// Something beyond an answer: leave it in the inbox for good.
    Keep,
    /// Not known as a guest's answer to the account's own meeting yet.
    Wait,
}

/// Files the account's new answer-only replies when the setting is on.
/// None when nothing was filed.
pub fn file_new(jolta: &Jolta, account: AccountId, now: DateTime<Utc>) -> Result<Option<Filed>> {
    let here = tz::here();
    let judged = jolta.store.read(|c| {
        let Some(since) = store::file_responses_since(c)? else { return Ok(Vec::new()) };
        let account = store::account(c, account)?;
        store::unsettled_replies(c, account.id, since)?
            .into_iter()
            .map(|reply| Ok((verdict(c, &jolta.blobs, &account, &reply, now, here)?, reply)))
            .collect::<Result<Vec<_>>>()
    })?;
    jolta.store.write(|tx| {
        // The setting may have gone off while the replies were read.
        if store::file_responses_since(tx)?.is_none() {
            return Ok(None);
        }
        let mut filing = Vec::new();
        for (verdict, reply) in &judged {
            if *verdict == Verdict::Wait || !store::settle_reply(tx, account, &reply.message_id)? {
                continue;
            }
            if *verdict == Verdict::File {
                filing.push(reply);
            }
        }
        let Some((_, op)) = actions::archive_messages(tx, account, &filing.iter().map(|r| r.id).collect::<Vec<_>>())? else { return Ok(None) };
        let moved = store::op(tx, op)?.map_or(0, |(op, _)| op.messages.len());
        let text = match (moved, filing.as_slice()) {
            (1, [reply]) => format!("Filed a response from {}", first_name(reply)),
            (n, _) => format!("Filed {n} responses"),
        };
        Ok(Some(Filed { op, text }))
    })
}

fn first_name(reply: &UnsettledReply) -> &str {
    reply.from_name.split_whitespace().next().unwrap_or(&reply.from_addr)
}

fn verdict(conn: &Connection, blobs: &Blobs, account: &Account, reply: &UnsettledReply, now: DateTime<Utc>, here: Tz) -> Result<Verdict> {
    let Ok(raw) = blobs.get(&reply.blob) else { return Ok(Verdict::Keep) };
    let found = invitation::find(&raw, &[]);
    if found.is_empty() || found.iter().any(|i| i.method != Method::Reply || !answer_only(&raw, i)) {
        return Ok(Verdict::Keep);
    }
    for sent in &found {
        if !guest_answers_own_meeting(conn, blobs, account, reply, sent, here)? {
            let waited = now.timestamp() - reply.date > KNOWN_WITHIN.num_seconds();
            return Ok(if waited { Verdict::Keep } else { Verdict::Wait });
        }
    }
    Ok(Verdict::File)
}

/// Whether a reply answers a meeting that the receiving account, by its
/// address or an alias its own calendar's copy marks, organizes, and comes
/// from one of that meeting's guests (`update::from_guest`), from their own
/// address or from Google's. A changed occurrence shares its series'
/// organizer and guests.
fn guest_answers_own_meeting(conn: &Connection, blobs: &Blobs, account: &Account, reply: &UnsettledReply, sent: &Invitation, here: Tz) -> Result<bool> {
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    if !same(&reply.from_addr, GOOGLE_FOR_GUESTS) && !sent.event.attendees.iter().any(|a| same(&a.person.address, &reply.from_addr)) {
        return Ok(false);
    }
    let uid = &sent.event.uid;
    let copy = store::stored_event(conn, uid, None, Some(account.id), here)?;
    let own = match &copy {
        Some(copy) => owns(&store::calendar(conn, copy.calendar_id)?, account),
        None => false,
    };
    let me = respond::addresses(&account.address, copy.as_ref().filter(|_| own).map(|c| &c.event));
    let organizer = update::organizer(conn, account.id, &me, uid)?;
    if !organizer.as_ref().is_some_and(|o| me.iter().any(|m| m.trim().eq_ignore_ascii_case(o.trim()))) {
        return Ok(false);
    }
    let seen = update::Seen { conn, blobs, account: account.id, message: reply.id, sender: &reply.from_addr, me: &me, here };
    update::from_guest(&seen, organizer.as_deref(), &sent.event)
}

/// The one provider address a guest's reply may come from. iCloud's
/// `noreply@email.apple.com`, which `update::from_guest` also accepts, was
/// seen sending organizers' invitations but no guest's reply: the one real
/// reply from Apple's Calendar came from the guest's own address.
const GOOGLE_FOR_GUESTS: &str = "calendar-notification@google.com";

/// Whether the calendar is the account's own, so the attendee its copy
/// marks as the user is an alias. A shared or delegated calendar marks its
/// owner. Google gives a calendar shared with "Make changes and manage
/// sharing" the owner role too, so there only the account's primary
/// calendar, whose id is the account's address, counts.
fn owns(calendar: &Calendar, account: &Account) -> bool {
    calendar.access == Access::Owner && (calendar.source != CalendarSource::Google || calendar.remote_id.eq_ignore_ascii_case(&account.address))
}

/// Who wrote a reply's calendar part, by its PRODID, for the words it puts
/// around every answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Producer {
    /// Microsoft 365 and Outlook: all 37 real replies said `Microsoft
    /// Exchange Server 2010`.
    Exchange,
    Google,
    Apple,
    Jolta,
    Other,
}

impl Producer {
    fn of(prodid: &str) -> Producer {
        let id = prodid.to_ascii_lowercase();
        if id.starts_with("microsoft exchange server") {
            Producer::Exchange
        } else if id.starts_with("-//google inc//google calendar") {
            Producer::Google
        } else if id.starts_with("-//apple inc.//") {
            Producer::Apple
        } else if id.starts_with("-//jolta//") {
            Producer::Jolta
        } else {
            Producer::Other
        }
    }

    /// Whether `text`, one body as a reader sees it, holds only the words
    /// this producer writes around an answer to `event`: nothing at all
    /// from Exchange, one sentence from Apple and Jolta, and from Google its
    /// sentence above the event's details. A quoted line is never theirs.
    fn wrote(self, text: &str, event: &EventData) -> bool {
        let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !blank(l)).collect();
        if lines.iter().any(|l| l.starts_with('>')) {
            return false;
        }
        match (self, lines.as_slice()) {
            (Producer::Exchange, []) => true,
            (Producer::Google, [first, ..]) => (said(first, ENGLISH) || said(first, DUTCH)) && google::details(text, event),
            (Producer::Jolta, [only]) => said(only, ENGLISH),
            (Producer::Apple, [only]) => APPLE.iter().any(|s| only.split_once(s).is_some_and(|(who, event)| !who.is_empty() && !event.is_empty())),
            _ => false,
        }
    }
}

fn said(line: &str, sentences: &[&str]) -> bool {
    sentences.iter().any(|s| line.len() > s.len() && line.ends_with(s))
}

/// Google's and Jolta's sentence.
const ENGLISH: &[&str] = &[" has accepted this invitation.", " has declined this invitation.", " has tentatively accepted this invitation."];
/// Google's sentence in Dutch.
const DUTCH: &[&str] = &[" heeft deze uitnodiging aangenomen.", " heeft deze uitnodiging afgeslagen."];
/// Apple's sentence, before the event's title and time.
const APPLE: &[&str] = &[" has accepted your invitation to the event: ", " has declined your invitation to the event: ", " has tentatively accepted your invitation to the event: "];

/// Whether a reply says nothing beyond its answer: it answers as one guest
/// without a note, and every body a reader could see, text or HTML, is
/// empty or holds only the words its producer writes around every answer.
/// Anything else, including a body Jolta can't read, counts as more.
pub fn answer_only(raw: &[u8], reply: &Invitation) -> bool {
    let [guest] = reply.event.attendees.as_slice() else { return false };
    if !blank(&guest.comment) {
        return false;
    }
    let raw = crate::smime::readable(raw);
    let Some(msg) = MessageParser::new().parse(&raw[..]) else { return false };
    let root = msg.root();
    let calendar = root.parts().filter(is_calendar).map(|p| String::from_utf8_lossy(&p.decoded()).into_owned()).find(|text| {
        ical::method(text).is_some_and(|m| m == "REPLY") && ical::read_invitation(text, &[]).unwrap_or_default().iter().any(|s| s.event.uid == reply.event.uid)
    });
    let Some(calendar) = calendar else { return false };
    let producer = Producer::of(&ical::producer(&calendar).unwrap_or_default());
    let (mut text, mut html) = (Vec::new(), Vec::new());
    let mut seen = std::collections::HashSet::new();
    for part in root.text_body().chain(root.html_body()) {
        if is_calendar(&part) || !seen.insert(part.id()) {
            continue;
        }
        match (part.kind(), part.text()) {
            (PartKind::Text, Some(t)) => text.push(t.into_owned()),
            (PartKind::Html, Some(h)) if !shows_image(&h) => html.push(html_text(&h)),
            // An image or other part shown in the body.
            _ => return false,
        }
    }
    if text.is_empty() && html.is_empty() {
        return producer.wrote("", &reply.event);
    }
    text.iter().all(|t| producer.wrote(t, &reply.event))
        && html.iter().all(|h| match producer {
            Producer::Google if !text.is_empty() => text.iter().any(|t| shows_only(h, t, GOOGLE_HTML_LABELS)),
            Producer::Exchange => producer.wrote(h, &reply.event),
            _ if text.is_empty() => producer.wrote(h, &reply.event),
            _ => text.iter().any(|t| shows_only(h, t, &[])),
        })
}

/// Whether an HTML body shows an image, whose words the text conversion
/// can't read: one the reader shows, or an SVG, a picture, a data URI or
/// an inline part, which no real reply's HTML has.
fn shows_image(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    crate::render::shows_beyond_text(html) || ["<svg", "<picture", "data:", "cid:"].iter().any(|m| lower.contains(m))
}

/// An HTML body reduced to its text, with a break at every tag so that
/// words in neighbouring cells stay apart.
fn html_text(html: &str) -> String {
    mail_parser::html_to_text(&html.replace('<', " <"))
}

/// Whether the HTML body says no more than the text body: none of its
/// words, links aside, comes more often than in the text, except `labels`.
/// Google's HTML puts the title above the sentence, so the order can
/// differ.
fn shows_only(html: &str, text: &str, labels: &[&str]) -> bool {
    let mut left: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for w in words(text) {
        *left.entry(w).or_default() += 1;
    }
    words(html).filter(|w| !labels.contains(&w.as_str())).all(|w| match left.get_mut(&w) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    })
}

/// The labels Google's HTML adds to the details its text body holds.
const GOOGLE_HTML_LABELS: &[&str] = &["when", "wanneer", "view", "map", "kaart"];

/// Lowercase words without links.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split_whitespace()
        .filter(|w| !w.contains("://"))
        .flat_map(|w| w.split(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

/// Whitespace and the zero-width characters some mailers leave in an
/// empty note.
fn blank(text: &str) -> bool {
    text.chars().all(|c| c.is_whitespace() || matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}'))
}

fn is_calendar(part: &MessagePart) -> bool {
    part.is_content_type("text", "calendar") || part.is_content_type("application", "ics")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::invitation::tests::{ME, import, jolta, mail};
    use crate::calendar::rsvp::{self, Answer, ImipReply};
    use crate::calendar::series::Horizon;
    use crate::calendar::{CalendarId, EventData};
    use crate::model::{MailboxId, MessageId};

    const UID: &str = "design-crit-1113@example.com";

    fn only(raw: &[u8]) -> bool {
        let found = invitation::find(raw, &[]);
        assert_eq!(found.len(), 1, "one reply");
        answer_only(raw, &found[0])
    }

    /// A reply written by `prodid` with this text body and ATTENDEE line.
    fn shaped(prodid: &str, text: &str, attendee: &str) -> String {
        bodies(prodid, &[("text/plain", text)], attendee)
    }

    /// A reply written by `prodid` with these alternatives, each a content
    /// type and its body, and this ATTENDEE line.
    fn bodies(prodid: &str, bodies: &[(&str, &str)], attendee: &str) -> String {
        let bodies: String = bodies.iter().map(|(kind, body)| format!("--b\r\nContent-Type: {kind}; charset=utf-8\r\n\r\n{body}\r\n")).collect();
        format!(
            "From: Bo Nilsson <bo@example.com>\r\nTo: me@example.com\r\nSubject: Accepted: Design crit\r\nMessage-ID: <shaped@example.com>\r\n\
             MIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n{bodies}\
             --b\r\nContent-Type: text/calendar; method=REPLY\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nPRODID:{prodid}\r\nBEGIN:VEVENT\r\nUID:{UID}\r\n\
             DTSTAMP:20261104T103000Z\r\nSUMMARY:Design crit\r\nDESCRIPTION:Bring the latest <https://example.com/mock-ups>mock-ups.\r\n{attendee}\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n"
        )
    }

    const BO: &str = "ATTENDEE;PARTSTAT=ACCEPTED;CN=Bo Nilsson:mailto:bo@example.com";
    const GOOGLE: &str = "-//Google Inc//Google Calendar 70.9054//EN";
    const EXCHANGE: &str = "Microsoft Exchange Server 2010";

    /// Google's text body in the layout since 2022, the way real replies
    /// wrap it, with `more` among its details.
    fn google(more: &str) -> String {
        format!(
            "Bo Nilsson has accepted this invitation.\r\n\r\nDesign crit\r\nFriday 13 Nov 2026 \u{22c5} 10:00 \u{2013} 10:45\r\nCentral European Time - Brussels\r\n\r\n\
             Bring the latest mock-ups.\r\n{more}\r\nOrganizer\r\nme@example.com\r\nme@example.com\r\n\r\nGuests\r\nme@example.com - organizer\r\nBo Nilsson\r\n\
             View all guest info  \r\nhttps://calendar.google.com/calendar/event?action=VIEW&eid=ZGVz\r\n\r\n~~//~~\r\n\
             Invitation from Google Calendar: https://calendar.google.com/calendar/\r\n\r\nYou are receiving this email because you are an attendee on the event.\r\n\r\n\
             Forwarding this invitation could allow any recipient to send a response to  \r\nthe organizer, be added to the guest list, invite others regardless of  \r\n\
             their own invitation status, or modify your RSVP.\r\n\r\nLearn more https://support.google.com/calendar/answer/37135#forwarding\r\n"
        )
    }

    /// Google's text body in Dutch in the layout of 2013.
    const GOOGLE_2013: &str = "Bo heeft deze uitnodiging afgeslagen.\r\n\r\nTitel: Design crit\r\nBring the latest mock-ups.\r\nWanneer: vr 13. nov 2026 10:00 \u{2013} 10:45 Brussel\r\n\
         Agenda: me@example.com\r\nWie:\r\n     * me@example.com - organisator\r\n     * Bo\r\n\r\n\r\nUitnodiging van Google Agenda: https://www.google.com/calendar/\r\n\r\n\
         Je ontvangt deze e-mail op het adres me@example.com omdat je je hebt  \r\naangemeld voor updates over antwoorden op uitnodigingen in de agenda van  \r\nme@example.com.\r\n";

    #[test]
    fn each_providers_answer_alone_and_with_a_note() {
        assert!(only(mail!("reply-exchange.eml")), "Exchange: an empty body and a COMMENT of one line break");
        assert!(!only(mail!("reply-exchange-note.eml")), "Exchange: the note in COMMENT and in the body");
        assert!(only(mail!("reply-google.eml")), "Google: its sentence above the event's details");
        assert!(!only(mail!("reply-google-note.eml")), "Google: the note in X-RESPONSE-COMMENT");
        assert!(!only(mail!("reply.eml")), "a note in COMMENT under Google's sentence");
        assert!(only(mail!("reply-apple.eml")), "Apple: its one sentence");
    }

    #[test]
    fn words_the_producer_didnt_write_stay() {
        assert!(only(shaped(EXCHANGE, "\u{200b} \u{a0}", BO).as_bytes()), "a zero-width space is no note");
        assert!(!only(shaped(EXCHANGE, "See you there", BO).as_bytes()), "words in Exchange's body with no COMMENT");
        assert!(!only(shaped("AndroidEmail", "When: 13-Nov-2026 10:00", BO).as_bytes()), "words from a mailer Jolta doesn't know");
        assert!(!only(shaped(GOOGLE, "Running late, see you at 10:15\r\nBo has accepted this invitation.", BO).as_bytes()), "words above Google's sentence");
        assert!(!only(shaped(GOOGLE, "has accepted this invitation.", BO).as_bytes()), "the sentence without anyone in it");
        assert!(!only(shaped(GOOGLE, "Bo has accepted this invitation with a note:\r\n\"Late\"", BO).as_bytes()), "Google's note in the body alone");
        let apple = "-//Apple Inc.//Mac OS X 10.8.3//EN";
        assert!(!only(shaped(apple, "Bo has accepted your invitation to the event: Design crit.\r\nCan we move it?", BO).as_bytes()), "a line under Apple's sentence");
        let two = format!("{BO}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:cy@example.com");
        assert!(!only(shaped(EXCHANGE, "", &two).as_bytes()), "a reply for two guests");
        let note = format!("{BO}\r\nCOMMENT:\\n\\nsee you\\n");
        assert!(!only(shaped(EXCHANGE, "", &note).as_bytes()), "a note in COMMENT alone");
    }

    #[test]
    fn only_exchange_sends_an_empty_body() {
        assert!(only(shaped(EXCHANGE, "", BO).as_bytes()));
        for prodid in ["AndroidEmail", "", GOOGLE, "-//Apple Inc.//Mac OS X 10.8.3//EN", "-//Jolta//Jolta//EN"] {
            assert!(!only(shaped(prodid, "", BO).as_bytes()), "an empty body from {prodid:?}");
        }
        let unnamed = shaped(EXCHANGE, "", BO).replace(&format!("PRODID:{EXCHANGE}\r\n"), "");
        assert!(!only(unnamed.as_bytes()), "no PRODID");
    }

    #[test]
    fn under_googles_sentence_only_googles_details() {
        assert!(only(shaped(GOOGLE, &google(""), BO).as_bytes()), "the layout since 2022");
        assert!(only(shaped(GOOGLE, GOOGLE_2013, BO).as_bytes()), "the layout of 2013, in Dutch");
        let quoted = "> Bo has accepted this invitation.\r\n\r\nCan we move it?";
        assert!(!only(shaped(GOOGLE, quoted, BO).as_bytes()), "a quoted sentence");
        let quoted_details = google("").replacen("Design crit\r\n", "> Design crit\r\n", 1);
        assert!(!only(shaped(GOOGLE, &quoted_details, BO).as_bytes()), "a quoted line among the details");
        assert!(!only(shaped(GOOGLE, &google("Can we move it?\r\n"), BO).as_bytes()), "a line among the details");
        assert!(!only(shaped(GOOGLE, &format!("{}\r\nCan we move it?\r\n", google("")), BO).as_bytes()), "a line under the footer");
        let footer = google("").replace("Learn more", "Read more");
        assert!(!only(shaped(GOOGLE, &footer, BO).as_bytes()), "a footer Jolta hasn't seen");
        let cut = google("");
        let cut = &cut[..cut.find("~~//~~").unwrap()];
        assert!(!only(shaped(GOOGLE, cut, BO).as_bytes()), "no footer");
        let place = GOOGLE_2013.replace("Agenda:", "Waar: Room 3\r\nAgenda:");
        assert!(!only(shaped(GOOGLE, &place, BO).as_bytes()), "a place the event doesn't have");
    }

    #[test]
    fn every_body_a_reader_sees() {
        let blank = [("text/plain", ""), ("text/html", "<html><body><p>\u{a0}</p></body></html>")];
        assert!(only(bodies(EXCHANGE, &blank, BO).as_bytes()), "Exchange's empty text and HTML");
        let html = [("text/plain", ""), ("text/html", "<html><body><p>Can we move it?</p></body></html>")];
        assert!(!only(bodies(EXCHANGE, &html, BO).as_bytes()), "words in the HTML alone");
        assert!(!only(bodies(EXCHANGE, &html[1..], BO).as_bytes()), "words in the only body, HTML");
        let text = google("");
        let html = "<html><body><h2>Design crit</h2><p>Bo Nilsson has accepted this invitation.</p><p>When</p><p>Friday 13 Nov 2026 &#8901; 10:00 &ndash; 10:45</p>\
                    <p>Bring the latest mock-ups.</p><a href=\"https://www.google.com/maps\">View map</a><p>Invitation from Google Calendar</p></body></html>";
        assert!(only(bodies(GOOGLE, &[("text/plain", &text), ("text/html", html)], BO).as_bytes()), "Google's HTML of the same details");
        let more = html.replace("<p>When</p>", "<p>Can we move it?</p><p>When</p>");
        assert!(!only(bodies(GOOGLE, &[("text/plain", &text), ("text/html", &more)], BO).as_bytes()), "a line in Google's HTML alone");
        let image = bodies(EXCHANGE, &[("text/plain", "")], BO).replace(
            "multipart/alternative; boundary=b\r\n\r\n",
            "multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\nContent-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n",
        );
        assert!(!only(image.as_bytes()), "an image in the body");
    }

    #[test]
    fn an_image_in_the_html_stays() {
        let images = [
            ("an image as a data URI", "<p><img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"\"></p>"),
            ("a remote image", "<p><img src=\"https://example.com/can-we-move-it.png\"></p>"),
            ("an inline image", "<p><img src=\"cid:note@example.com\"></p>"),
            ("a CSS background", "<div style=\"background-image: url(data:image/png;base64,iVBORw0KGgo=); height: 40px\">&nbsp;</div>"),
            ("a CSS background behind entities", "<div style=\"background: url(&#100;ata:image/png;base64,iVBORw0KGgo=)\">&nbsp;</div>"),
            ("a stylesheet's background", "<style>p { background: url('cid:note@example.com') }</style><p>&nbsp;</p>"),
            ("text from a stylesheet", "<style>p::before { content: 'Can we move it?' }</style><p>&nbsp;</p>"),
            ("an SVG", "<svg width=\"200\" height=\"40\"><image href=\"https://example.com/note.png\"/></svg>"),
            ("a picture", "<picture><source srcset=\"https://example.com/note.png\"></picture>"),
        ];
        for (what, html) in images {
            assert!(!only(bodies(EXCHANGE, &[("text/plain", ""), ("text/html", html)], BO).as_bytes()), "{what} in Exchange's HTML");
            let html = format!("<html><body><h2>Design crit</h2><p>Bo Nilsson has accepted this invitation.</p>{html}</body></html>");
            assert!(!only(bodies(GOOGLE, &[("text/plain", &google("")), ("text/html", &html)], BO).as_bytes()), "{what} in Google's HTML");
        }
        let icons = "<html><head><style>.icon { background-image: url('https://fonts.gstatic.com/s/i/googlematerialiconsfilled/event/v6/24px.png') }</style></head>\
                     <body><h2>Design crit</h2><p>Bo Nilsson has accepted this invitation.</p></body></html>";
        assert!(only(bodies(GOOGLE, &[("text/plain", &google("")), ("text/html", icons)], BO).as_bytes()), "Google's own icons, which the reader drops");
    }

    #[test]
    fn only_google_s_html_adds_labels() {
        for html in ["<p>When?</p>", "<p>View</p><p>map</p>"] {
            assert!(!only(bodies(EXCHANGE, &[("text/plain", ""), ("text/html", html)], BO).as_bytes()), "{html} under Exchange's empty text");
        }
        let apple = "-//Apple Inc.//Mac OS X 10.8.3//EN";
        let sentence = "Bo has accepted your invitation to the event: Design crit.";
        let html = format!("<p>{sentence}</p><p>When?</p>");
        assert!(!only(bodies(apple, &[("text/plain", sentence), ("text/html", &html)], BO).as_bytes()), "a label in Apple's HTML");
    }

    /// Jolta's own iMIP reply, which the iCloud account sends, with and
    /// without a note.
    #[test]
    fn jolta_s_own_reply() {
        let request = format!(
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{UID}\r\nSEQUENCE:0\r\nDTSTAMP:20261104T080000Z\r\nDTSTART:20261113T090000Z\r\n\
             DTEND:20261113T094500Z\r\nSUMMARY:Design crit\r\nORGANIZER;CN=Me:mailto:me@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:bo@example.com\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let me = ["bo@example.com".to_string()];
        let built = |answer, note| rsvp::imip_reply(&ImipReply { invitation: &request, from: &me[0], me: &me, answer, note, occurrence: None }, Brussels).unwrap().raw;
        assert!(only(&built(Answer::Accept, None)));
        assert!(only(&built(Answer::Maybe, None)));
        assert!(only(&built(Answer::Decline, Some("  "))));
        assert!(!only(&built(Answer::Decline, Some("Away that week"))));
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 11, 0, 0).unwrap()
    }

    const INBOX: MailboxId = MailboxId(1);
    const ARCHIVE: MailboxId = MailboxId(2);

    /// The test account with an Archive folder and the setting on, and
    /// another of the user's accounts.
    fn setup(name: &str) -> (Jolta, CalendarId) {
        let (j, calendar) = jolta(&format!("filing-{name}"));
        j.store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO mailbox (id, account_id, remote_id, name, role) VALUES (2, 1, 'Archive', 'Archive', 'archive');
                     INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (2, 'imap', 'me2@example.net', 'me2', 'a2', 0);",
                )?;
                store::set_file_responses(tx, true, 0)
            })
            .unwrap();
        (j, calendar)
    }

    /// The meeting `UID` in the account's calendar, organized by
    /// `organizer` with Bo and the account as guests. `me` marks the user's
    /// addresses, as a sync does.
    fn meeting(j: &Jolta, calendar: CalendarId, organizer: &str, me: &[&str], more: &str) {
        let ics = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{UID}\r\nSEQUENCE:0\r\nDTSTART:20261113T090000Z\r\nDTEND:20261113T094500Z\r\nSUMMARY:Design crit\r\n\
             ORGANIZER:mailto:{organizer}\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:{organizer}\r\nATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\n{more}END:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let me: Vec<String> = me.iter().map(|m| m.to_string()).collect();
        let events: Vec<EventData> = crate::calendar::ical::read(&ics, &me).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "crit.ics", None, &events, None, &Horizon::around(now(), Brussels))).unwrap();
    }

    /// Exchange's reply with nothing beyond the answer, From `from`, for
    /// `guest`, naming `organizer` or, as Exchange does, none.
    fn exchange(id: &str, from: &str, guest: &str, organizer: Option<&str>, more: &str) -> String {
        let organizer = organizer.map(|o| format!("ORGANIZER:mailto:{o}\r\n")).unwrap_or_default();
        format!(
            "From: {from}\r\nTo: me@example.com\r\nSubject: Accepted: Design crit\r\nDate: Wed, 04 Nov 2026 10:30:00 +0000\r\nMessage-ID: <{id}@example.com>\r\n\
             MIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\n\r\n\
             --b\r\nContent-Type: text/calendar; method=REPLY\r\n\r\nBEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nPRODID:{EXCHANGE}\r\nBEGIN:VEVENT\r\n\
             ATTENDEE;PARTSTAT=ACCEPTED:mailto:{guest}\r\nCOMMENT;LANGUAGE=en-US:\\n\r\nUID:{UID}\r\n{more}{organizer}DTSTART:20261113T090000Z\r\n\
             DTEND:20261113T094500Z\r\nDTSTAMP:20261104T103000Z\r\nSEQUENCE:0\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n"
        )
    }

    fn mailboxes(j: &Jolta, message: MessageId) -> Vec<MailboxId> {
        j.store
            .read(|c| {
                let mut stmt = c.prepare("SELECT mailbox_id FROM message_mailbox WHERE message_id = ?1 ORDER BY mailbox_id")?;
                Ok(stmt.query_map([message], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
    }

    fn file(j: &Jolta) -> Option<Filed> {
        file_new(j, AccountId(1), now()).unwrap()
    }

    fn filed(j: &Jolta, raw: &str) -> bool {
        let message = import(j, raw.as_bytes());
        let done = file(j);
        let archived = mailboxes(j, message) == [ARCHIVE];
        assert_eq!(done.is_some(), archived);
        archived
    }

    #[test]
    fn a_reply_is_filed_once() {
        let (j, calendar) = setup("once");
        meeting(&j, calendar, ME, &[ME], "");
        let message = import(&j, mail!("reply-exchange.eml"));
        let done = file(&j).expect("filed");
        assert_eq!(done.text, "Filed a response from Bo");
        assert_eq!(mailboxes(&j, message), [ARCHIVE]);
        let (op, _) = j.store.read(|c| store::op(c, done.op)).unwrap().unwrap();
        assert_eq!(op.kind, store::OpKind::Move { from: INBOX, to: ARCHIVE }, "the E key's archive");
        assert_eq!(file(&j), None, "twice");

        j.store.write(|tx| actions::undo(tx, done.op)).unwrap();
        assert_eq!(mailboxes(&j, message), [INBOX], "undone");
        assert_eq!(file(&j), None, "moved back to the inbox, it stays");

        j.store.write(|tx| Ok(tx.execute_batch(&format!("DELETE FROM message_mailbox WHERE message_id = {0}; DELETE FROM message WHERE id = {0};", message.0))?)).unwrap();
        import(&j, exchange("stranger", "eve@example.com", "eve@example.com", None, "").as_bytes());
        let again = crate::import::import_raw(&j, AccountId(1), INBOX, &[mail!("reply-exchange.eml").as_slice(), b"\r\n"].concat()).unwrap();
        assert_ne!(again, message);
        assert_eq!(file(&j), None, "the same mail synced again under a new id");
        assert_eq!(mailboxes(&j, again), [INBOX]);

        let google = import(&j, mail!("reply-google.eml"));
        let done = file(&j).expect("Google's reply filed");
        assert_eq!(mailboxes(&j, google), [ARCHIVE]);
        let (op, _) = j.store.read(|c| store::op(c, done.op)).unwrap().unwrap();
        j.store.write(|tx| actions::give_up(tx, &op, "the server refused")).unwrap();
        assert_eq!(mailboxes(&j, google), [INBOX], "the archive failed for good");
        assert_eq!(file(&j), None, "and isn't tried again");

        let two = [exchange("a", "bo@example.com", "bo@example.com", None, ""), exchange("b", "Cy <bo@example.com>", "bo@example.com", None, "")];
        for raw in &two {
            import(&j, raw.as_bytes());
        }
        let done = file(&j).expect("both filed");
        assert_eq!(done.text, "Filed 2 responses");
        assert_eq!(j.store.read(|c| store::op(c, done.op)).unwrap().unwrap().0.messages.len(), 2, "in one op");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn only_replies_arriving_while_the_setting_is_on() {
        let (j, calendar) = setup("setting");
        meeting(&j, calendar, ME, &[ME], "");
        j.store.write(|tx| store::set_file_responses(tx, false, 0)).unwrap();
        assert!(!filed(&j, &exchange("off", "bo@example.com", "bo@example.com", None, "")), "off");
        let same_second = exchange("same", "bo@example.com", "bo@example.com", None, "");
        import(&j, same_second.as_bytes());
        let on = Utc.with_ymd_and_hms(2026, 11, 4, 10, 30, 0).unwrap().timestamp();
        j.store.write(|tx| store::set_file_responses(tx, true, on)).unwrap();
        assert_eq!(file(&j), None, "turning it on files no mail already there, also from the same second");
        let later = exchange("later", "bo@example.com", "bo@example.com", None, "").replace("10:30:00", "10:40:00");
        assert!(filed(&j, &later));
        j.store.write(|tx| store::set_file_responses(tx, true, now().timestamp())).unwrap();
        assert_eq!(j.store.read(store::file_responses_since).unwrap(), Some(on), "on again keeps its start");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn only_replies_to_a_meeting_the_receiving_account_organizes() {
        let (j, calendar) = setup("me");
        meeting(&j, calendar, "me2@example.net", &[ME], &format!("ATTENDEE:mailto:{ME}\r\n"));
        assert!(!filed(&j, &exchange("other-account", "bo@example.com", "bo@example.com", None, "")), "another of the user's accounts organizes it");
        meeting(&j, calendar, "ann@example.com", &[ME], &format!("ATTENDEE:mailto:{ME}\r\n"));
        assert!(!filed(&j, &exchange("ann", "bo@example.com", "bo@example.com", Some("ann@example.com"), "")), "Ann's meeting");
        meeting(&j, calendar, "me.alias@example.com", &[ME, "me.alias@example.com"], "");
        assert!(filed(&j, &exchange("alias", "bo@example.com", "bo@example.com", Some("me.alias@example.com"), "")), "the account's alias organizes it");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    /// Google marks a shared calendar's owner as the copy's `self`, so on
    /// a colleague's calendar their own meeting looks like the user's.
    #[test]
    fn a_shared_calendars_owner_is_no_alias() {
        for (access, source, remote_id) in [("write", "caldav", "/cal/work/"), ("owner", "google", "ann@example.com")] {
            let (j, calendar) = setup(&format!("shared-{source}"));
            j.store.write(|tx| Ok(tx.execute("UPDATE calendar SET access = ?1, source = ?2, remote_id = ?3", [access, source, remote_id])?)).unwrap();
            meeting(&j, calendar, "ann@example.com", &[ME, "ann@example.com"], "");
            assert!(!filed(&j, &exchange("ann", "bo@example.com", "bo@example.com", Some("ann@example.com"), "")), "Ann's meeting on her {source} calendar shared with {access}");
            std::fs::remove_dir_all(&j.data_dir).unwrap();
        }
    }

    #[test]
    fn only_a_guests_reply() {
        let (j, calendar) = setup("guest");
        meeting(&j, calendar, ME, &[ME], "");
        assert!(!filed(&j, &exchange("forged", "eve@example.com", "bo@example.com", None, "")), "Eve answers in Bo's name");
        assert!(!filed(&j, &exchange("eve", "eve@example.com", "eve@example.com", None, "")), "Eve isn't invited");
        assert!(!filed(&j, &exchange("ann", "bo@example.com", "bo@example.com", Some("ann@example.com"), "")), "another organizer with the same UID");
        assert!(!filed(&j, &exchange("apple", "noreply@email.apple.com", "bo@example.com", Some(ME), "")), "iCloud's sender, seen only on organizers' invitations");
        assert!(filed(&j, &exchange("google", "calendar-notification@google.com", "bo@example.com", Some(ME), "")), "Google sending for Bo");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_reply_for_one_occurrence() {
        let (j, calendar) = setup("occurrence");
        let ics = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{UID}\r\nDTSTART:20261113T090000Z\r\nDTEND:20261113T094500Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Design crit\r\n\
             ORGANIZER:mailto:{ME}\r\nATTENDEE;CN=Bo Nilsson:mailto:bo@example.com\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:{UID}\r\nRECURRENCE-ID:20261127T090000Z\r\n\
             DTSTART:20261127T100000Z\r\nDTEND:20261127T104500Z\r\nSUMMARY:Design crit\r\nORGANIZER:mailto:{ME}\r\nATTENDEE:mailto:bo@example.com\r\n\
             ATTENDEE:mailto:cy@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        let events = crate::calendar::ical::read(&ics, &[ME.to_string()]).unwrap();
        j.store.write(|tx| store::apply_item(tx, calendar, "crit.ics", None, &events, None, &Horizon::around(now(), Brussels))).unwrap();
        let unchanged = "RECURRENCE-ID:20261120T090000Z\r\n";
        assert!(filed(&j, &exchange("unchanged", "bo@example.com", "bo@example.com", None, unchanged)), "an occurrence the series places");
        let moved = "RECURRENCE-ID;TZID=Europe/Brussels:20261127T100000\r\n";
        assert!(filed(&j, &exchange("moved", "cy@example.com", "cy@example.com", None, moved)), "a guest of the moved occurrence only");
        assert!(!filed(&j, &exchange("stranger", "eve@example.com", "eve@example.com", None, unchanged)), "a stranger's");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn a_reply_before_its_meeting_is_known_waits_a_day() {
        let (j, calendar) = setup("wait");
        let early = import(&j, exchange("early", "bo@example.com", "bo@example.com", None, "").as_bytes());
        assert_eq!(file(&j), None);
        meeting(&j, calendar, ME, &[ME], "");
        assert!(file(&j).is_some(), "filed once the calendar has the meeting");
        assert_eq!(mailboxes(&j, early), [ARCHIVE]);

        j.store.write(|tx| Ok(tx.execute_batch("DELETE FROM event")?)).unwrap();
        let late = import(&j, exchange("late", "bo@example.com", "bo@example.com", None, "").as_bytes());
        assert_eq!(file_new(&j, AccountId(1), now() + Duration::days(2)).unwrap(), None);
        meeting(&j, calendar, ME, &[ME], "");
        assert_eq!(file(&j), None, "settled after a day");
        assert_eq!(mailboxes(&j, late), [INBOX]);
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
