//! The invitation card's HTML. Every text in it comes from whoever sent the
//! invitation, so every text is escaped; the card itself is Jolta's markup
//! and skips the mail sanitiser.

use std::fmt::Write;

use super::escape;
use crate::calendar::Response;
use crate::calendar::card::{Answering, Block, Card, Edit, EventCard, Join, ReplyCard, Strip, answer_word};

pub fn card(card: &Card) -> String {
    match card {
        Card::Event(e) => event(e),
        Card::Reply(r) => reply(r),
    }
}

fn event(c: &EventCard) -> String {
    let mut h = format!(
        "<section class=\"jolta-invite{}{}\"{}><div class=\"jolta-invite-head\"><span class=\"jolta-invite-label\">{CALENDAR}{}</span>",
        if c.cancelled { " jolta-invite-cancelled" } else { "" },
        if c.outdated { " jolta-invite-outdated" } else { "" },
        c.answering.as_ref().map(|a| a.message).or(c.removing.as_ref().map(|r| r.message)).map(|m| format!(" data-rsvp=\"{}\"", m.0)).unwrap_or_default(),
        c.label
    );
    // The chosen button says it when the user can answer.
    if let Some(answer) = c.answer.filter(|_| c.answering.is_none()) {
        let _ = write!(h, "<span class=\"jolta-invite-answer\">{}</span>", answer_word(answer));
    }
    h.push_str(&copy_link(c));
    let _ = write!(h, "</div><div class=\"jolta-invite-title\">{}</div>", escape(&c.title));
    match (&c.changes.when, &c.when) {
        (Some(e), _) => {
            let _ = write!(h, "<div class=\"jolta-invite-when\">{}</div>", edit(e));
        }
        (None, Some(when)) => {
            let _ = write!(h, "<div class=\"jolta-invite-when\">{}</div>", escape(when));
        }
        (None, None) => {}
    }
    if let Some(other) = &c.elsewhere {
        let _ = write!(h, "<div class=\"jolta-invite-elsewhere\">{}</div>", escape(other));
    }
    let facts: Vec<String> = [(REPEAT, &c.repeat, &c.changes.repeat), (PLACE, &c.location, &c.changes.location), (VIDEO, &None, &c.changes.join)]
        .into_iter()
        .filter_map(|(icon, text, changed)| match (changed, text) {
            (Some(e), _) => Some(format!("<span>{icon}<span class=\"jolta-edit\">{}</span></span>", edit(e))),
            (None, Some(t)) => Some(format!("<span>{icon}{}</span>", escape(t))),
            (None, None) => None,
        })
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
    let mut answers = c.answering.as_ref().map(|a| answers(a, c.answer)).unwrap_or_default();
    if let Some(r) = &c.removing {
        let what = r.occurrence.as_ref().map_or_else(|| "this meeting".to_string(), |(_, day)| format!("{day}\u{2019}s meeting"));
        let _ = write!(
            answers,
            "<a class=\"jolta-remove\" href=\"x-rsvp:{}/remove\" title=\"Take {} out of your calendar without telling the organizer\">Remove from calendar</a>",
            r.message.0,
            escape(&what)
        );
    }
    let join = c.join.as_ref().map(join).unwrap_or_default();
    if !answers.is_empty() || !join.is_empty() {
        let _ = write!(h, "<div class=\"jolta-invite-actions\">{answers}{join}</div>");
    }
    if let Some(a) = &c.answering {
        h.push_str(&options(a));
    }
    h.push_str("</section>");
    h
}

/// Add to calendar or Update in your calendar, in the head's free corner:
/// both come only with the answer buttons, which take the badge's place.
fn copy_link(c: &EventCard) -> String {
    match (&c.adding, &c.updating) {
        (Some(a), _) => format!(
            "<a class=\"jolta-add\" href=\"x-rsvp:{}/add\" title=\"Put this meeting in one of your calendars without telling anyone\">Add to calendar{CHEVRON}</a>",
            a.message.0
        ),
        (_, Some(u)) => format!(
            "<a class=\"jolta-update\" href=\"x-rsvp:{}/update\" title=\"Write this version into {} without telling anyone\">Update in your calendar</a>",
            u.message.0,
            escape(&u.calendar)
        ),
        _ => String::new(),
    }
}

