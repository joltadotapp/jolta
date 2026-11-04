//! The invitation card's HTML. Every text in it comes from whoever sent the
//! invitation, so every text is escaped; the card itself is Jolta's markup
//! and skips the mail sanitiser.

use std::fmt::Write;

use super::escape;
use crate::calendar::card::{Block, Card, EventCard, Join, ReplyCard, Strip};

pub fn card(card: &Card) -> String {
    match card {
        Card::Event(e) => event(e),
        Card::Reply(r) => reply(r),
    }
}

fn event(c: &EventCard) -> String {
    let mut h = format!(
        "<section class=\"jolta-invite{}\"><div class=\"jolta-invite-head\"><span class=\"jolta-invite-label\">{CALENDAR}{}</span>",
        if c.cancelled { " jolta-invite-cancelled" } else { "" },
        c.label
    );
    if let Some(answer) = c.answer {
        let _ = write!(h, "<span class=\"jolta-invite-answer\">{answer}</span>");
    }
    let _ = write!(h, "</div><div class=\"jolta-invite-title\">{}</div><div class=\"jolta-invite-when\">{}</div>", escape(&c.title), escape(&c.when));
    if let Some(other) = &c.elsewhere {
        let _ = write!(h, "<div class=\"jolta-invite-elsewhere\">{}</div>", escape(other));
    }
    let facts: Vec<String> = [(REPEAT, &c.repeat), (PLACE, &c.location)]
        .into_iter()
        .filter_map(|(icon, text)| text.as_ref().map(|t| format!("<span>{icon}{}</span>", escape(t))))
        .collect();
    if !facts.is_empty() {
        let _ = write!(h, "<div class=\"jolta-invite-facts\">{}</div>", facts.concat());
    }
    let people = match (&c.organizer, &c.guests) {
        (Some(o), Some(g)) => format!("<b>{}</b> organizer · with {}", escape(o), escape(g)),
        (Some(o), None) => format!("<b>{}</b> organizer", escape(o)),
        (None, Some(g)) => format!("With {}", escape(g)),
        (None, None) => String::new(),
    };
    if !people.is_empty() {
        let _ = write!(h, "<div class=\"jolta-invite-people\">{PEOPLE}<span>{people}</span></div>");
    }
    for note in &c.notes {
        let _ = write!(h, "<div class=\"jolta-invite-note{}\">{}</div>", if note.warn { " jolta-warn" } else { "" }, escape(&note.text));
    }
    if let Some(a) = &c.availability {
        let _ = write!(h, "<div class=\"jolta-invite-free{}\">{}</div>", if a.conflict { " jolta-warn" } else { "" }, escape(&a.text));
        if let Some(s) = &a.strip {
            h.push_str(&strip(s));
        }
    }
    // Answer buttons join Join in this row.
    if let Some(j) = &c.join {
        let _ = write!(h, "<div class=\"jolta-invite-actions\">{}</div>", join(j));
    }
    h.push_str("</section>");
    h
}

const LANE: f32 = 20.0;
const HOURS_ROW: f32 = 18.0;

fn strip(s: &Strip) -> String {
    let this_top = HOURS_ROW + f32::from(s.lanes) * LANE + 4.0;
    let mut h = format!("<div class=\"jolta-strip\" style=\"height:{}px\" aria-hidden=\"true\">", this_top + LANE);
    for (i, (left, label)) in s.hours.iter().enumerate() {
        // The last line is the right edge; its label would hang outside.
        let label = if i + 1 < s.hours.len() { format!("<i>{}</i>", escape(label)) } else { String::new() };
        let _ = write!(h, "<span class=\"jolta-strip-hour\" style=\"left:{left:.2}%\">{label}</span>");
    }
    for b in &s.busy {
        let class = if b.conflict { "jolta-strip-busy jolta-conflict" } else { "jolta-strip-busy" };
        h.push_str(&block(class, b, HOURS_ROW + f32::from(b.lane) * LANE));
    }
    h.push_str(&block("jolta-strip-this", &s.this, this_top));
    h.push_str("</div>");
    h
}

/// Narrower than this, in percent of the strip, a block shows no words.
const LABEL_MIN: f32 = 9.0;

fn block(class: &str, b: &Block, top: f32) -> String {
    let title = escape(&b.title);
    let label = if b.width >= LABEL_MIN { title.as_str() } else { "" };
    format!("<span class=\"{class}\" style=\"left:{:.2}%;width:{:.2}%;top:{top}px\" title=\"{title}\">{label}</span>", b.left, b.width)
}

/// `x-join:` hands the app both links; it opens the provider's app when it
/// is installed, as the calendar's Join does.
fn join(j: &Join) -> String {
    let mut href = format!("x-join:open?url={}", percent(&j.url));
    if let Some(app) = &j.app_url {
        let _ = write!(href, "&app={}", percent(app));
    }
    format!(
        "<a class=\"jolta-join{}\" href=\"{href}\" title=\"{}\">Join {}</a>",
        if j.live { " jolta-live" } else { "" },
        escape(&j.url),
        escape(j.provider)
    )
}

fn reply(r: &ReplyCard) -> String {
    let comment = r.comment.as_ref().map(|c| format!(": \"{}\"", escape(c))).unwrap_or_default();
    format!(
        "<section class=\"jolta-invite jolta-invite-reply\">{CALENDAR}<span><b>{}</b>{comment}</span><span class=\"jolta-invite-about\">{}</span></section>",
        escape(&r.line),
        escape(&r.about)
    )
}

fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

const CALENDAR: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><rect x=\"4.5\" y=\"5.5\" width=\"15\" height=\"14\" rx=\"2\"/>\
<path d=\"M4.5 10h15M9 3.5v4M15 3.5v4\"/></svg>";

const REPEAT: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M5 11V9.5A3.5 3.5 0 0 1 8.5 6H19m-3-3 3 3-3 3\"/>\
<path d=\"M19 13v1.5a3.5 3.5 0 0 1-3.5 3.5H5m3 3-3-3 3-3\"/></svg>";

const PLACE: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M12 20.5s-6-5.4-6-10.2a6 6 0 0 1 12 0c0 4.8-6 10.2-6 10.2z\"/>\
<circle cx=\"12\" cy=\"10.3\" r=\"2.2\"/></svg>";

const PEOPLE: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><circle cx=\"9\" cy=\"9\" r=\"3.2\"/>\
<path d=\"M3.5 19c.6-3 2.8-4.6 5.5-4.6s4.9 1.6 5.5 4.6M15.5 6.2a3 3 0 0 1 0 5.6M17.5 14.8c1.6.6 2.6 2 3 4.2\"/></svg>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::card::{Availability, Note};
    use crate::render::{MessageView, render_thread};
    use crate::smime::Protection;

    fn hostile() -> EventCard {
        let block = |title: &str, conflict| Block { left: 40.0, width: 15.0, lane: 0, title: title.into(), conflict };
        EventCard {
            label: "Invitation",
            title: "<img src=x onerror=alert(1)> & co".into(),
            cancelled: false,
            when: "Thu 12 Nov · 16:00 to 16:45".into(),
            elsewhere: Some("10:00 to 10:45 New York".into()),
            repeat: None,
            location: Some("<b>Room</b>".into()),
            join: Some(Join {
                provider: "Zoom",
                url: "https://us02web.zoom.us/j/1?pwd=a\"b".into(),
                app_url: Some("zoommtg://zoom.us/join?confno=1&pwd=a".into()),
                live: true,
            }),
            organizer: Some("Ann <ann@example.com>".into()),
            guests: Some("Bo and Cy".into()),
            notes: vec![Note { text: "You haven't written to this sender before".into(), warn: true }],
            answer: Some("Not answered"),
            availability: Some(Availability {
                text: "Overlaps with <script>".into(),
                conflict: true,
                strip: Some(Strip { hours: vec![(0.0, "14:00".into())], busy: vec![block("<i>Standup</i>", true)], lanes: 1, this: block("x", false) }),
            }),
        }
    }

    #[test]
    fn every_text_is_escaped() {
        let html = card(&Card::Event(hostile()));
        for raw in ["<img", "<b>Room", "<script", "<i>Standup", "Ann <ann", "a\"b"] {
            assert!(!html.contains(raw), "{raw} got through:\n{html}");
        }
        assert!(html.contains("&lt;img&#32;src&#61;x"), "{html}");
        assert!(html.contains("href=\"x-join:open?url=https%3A%2F%2Fus02web.zoom.us%2Fj%2F1%3Fpwd%3Da%22b&app=zoommtg%3A%2F%2Fzoom.us%2Fjoin%3Fconfno%3D1%26pwd%3Da\""));
        assert!(html.contains("class=\"jolta-join jolta-live\""), "Join is the accent while the meeting runs");
        assert!(html.contains("<div class=\"jolta-invite-note jolta-warn\">You&#32;haven&apos;t"));
        assert!(html.contains("jolta-strip-busy jolta-conflict"));
        assert!(html.contains("<span class=\"jolta-invite-answer\">Not answered</span>"));
    }

    #[test]
    fn a_reply_is_one_line_and_cards_sit_outside_the_message() {
        let r = Card::Reply(ReplyCard { line: "Bo accepted".into(), comment: Some("See <you>".into()), about: "Design crit · Fri 13 Nov".into() });
        let html = card(&r);
        assert!(html.contains("<b>Bo&#32;accepted</b>: \"See&#32;&lt;you&gt;\""), "{html}");

        let raw = b"Subject: s\r\n\r\nBody text\r\n";
        let cards = [Card::Event(hostile())];
        let view = MessageView { id: 7, from: "a", to: "b", date: "d", snippet: "", raw, open: true, vip: false, protection: &Protection::None, cards: &cards };
        let page = render_thread(&[view], "").html;
        let (card_at, body_at) = (page.find("jolta-invite").unwrap(), page.find("id=\"m7\"").unwrap());
        assert!(card_at < body_at, "the card comes before the body and outside it, where the message's styles can't reach");
    }

    #[test]
    fn the_inline_calendar_part_is_no_file() {
        let raw = include_bytes!("../../tests/fixtures/invitations/google-request.eml");
        let view = MessageView { id: 1, from: "a", to: "b", date: "d", snippet: "", raw, open: true, vip: false, protection: &Protection::None, cards: &[] };
        let page = render_thread(&[view], "").html;
        assert!(page.contains("<b>invite.ics</b>"), "a calendar file stays a file");
        assert_eq!(page.matches("class=\"jolta-file\"").count(), 1, "the inline part is the card's");
    }
}
