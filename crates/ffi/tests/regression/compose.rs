use jolta_core::MailboxRole;
use jolta_ffi::ReplyMode;

use crate::fixture::{App, Mail, block_on, now, png, span};

fn addresses(field: &str) -> Vec<String> {
    let mut out: Vec<String> = field
        .split(',')
        .filter_map(|a| {
            let a = a.trim();
            let a = a.rsplit_once('<').map_or(a, |(_, rest)| rest.trim_end_matches('>'));
            (!a.is_empty()).then(|| a.to_lowercase())
        })
        .collect();
    out.sort();
    out
}

/// A reply threads under the newest message, keeps the subject without a
/// second "Re:", and quotes what it answers.
#[test]
fn reply_threads_correctly() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let ask = Mail::new("Ann Peeters <ann@test.invalid>", "me@test.invalid", "Quarterly numbers", "Can you send them?").at(now() - 7200);
    let answer = Mail::reply(&ask, "me@test.invalid", "Here they are.");
    let thanks = Mail::reply(&answer, "Ann Peeters <ann@test.invalid>", "Thanks a lot!");
    let first = app.deliver(&me, MailboxRole::Inbox, &ask);
    app.deliver(&me, MailboxRole::Sent, &answer);
    app.deliver(&me, MailboxRole::Inbox, &thanks);

    let reply = block_on(app.core.reply_compose(app.thread_of(first).0, ReplyMode::Reply, 3600)).unwrap();
    assert_eq!(addresses(&reply.to), ["ann@test.invalid"]);
    assert_eq!(reply.subject, "Re: Quarterly numbers");
    let bare = |m: &Mail| m.id.trim_matches(['<', '>']).to_string();
    assert_eq!(reply.in_reply_to, Some(bare(&thanks)));
    assert_eq!(reply.references, [bare(&ask), bare(&answer), bare(&thanks)]);
    assert!(reply.body.contains("> Thanks a lot!"), "the reply quotes the message it answers");
}

/// Reply all leaves out the user's own address and lists each other person
/// once, even when they appear in both To and Cc.
#[test]
fn reply_all_lists_everyone_once() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let mail = Mail::new("ann@test.invalid", "Me <ME@test.invalid>, carl@test.invalid", "Plans", "Thoughts?")
        .cc("dana@test.invalid, Carl <carl@test.invalid>, ann@test.invalid");
    let message = app.deliver(&me, MailboxRole::Inbox, &mail);

    let reply = block_on(app.core.reply_compose(app.thread_of(message).0, ReplyMode::ReplyAll, 0)).unwrap();
    assert_eq!(addresses(&reply.to), ["ann@test.invalid"]);
    assert_eq!(addresses(&reply.cc), ["carl@test.invalid", "dana@test.invalid"]);
}

/// Replying to your own message goes to the people you wrote to.
#[test]
fn reply_to_own_message_goes_to_its_recipients() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let sent = app.deliver(&me, MailboxRole::Sent, &Mail::new("me@test.invalid", "bob@test.invalid", "Following up", "Any news?"));
    let reply = block_on(app.core.reply_compose(app.thread_of(sent).0, ReplyMode::Reply, 0)).unwrap();
    assert_eq!(addresses(&reply.to), ["bob@test.invalid"]);
}

/// A reply draft waiting in Drafts isn't what Reply answers: the reply goes
/// to the newest message that isn't a draft.
#[test]
fn reply_skips_the_conversations_draft() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let ask = Mail::new("Ann <ann@test.invalid>", "me@test.invalid", "Portfolio", "Which photos should go in?").at(now() - 7200);
    let draft = Mail::reply(&ask, "me@test.invalid", "Half-written answer");
    let first = app.deliver(&me, MailboxRole::Inbox, &ask);
    app.deliver(&me, MailboxRole::Drafts, &draft);

    let thread = app.thread_of(first).0;
    let reply = block_on(app.core.reply_compose(thread, ReplyMode::Reply, 0)).unwrap();
    assert_eq!(addresses(&reply.to), ["ann@test.invalid"]);
    assert_eq!(reply.in_reply_to, Some(ask.id.trim_matches(['<', '>']).to_string()));
    assert!(reply.body.contains("> Which photos should go in?"), "the reply quotes Ann's message, not the draft");
    let forward = block_on(app.core.reply_compose(thread, ReplyMode::Forward, 0)).unwrap();
    assert!(!forward.body.contains("Half-written answer"), "a forward doesn't carry the draft either");
}