/// A changed line: the old part struck through, the new one marked. A
/// line the update added has no old part, one it took away no new one.
fn edit(e: &Edit) -> String {
    let mut h = escape(&e.lead);
    if !e.old.is_empty() {
        let _ = write!(h, "<del>{}</del>", escape(&e.old));
    }
    if !e.old.is_empty() && !e.new.is_empty() {
        h.push(' ');
    }
    if !e.new.is_empty() {
        let _ = write!(h, "<ins>{}</ins>", escape(&e.new));
    }
    h.push_str(&escape(&e.tail));
    h
}

/// Accept, Maybe and Decline as `x-rsvp:<message>/<answer>` links with
/// their keys; the answer given reads as done ("Accepted"). The data
/// attributes let the app mark an answer while it goes out.
fn answers(a: &Answering, given: Option<Response>) -> String {
    let mut h = String::from("<span class=\"jolta-rsvp\" role=\"group\" aria-label=\"Your answer\">");
    for (name, label, done, key, response) in [
        ("accept", "Accept", "Accepted", 'Y', Response::Accepted),
        ("maybe", "Maybe", "Maybe", 'M', Response::Tentative),
        ("decline", "Decline", "Declined", 'N', Response::Declined),
    ] {
        let chosen = given == Some(response);
        let _ = write!(
            h,
            "<a class=\"jolta-rsvp-{name}{}\" href=\"x-rsvp:{}/{name}\" title=\"{label} ({key})\" aria-pressed=\"{chosen}\" \
             data-answer=\"{name}\" data-label=\"{label}\" data-done=\"{done}\">{CHECK}<span>{}</span><kbd>{key}</kbd></a>",
            if chosen { " jolta-chosen" } else { "" },
            a.message.0,
            if chosen { done } else { label }
        );
    }
    h.push_str("</span>");
    h
}

/// The choices that change how the next answer goes out. The app keeps
/// them and marks them here while the card is on screen.
fn options(a: &Answering) -> String {
    let id = a.message.0;
    let mut h = format!(
        "<div class=\"jolta-rsvp-options\"><a class=\"jolta-rsvp-note\" href=\"x-rsvp:{id}/note\">Add a note</a>\
         <a class=\"jolta-rsvp-quiet\" href=\"x-rsvp:{id}/quiet\" title=\"Set your answer without telling {}\">{BOX}Don&apos;t send a reply</a>",
        escape(if a.organizer.is_empty() { "the organizer" } else { &a.organizer })
    );
    if let Some(o) = a.occurrence.as_ref().filter(|o| !o.alone) {
        let _ = write!(
            h,
            "<a class=\"jolta-rsvp-scope\" href=\"x-rsvp:{id}/scope\" title=\"Answer the whole series or only {}\"><span>Whole series</span>{CHEVRON}</a>",
            escape(&o.day)
        );
    }
    h.push_str("</div>");
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
        let next = s.busy.iter().filter(|o| o.lane == b.lane && o.left > b.left).map(|o| o.left).fold(100.0, f32::min);
        h.push_str(&block(class, b, HOURS_ROW + f32::from(b.lane) * LANE, next));
    }
    h.push_str(&block("jolta-strip-this", &s.this, this_top, 100.0));
    h.push_str("</div>");
    h
}

/// Narrower than this, in percent of the strip, a block's title doesn't fit
/// inside it and goes beside it, when its row has that much room.
const LABEL_MIN: f32 = 9.0;