/// A forward carries the original attachments and doesn't thread.
#[test]
fn forward_carries_attachments() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let mail = Mail::new("ann@test.invalid", "me@test.invalid", "Invoice", "See attached.").attach("invoice.txt", "total 12");
    let message = app.deliver(&me, MailboxRole::Inbox, &mail);
    let forward = block_on(app.core.reply_compose(app.thread_of(message).0, ReplyMode::Forward, 0)).unwrap();
    assert_eq!(forward.subject, "Fwd: Invoice");
    assert!(forward.to.is_empty());
    assert_eq!(forward.in_reply_to, None);
    assert_eq!(forward.attachments.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["invoice.txt"]);
    assert_eq!(std::fs::read_to_string(&forward.attachments[0].path).unwrap().trim(), "total 12");
}

/// Undo send hands the message back unchanged; a scheduled send shows in
/// the scheduled list until it is cancelled.
#[test]
fn undo_send_and_scheduled_send() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let mut compose = app.core.new_compose(None).unwrap();
    compose.account_id = Some(me.id.0);
    compose.to = "bob@test.invalid".into();
    compose.subject = "Hello".into();
    compose.body = "Hi Bob,\n\nSee you Friday.".into();

    let token = app.core.send(compose.clone(), None, None).unwrap();
    assert_eq!(app.count("SELECT count(*) FROM outbox WHERE state = 'queued'"), 1);
    let back = app.core.cancel_send(token).unwrap().expect("the message comes back");
    assert_eq!((back.to.as_str(), back.subject.as_str()), ("bob@test.invalid", "Hello"));
    assert!(back.body.contains("See you Friday."));
    assert_eq!(app.count("SELECT count(*) FROM outbox WHERE state = 'queued'"), 0);

    let later = now() + 86_400;
    let token = app.core.send(compose, Some(later), None).unwrap();
    let scheduled = app.core.scheduled().unwrap();
    assert_eq!(scheduled.len(), 1);
    assert_eq!((scheduled[0].subject.as_str(), scheduled[0].send_at), ("Hello", later));
    app.core.cancel_send(token).unwrap();
    assert!(app.core.scheduled().unwrap().is_empty());
}

/// A pasted image stays where it was put through autosave and reopening the
/// draft, and the sent message shows it inline from a part of its own.
#[test]
fn a_pasted_image_survives_the_draft_and_goes_out_inline() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let png = png();
    let path = app.core.save_pasted_image(png.clone()).unwrap();
    let mut compose = app.core.new_compose(None).unwrap();
    compose.account_id = Some(me.id.0);
    compose.to = "bob@test.invalid".into();
    compose.subject = "The chart".into();
    compose.body = app.core.body_markdown(vec![span("Here:\n", None), span("Pasted image.png", Some(&path)), span("\nThanks", None)]);
    compose.image_size = jolta_ffi::ImageSizeChoice::Medium;
    let id = app.core.save_draft(compose).unwrap();

    let sizes = app.core.message_sizes(jolta_ffi::ComposeData { account_id: None, ..app.core.draft(id).unwrap().unwrap() }).unwrap();
    assert_eq!(sizes.len(), 4, "the size menu shows before a sender is chosen");

    let reopened = app.core.draft(id).unwrap().expect("the draft is there");
    assert_eq!(reopened.image_size, jolta_ffi::ImageSizeChoice::Medium, "the draft keeps its image size");
    let spans = app.core.body_spans(reopened.body.clone());
    let images: Vec<_> = spans.iter().filter_map(|s| Some((s.text.as_str(), s.image.as_deref()?))).collect();
    assert_eq!(images, [("Pasted image.png", path.as_str())]);
    assert_eq!(std::fs::read(&path).unwrap(), png);

    app.core.send(reopened, None, None).unwrap();
    let blob: String = app.jolta.store.read(|c| Ok(c.query_row("SELECT blob FROM outbox", [], |r| r.get(0))?)).unwrap();
    let raw = app.jolta.blobs.get(&blob).unwrap();
    let msg = mail_parser::MessageParser::new().parse(&raw).unwrap();
    let image = msg.parts().find(|p| p.content_id().is_some()).expect("the image has its own part");
    assert_eq!(&image.decoded()[..], &png[..]);
    let html = msg.body_html(0).unwrap();
    assert!(html.contains(&format!("src=\"cid:{}\"", image.content_id().unwrap())), "{html}");
    assert!(msg.body_text(0).unwrap().replace("\r\n", "\n").contains("Here:\n[image: Pasted image.png]\nThanks"));
}