/// `next` is where the next block in the row starts.
fn block(class: &str, b: &Block, top: f32, next: f32) -> String {
    let title = escape(&b.title);
    let inside = b.width >= LABEL_MIN;
    let mut h = format!(
        "<span class=\"{class}\" style=\"left:{:.2}%;width:{:.2}%;top:{top}px\" title=\"{title}\">{}</span>",
        b.left,
        b.width,
        if inside { title.as_str() } else { "" }
    );
    let (end, room) = (b.left + b.width, next - b.left - b.width);
    if !inside && room >= LABEL_MIN && !title.is_empty() {
        let _ = write!(h, "<span class=\"{class} jolta-strip-beside\" style=\"left:{end:.2}%;max-width:{room:.2}%;top:{top}px\">{title}</span>");
    }
    h
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

const CHECK: &str = "<svg width=\"11\" height=\"11\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"2.6\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M5 12.5l4.5 4.5L19 7.5\"/></svg>";

const BOX: &str = "<svg width=\"11\" height=\"11\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"2\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><rect x=\"4\" y=\"4\" width=\"16\" height=\"16\" rx=\"4\"/>\
<path class=\"jolta-tick\" d=\"M8 12.3l2.8 2.8L16.2 9\"/></svg>";

const CHEVRON: &str = "<svg width=\"9\" height=\"9\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"2.6\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M6 9.5l6 6 6-6\"/></svg>";

const REPEAT: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M5 11V9.5A3.5 3.5 0 0 1 8.5 6H19m-3-3 3 3-3 3\"/>\
<path d=\"M19 13v1.5a3.5 3.5 0 0 1-3.5 3.5H5m3 3-3-3 3-3\"/></svg>";

const PLACE: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M12 20.5s-6-5.4-6-10.2a6 6 0 0 1 12 0c0 4.8-6 10.2-6 10.2z\"/>\
<circle cx=\"12\" cy=\"10.3\" r=\"2.2\"/></svg>";

const VIDEO: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><rect x=\"3.5\" y=\"6.5\" width=\"12\" height=\"11\" rx=\"2.2\"/>\
<path d=\"M15.5 10.5l5-3v9l-5-3\"/></svg>";

const PEOPLE: &str = "<svg width=\"12\" height=\"12\" viewBox=\"0 0 24 24\" aria-hidden=\"true\" fill=\"none\" stroke=\"currentColor\" \
stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><circle cx=\"9\" cy=\"9\" r=\"3.2\"/>\
<path d=\"M3.5 19c.6-3 2.8-4.6 5.5-4.6s4.9 1.6 5.5 4.6M15.5 6.2a3 3 0 0 1 0 5.6M17.5 14.8c1.6.6 2.6 2 3 4.2\"/></svg>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::card::{Availability, Changes, Note, ShownOccurrence};
    use crate::model::MessageId;
    use crate::render::{MessageView, render_thread};
    use crate::smime::Protection;

    fn hostile() -> EventCard {
        let block = |title: &str, conflict| Block { left: 40.0, width: 15.0, lane: 0, title: title.into(), conflict };
        EventCard {
            label: "Invitation",
            title: "<img src=x onerror=alert(1)> & co".into(),
            cancelled: false,
            when: Some("Thu 12 Nov · 16:00 to 16:45".into()),
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
            changes: Default::default(),
            outdated: false,
            notes: vec![Note { text: "You haven't written to this sender before".into(), warn: true }],
            answer: Some(Response::NeedsAction),
            availability: Some(Availability {
                text: "Overlaps with <script>".into(),
                conflict: true,
                strip: Some(Strip { hours: vec![(0.0, "14:00".into())], busy: vec![block("<i>Standup</i>", true)], lanes: 1, this: block("x", false) }),
            }),
            answering: None,
            removing: None,
            adding: None,
            updating: None,
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
        assert!(html.contains("<span class=\"jolta-invite-answer\">Not answered</span>"), "no buttons on this card, so the badge says it");
    }

    #[test]
    fn add_and_update_are_links_the_app_handles() {
        use crate::calendar::add::{Adding, Updating};
        let message = crate::MessageId(7);
        let add = card(&Card::Event(EventCard { adding: Some(Adding { message, uid: "u".into() }), ..hostile() }));
        assert!(add.contains("<a class=\"jolta-add\" href=\"x-rsvp:7/add\""), "{add}");
        let calendar = "<b>Work</b>".to_string();
        let update = card(&Card::Event(EventCard { updating: Some(Updating { message, uid: "u".into(), calendar }), ..hostile() }));
        assert!(update.contains("href=\"x-rsvp:7/update\"") && update.contains(">Update in your calendar</a>"), "{update}");
        assert!(!update.contains("<b>Work"), "the calendar's name is escaped");
    }

    #[test]
    fn an_update_strikes_the_old_value_and_marks_the_new() {
        let changes = Changes {
            when: Some(Edit { lead: "Thu 12 Nov · ".into(), old: "16:00 to 17:00".into(), new: "14:00 to 15:00".into(), tail: String::new() }),
            location: Some(Edit { old: "<Room A>".into(), ..Edit::default() }),
            join: Some(Edit { new: "New Zoom link".into(), ..Edit::default() }),
            repeat: None,
        };
        let html = card(&Card::Event(EventCard { changes, location: None, outdated: true, ..hostile() }));
        assert!(html.contains("<div class=\"jolta-invite-when\">Thu&#32;12&#32;Nov&#32;·&#32;<del>16:00&#32;to&#32;17:00</del> <ins>14:00&#32;to&#32;15:00</ins></div>"), "{html}");
        assert!(html.contains("<span class=\"jolta-edit\"><del>&lt;Room&#32;A&gt;</del></span>"), "a place taken away: {html}");
        assert!(html.contains("<span class=\"jolta-edit\"><ins>New&#32;Zoom&#32;link</ins></span>"));
        assert!(html.starts_with("<section class=\"jolta-invite jolta-invite-outdated\""));
    }

    #[test]
    fn the_answer_buttons_show_the_answer_given() {
        let answering = Answering { message: MessageId(7), uid: "u".into(), occurrence: None, organizer: "Ann <b>".into() };
        let with = |answer: Option<Response>, answering: Option<Answering>| card(&Card::Event(EventCard { answer, answering, join: None, ..hostile() }));
        let chosen = |html: &str| html.matches("jolta-chosen").count();

        let open = with(Some(Response::NeedsAction), Some(answering.clone()));
        for (name, key) in [("accept", 'Y'), ("maybe", 'M'), ("decline", 'N')] {
            assert!(open.contains(&format!("href=\"x-rsvp:7/{name}\"")), "{open}");
            assert!(open.contains(&format!("<kbd>{key}</kbd>")));
        }
        assert_eq!((chosen(&open), open.matches("aria-pressed=\"false\"").count()), (0, 3), "nothing chosen yet");
        assert!(!open.contains("jolta-invite-answer"), "the buttons say it, not a badge");
        assert!(open.contains("href=\"x-rsvp:7/note\">Add a note</a>") && open.contains("href=\"x-rsvp:7/quiet\""));
        assert!(open.contains("without telling Ann&#32;&lt;b&gt;"), "{open}");
        assert!(!open.contains("x-rsvp:7/scope"), "a single event has no series to choose from");
        assert!(open.find("jolta-rsvp-accept").unwrap() < open.find("jolta-rsvp-options").unwrap(), "options under the buttons");

        let maybe = with(Some(Response::Tentative), Some(answering.clone()));
        assert_eq!(chosen(&maybe), 1);
        assert!(maybe.contains("<a class=\"jolta-rsvp-maybe jolta-chosen\" href=\"x-rsvp:7/maybe\" title=\"Maybe (M)\" aria-pressed=\"true\""), "{maybe}");
        assert!(maybe.contains("<span>Accept</span>") && maybe.contains("<span>Decline</span>"));
        let accepted = with(Some(Response::Accepted), Some(answering.clone()));
        assert!(accepted.contains("<span>Accepted</span>") && !accepted.contains("<span>Accept</span>"), "the answer given reads as done");

        let series = Answering {
            occurrence: Some(ShownOccurrence { recurrence_id: crate::calendar::Moment::Date(chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap()), day: "Thu 5 Nov".into(), alone: false }),
            ..answering.clone()
        };
        let series = with(None, Some(series));
        assert!(series.contains("href=\"x-rsvp:7/scope\" title=\"Answer the whole series or only Thu&#32;5&#32;Nov\"><span>Whole series</span>"), "{series}");
        let alone = Answering {
            occurrence: Some(ShownOccurrence { recurrence_id: crate::calendar::Moment::Date(chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap()), day: "Thu 5 Nov".into(), alone: true }),
            ..answering
        };
        assert!(!with(None, Some(alone)).contains("x-rsvp:7/scope"), "an invitation to one occurrence has no series to choose");

        let own = with(Some(Response::Accepted), None);
        assert!(!own.contains("x-rsvp:") && own.contains("<span class=\"jolta-invite-answer\">Accepted</span>"), "no buttons, the badge says it");
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
    fn a_short_block_has_its_title_beside_it() {
        let block = |left: f32, width: f32, lane: u8, title: &str| Block { left, width, lane, title: title.into(), conflict: false };
        let strip = Strip {
            hours: Vec::new(),
            busy: vec![block(10.0, 5.0, 0, "Coffee"), block(17.0, 20.0, 0, "Design sync"), block(60.0, 5.0, 0, "Call")],
            lanes: 1,
            this: block(45.0, 5.0, 0, "Team standup"),
        };
        let html = super::strip(&strip);
        assert!(html.contains(">Team&#32;standup</span>"), "{html}");
        assert!(html.contains("left:50.00%;max-width:50.00%"), "the rest of its row: {html}");
        assert!(html.contains(">Call</span>"), "nothing after it in its row");
        assert!(!html.contains(">Coffee</span>"), "Design sync starts 2% later, no room: {html}");
        assert!(html.contains(">Design&#32;sync</span>"), "wide enough for its title inside");
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