/// A draft survives saving twice (one row, the newest text) and goes away
/// when discarded.
#[test]
fn drafts_save_and_discard() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let mut compose = app.core.new_compose(None).unwrap();
    compose.account_id = Some(me.id.0);
    compose.subject = "Draft".into();
    compose.body = "First try.".into();
    let id = app.core.save_draft(compose.clone()).unwrap();
    compose.draft_id = Some(id);
    compose.body = "Second try.".into();
    assert_eq!(app.core.save_draft(compose).unwrap(), id);
    let drafts = app.core.drafts().unwrap();
    assert_eq!(drafts.len(), 1);
    assert!(drafts[0].body.contains("Second try."));
    app.core.discard_draft(id).unwrap();
    assert!(app.core.drafts().unwrap().is_empty());
}

/// A reply draft written elsewhere opens from Drafts with its recipients,
/// subject, body and the message it answers. Opening it twice gives the same
/// draft, and a conversation with no draft opens none.
#[test]
fn a_draft_in_drafts_opens_for_editing() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let ask = Mail::new("Ann Peeters <ann@test.invalid>", "me@test.invalid", "Quarterly numbers", "Can you send them?");
    let asked = app.deliver(&me, MailboxRole::Inbox, &ask);
    let mut draft = Mail::reply(&ask, "me@test.invalid", "Here they are, almost.\r\n\r\n> Can you send them?");
    draft.cc = "Bob <bob@test.invalid>".into();
    let drafted = app.deliver(&me, MailboxRole::Drafts, &draft);

    let thread = app.thread_of(drafted).0;
    let compose = block_on(app.core.open_draft(thread)).unwrap().expect("the conversation holds a draft");
    assert_eq!(compose.account_id, Some(me.id.0));
    assert_eq!(addresses(&compose.to), ["ann@test.invalid"]);
    assert_eq!(addresses(&compose.cc), ["bob@test.invalid"]);
    assert_eq!(compose.subject, "Re: Quarterly numbers");
    assert_eq!(compose.body, "Here they are, almost.\n\n> Can you send them?", "with the editor's line breaks, not the message's CRLF");
    let bare = |m: &Mail| m.id.trim_matches(['<', '>']).to_string();
    assert_eq!(compose.in_reply_to, Some(bare(&ask)));
    assert_eq!(compose.references, [bare(&ask)]);
    assert_eq!(compose.replying_to, Some(asked.0), "sending it marks the question answered");
    assert!(compose.draft_id.is_some(), "saving it replaces this draft");

    let again = block_on(app.core.open_draft(thread)).unwrap().expect("still a draft");
    assert_eq!(again.draft_id, compose.draft_id, "one draft, however often it opens");
    assert_eq!(app.core.drafts().unwrap().len(), 1);

    let other = app.deliver(&me, MailboxRole::Inbox, &Mail::new("carl@test.invalid", "me@test.invalid", "Hello", "Hi."));
    assert!(block_on(app.core.open_draft(app.thread_of(other).0)).unwrap().is_none());
}
