//! Round trips through a real IMAP server (Dovecot) and SMTP catcher
//! (Mailpit). Start them with `scripts/test-servers.sh up`; without them
//! these tests skip.

use futures::TryStreamExt;
use jolta_ffi::{ListContext, ThreadAction};

use crate::fixture::{App, Mail, block_on, now, png, span};
use crate::server::{Mailbox, available, caught, wait};

fn inbox_has(app: &App, subject: &str) -> bool {
    app.subjects("in:inbox").iter().any(|s| s == subject)
}

/// The first sync brings in every folder, and IDLE brings new mail within
/// seconds.
#[test]
fn first_sync_and_new_mail() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "Welcome", "First message."));
    server.append("Sent", &Mail::new(&server.user, "bob@test.invalid", "Sent earlier", "From before Jolta."));
    let app = App::unstarted();
    let account = server.sign_in(&app);

    wait(20, "the inbox synced", || inbox_has(&app, "Welcome"));
    wait(20, "Sent synced", || app.subjects("in:sent") == ["Sent earlier"]);
    let roles: Vec<String> = app.core.mailboxes(account.id.0).unwrap().into_iter().filter_map(|m| m.role).collect();
    for role in ["inbox", "sent", "drafts", "archive", "trash", "spam"] {
        assert!(roles.iter().any(|r| r == role), "the {role} folder was recognised");
    }

    server.append("INBOX", &Mail::new("carl@test.invalid", &server.user, "Fresh news", "Arrived while Jolta ran."));
    wait(20, "IDLE brought the new message", || inbox_has(&app, "Fresh news"));
    assert!(app.events.seen("new mail"), "the app was told about new mail");
}

/// Archive, read, flag and trash in Jolta reach the server, and undo after
/// sending puts things back there too.
#[test]
fn actions_reach_the_server() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    for subject in ["Keep", "File me", "Bin me"] {
        server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, subject, "Body."));
    }
    let app = App::unstarted();
    server.sign_in(&app);
    wait(20, "the inbox synced", || app.subjects("in:inbox").len() == 3);
    let thread = |subject: &str| app.list("in:inbox").into_iter().find(|r| r.subject == subject).unwrap().thread_id;

    app.core.perform(ThreadAction::MarkRead, vec![thread("Keep")], ListContext::Inbox).unwrap();
    app.core.perform(ThreadAction::Flag, vec![thread("Keep")], ListContext::Inbox).unwrap();
    wait(20, "the flags reached the server", || {
        let flags = server.flags("INBOX", "Keep");
        flags.iter().any(|f| f.contains("Seen")) && flags.iter().any(|f| f.contains("Flagged"))
    });

    let archived = app.core.perform(ThreadAction::Archive, vec![thread("File me")], ListContext::Inbox).unwrap();
    wait(20, "the archive reached the server", || server.subjects("Archive") == ["File me"]);
    app.core.undo(archived).unwrap();
    wait(20, "the undo reached the server", || server.subjects("Archive").is_empty() && server.subjects("INBOX").contains(&"File me".to_string()));

    app.core.perform(ThreadAction::Trash, vec![thread("Bin me")], ListContext::Inbox).unwrap();
    wait(20, "the trash reached the server", || server.subjects("Trash") == ["Bin me"]);
    let account = app.core.accounts().unwrap()[0].id;
    let trash = app.core.mailboxes(account).unwrap().into_iter().find(|m| m.role.as_deref() == Some("trash")).unwrap().id;
    app.core.empty_mailboxes(vec![trash]).unwrap();
    wait(20, "Trash was emptied on the server", || server.subjects("Trash").is_empty());
    assert!(!app.events.seen("action failed"));
}

/// Undo while the worker is moving the message: the move lands on the
/// server, then the opposite move brings the message back, there and in
/// Jolta, and the sync after the move never takes it out of the inbox.
#[test]
fn undo_while_the_archive_is_on_its_way() {
    if !available() {
        return;
    }
    use jolta_core::actions::{self, Action, Context};
    use jolta_core::{ThreadId, store};
    let server = Mailbox::new();
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "File me", "Body."));
    let app = App::unstarted();
    let account = server.add(&app);
    block_on(jolta_core::imap::sync_account(&app.jolta, &account, jolta_core::imap::SyncOptions { bodies: false }, |_| {})).expect("the first sync");
    let thread = app.list("in:inbox")[0].thread_id;
    app.jolta.store.write(|tx| actions::perform(tx, ThreadId(thread), Action::Archive, Context::Inbox)).unwrap().expect("archived");
    let claimed = app.jolta.store.write(|tx| store::claim_op(tx, account.id)).unwrap().expect("the worker claims the archive");
    app.jolta.store.write(|tx| actions::undo(tx, claimed.id)).unwrap();
    assert_eq!(app.subjects("in:inbox"), ["File me"]);
    block_on(async {
        let mut session = jolta_core::imap::connect_account(&account).await?;
        let caps = jolta_core::imap::caps(&mut session).await?;
        jolta_core::imap::apply_op(&app.jolta, &mut session, caps, &claimed).await
    })
    .expect("the claimed move reaches the server");
    app.jolta.store.write(|tx| store::finish_op(tx, claimed.id)).unwrap();
    assert_eq!(server.subjects("Archive"), ["File me"], "the archive landed after the undo");

    app.core.start().unwrap();
    wait(20, "the undo reached the server", || server.subjects("Archive").is_empty() && server.subjects("INBOX") == ["File me"]);
    app.core.sync_now();
    wait(20, "nothing is left to send", || app.count("SELECT count(*) FROM op WHERE state IN ('pending', 'sending')") == 0);
    wait(20, "the next sync keeps it in the inbox", || app.subjects("in:inbox") == ["File me"]);
    assert!(app.subjects("in:archive").is_empty());
    assert!(!app.events.seen("action failed"));
}

/// The archive lands on the server but its OK never arrives, so the op
/// waits again as failed; Undo then keeps it and queues the opposite move,
/// which brings the message back to the inbox on the server and in Jolta.
#[test]
fn undo_after_the_archives_ok_was_lost() {
    if !available() {
        return;
    }
    use jolta_core::actions::{self, Action, Context};
    use jolta_core::{ThreadId, store};
    let server = Mailbox::new();
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "File me", "Body."));
    let app = App::unstarted();
    let account = server.add(&app);
    block_on(jolta_core::imap::sync_account(&app.jolta, &account, jolta_core::imap::SyncOptions { bodies: false }, |_| {})).expect("the first sync");
    let thread = app.list("in:inbox")[0].thread_id;
    app.jolta.store.write(|tx| actions::perform(tx, ThreadId(thread), Action::Archive, Context::Inbox)).unwrap().expect("archived");
    let claimed = app.jolta.store.write(|tx| store::claim_op(tx, account.id)).unwrap().expect("the worker claims the archive");
    block_on(async {
        let mut session = jolta_core::imap::connect_account(&account).await?;
        let caps = jolta_core::imap::caps(&mut session).await?;
        jolta_core::imap::apply_op(&app.jolta, &mut session, caps, &claimed).await
    })
    .expect("the move reaches the server");
    assert_eq!(server.subjects("Archive"), ["File me"]);
    app.jolta.store.write(|tx| store::fail_attempt(tx, claimed.id, "the connection dropped before the OK", false)).unwrap();
    app.jolta.store.write(|tx| actions::undo(tx, claimed.id)).unwrap().expect("Undo queues the opposite move");
    assert_eq!(app.subjects("in:inbox"), ["File me"]);

    app.core.start().unwrap();
    wait(20, "the undo reached the server", || server.subjects("Archive").is_empty() && server.subjects("INBOX") == ["File me"]);
    app.core.sync_now();
    wait(20, "nothing is left to send", || app.count("SELECT count(*) FROM op WHERE state IN ('pending', 'sending')") == 0);
    wait(20, "the next sync keeps it in the inbox", || app.subjects("in:inbox") == ["File me"]);
    assert!(app.subjects("in:archive").is_empty());
    assert!(!app.events.seen("action failed"));
}

/// Changes made elsewhere (a phone, the web) show up in Jolta.
#[test]
fn changes_from_elsewhere_arrive() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    for subject in ["Star me", "Delete me", "Read me"] {
        server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, subject, "Body."));
    }
    let app = App::unstarted();
    server.sign_in(&app);
    wait(20, "the inbox synced", || app.subjects("in:inbox").len() == 3);

    server.store("INBOX", "Star me", "+FLAGS (\\Flagged)");
    server.store("INBOX", "Read me", "+FLAGS (\\Seen)");
    server.store("INBOX", "Delete me", "+FLAGS (\\Deleted)");
    app.core.sync_now();
    wait(20, "the flag arrived", || app.subjects("is:starred") == ["Star me"]);
    wait(20, "the read state arrived", || app.subjects("in:inbox is:read") == ["Read me"]);
    wait(20, "the deletion arrived", || !inbox_has(&app, "Delete me"));
}

/// A sent message leaves after the undo window, reaches the SMTP server
/// with threading headers, and its copy lands in Sent.
#[test]
fn sending_reaches_smtp_and_sent() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    let question = Mail::new("ann@test.invalid", &server.user, &format!("Question {}", now()), "Are you coming?");
    server.append("INBOX", &question);
    let app = App::unstarted();
    server.sign_in(&app);
    wait(20, "the inbox synced", || inbox_has(&app, &question.subject));

    let thread = app.list("in:inbox")[0].thread_id;
    let mut reply = crate::fixture::block_on(app.core.reply_compose(thread, jolta_ffi::ReplyMode::Reply, 0)).unwrap();
    reply.body = format!("Yes, see you there.\n\n{}", reply.body);
    app.core.send(reply, None, None).unwrap();

    let subject = format!("Re: {}", question.subject);
    wait(30, "Mailpit caught the reply", || caught(&subject).contains("\"MessageID\""));
    let json = caught(&subject);
    assert!(json.contains("ann@test.invalid"), "the reply went to the sender: {json}");
    wait(20, "the copy reached Sent on the server", || server.subjects("Sent") == [subject.clone()]);
    wait(20, "the reply joined the conversation", || app.list("in:inbox")[0].message_count == 2);
    assert!(app.events.seen("sent"));
}

/// Snoozing moves the thread to a Snoozed folder on the server, so phones
/// hide it too, and brings it back at its time.
#[test]
fn snooze_round_trip() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "Later please", "Body."));
    let app = App::unstarted();
    server.sign_in(&app);
    wait(20, "the inbox synced", || inbox_has(&app, "Later please"));

    let thread = app.list("in:inbox")[0].thread_id;
    app.core.perform(ThreadAction::Snooze { until: now() + 4 }, vec![thread], ListContext::Inbox).unwrap();
    wait(20, "the snooze reached the server", || server.subjects("Snoozed") == ["Later please"]);
    wait(30, "the thread woke up", || inbox_has(&app, "Later please"));
    wait(20, "the wake reached the server", || server.subjects("INBOX") == ["Later please"] && server.subjects("Snoozed").is_empty());
}

/// A draft saved in Jolta appears in the server's Drafts, and discarding it
/// removes it there.
#[test]
fn drafts_reach_the_server() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    let app = App::unstarted();
    let account = server.sign_in(&app);
    wait(20, "the first sync finished", || app.events.seen("synced"));

    let mut compose = app.core.new_compose(None).unwrap();
    compose.account_id = Some(account.id.0);
    compose.to = "bob@test.invalid".into();
    compose.subject = "Half written".into();
    compose.body = "Dear Bob,".into();
    let id = app.core.save_draft(compose).unwrap();
    app.core.sync_now();
    wait(20, "the draft reached the server", || server.subjects("Drafts") == ["Half written"]);
    app.core.discard_draft(id).unwrap();
    app.core.sync_now();
    wait(20, "the draft left the server", || server.subjects("Drafts").is_empty());
}

/// A draft saved in Jolta opens again from Drafts as it was written, and
/// saving it replaces its server copy. Opened again right after, before
/// Drafts has synced, it is still the same draft. A draft written elsewhere
/// opens too, saving it replaces that copy, and sending it removes it.
#[test]
fn drafts_reopen_from_drafts() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    let question = Mail::new("ann@test.invalid", &server.user, "Numbers", "Can you send them?");
    server.append("INBOX", &question);
    server.append("Drafts", &Mail::new(&server.user, "bob@test.invalid", "From my phone", "Started on the phone."));
    let app = App::unstarted();
    server.sign_in(&app);
    wait(20, "the inbox synced", || inbox_has(&app, "Numbers"));
    let drafts_thread = |subject: &str| app.list("in:drafts").into_iter().find(|r| r.subject == subject).map(|r| r.thread_id);
    // Opening a conversation's draft never makes a second one: it is this draft as last saved, or none while Drafts catches up.
    let reopen = |thread: i64, id: i64, subject: &str| {
        if let Some(c) = block_on(app.core.open_draft(thread)).unwrap() {
            assert_eq!((c.draft_id, c.subject.as_str()), (Some(id), subject), "the same draft, as saved");
        }
    };

    let thread = app.list("in:inbox")[0].thread_id;
    let mut reply = block_on(app.core.reply_compose(thread, jolta_ffi::ReplyMode::Reply, 0)).unwrap();
    reply.body = format!("**Almost** there.{}", reply.body);
    reply.bcc = "carl@test.invalid".into();
    let id = app.core.save_draft(reply.clone()).unwrap();
    wait(20, "the reply draft came back in Drafts", || drafts_thread("Re: Numbers").is_some());
    let reply_thread = drafts_thread("Re: Numbers").unwrap();

    let mut opened = block_on(app.core.open_draft(reply_thread)).unwrap().expect("a draft");
    assert_eq!(opened.draft_id, Some(id), "the draft Jolta saved");
    assert_eq!((opened.body.as_str(), opened.bcc.as_str()), (reply.body.as_str(), "carl@test.invalid"), "as it was written");
    assert_eq!(opened.in_reply_to, reply.in_reply_to);
    opened.subject = "Re: Numbers, attached".into();
    app.core.save_draft(opened).unwrap();
    wait(20, "the server copy was replaced", || server.subjects("Drafts") == ["From my phone", "Re: Numbers, attached"]);
    reopen(reply_thread, id, "Re: Numbers, attached");
    wait(20, "Drafts shows the new copy", || drafts_thread("Re: Numbers, attached").is_some());
    reopen(reply_thread, id, "Re: Numbers, attached");
    assert!(drafts_thread("Re: Numbers").is_none(), "and not the old one");

    wait(20, "the phone's draft synced", || drafts_thread("From my phone").is_some());
    let phone_thread = drafts_thread("From my phone").unwrap();
    let mut phone = block_on(app.core.open_draft(phone_thread)).unwrap().expect("a draft");
    assert_eq!((phone.to.as_str(), phone.body.as_str()), ("bob@test.invalid", "Started on the phone."));
    phone.subject = "From my Mac".into();
    app.core.save_draft(phone.clone()).unwrap();
    wait(20, "the phone's copy was replaced", || server.subjects("Drafts") == ["From my Mac", "Re: Numbers, attached"]);
    reopen(phone_thread, phone.draft_id.unwrap(), "From my Mac");
    assert_eq!(app.core.drafts().unwrap().len(), 2, "two drafts, however often they open");

    app.core.send(phone, None, None).unwrap();
    wait(40, "sending removed the draft", || server.subjects("Drafts") == ["Re: Numbers, attached"]);
}

/// A pasted image goes to the server with its draft, shown inline there, and
/// opening the draft again from Drafts brings it back where it was.
#[test]
fn a_pasted_image_comes_back_with_its_draft() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    let app = App::unstarted();
    let account = server.sign_in(&app);
    wait(20, "the first sync finished", || app.events.seen("synced"));

    let path = app.core.save_pasted_image(png()).unwrap();
    let mut compose = app.core.new_compose(None).unwrap();
    compose.account_id = Some(account.id.0);
    compose.to = "bob@test.invalid".into();
    compose.subject = "With a chart".into();
    compose.body = app.core.body_markdown(vec![span("See:\n", None), span("Pasted image.png", Some(&path)), span("\nBye", None)]);
    compose.image_size = jolta_ffi::ImageSizeChoice::Large;
    let id = app.core.save_draft(compose).unwrap();
    app.core.sync_now();
    let thread = || app.list("in:drafts").into_iter().find(|r| r.subject == "With a chart").map(|r| r.thread_id);
    wait(20, "the draft came back in Drafts", || thread().is_some());

    let opened = block_on(app.core.open_draft(thread().unwrap())).unwrap().expect("a draft");
    assert_eq!(opened.draft_id, Some(id));
    assert_eq!(opened.image_size, jolta_ffi::ImageSizeChoice::Large, "and its image size");
    let spans = app.core.body_spans(opened.body);
    let texts: Vec<_> = spans.iter().map(|s| (s.text.as_str(), s.image.as_deref())).collect();
    assert_eq!(texts, [("See:\n", None), ("Pasted image.png", Some(path.as_str())), ("\nBye", None)]);

    let raw = server.with(async |s| {
        s.select("Drafts").await.unwrap();
        let fetched: Vec<_> = s.fetch("1", "BODY.PEEK[]").await.unwrap().try_collect().await.unwrap();
        fetched[0].body().unwrap().to_vec()
    });
    let msg = mail_parser::MessageParser::new().parse(&raw).unwrap();
    let image = msg.parts().find(|p| p.content_id().is_some()).expect("the server copy carries the image");
    assert_eq!(&image.decoded()[..], &png()[..]);
    assert!(msg.body_html(0).unwrap().contains(&format!("cid:{}", image.content_id().unwrap())));
}

/// Found on 2026-11-09: a saved draft reached the server's Drafts, but
/// Jolta's Drafts, which lists the server's copy, stayed empty until Sync
/// now or the next full sync ten minutes later. Discarding takes it out
/// the same way.
#[test]
fn a_saved_draft_shows_in_drafts_without_sync_now() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    let app = App::unstarted();
    let account = server.sign_in(&app);
    wait(20, "the first sync finished", || app.events.seen("synced"));

    let mut compose = app.core.new_compose(None).unwrap();
    compose.account_id = Some(account.id.0);
    compose.to = "bob@test.invalid".into();
    compose.subject = "Half written".into();
    compose.body = "Dear Bob,".into();
    let id = app.core.save_draft(compose).unwrap();
    wait(20, "the draft shows in Drafts", || app.subjects("in:drafts") == ["Half written"]);
    app.core.discard_draft(id).unwrap();
    wait(20, "the draft left Drafts", || app.subjects("in:drafts").is_empty());
}

fn count(app: &App, event: &str) -> usize {
    app.events.0.lock().unwrap().iter().filter(|e| **e == event).count()
}

/// Found on 2026-11-09: when Gmail closed an idle connection, the next
/// round failed with "peer closed connection", and new mail IDLE reported
/// in that round, and the error in the status line, stayed until the next
/// full sync, up to ten minutes later.
#[test]
fn new_mail_after_the_server_drops_the_connection() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    let app = App::unstarted();
    server.sign_in(&app);
    wait(20, "the first sync finished", || app.events.seen("synced"));

    server.drop_connections();
    // IDLE reconnects after its two-second wait; the worker's connection stays closed.
    std::thread::sleep(std::time::Duration::from_secs(4));
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "After the drop", "Body."));
    wait(20, "the new mail shows", || inbox_has(&app, "After the drop"));
    wait(20, "the account is in sync again", || {
        let events = app.events.0.lock().unwrap();
        let failed = events.iter().rposition(|e| *e == "sync failed");
        failed.is_some_and(|f| events[f..].contains(&"synced"))
    });
}

/// Found on 2026-11-09: while every round failed, the worker waited out its
/// backoff, up to five minutes, before trying again, so a message sent then
/// stayed in the outbox past its undo window until Sync now.
#[test]
fn a_message_leaves_on_time_while_syncing_fails() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "Question", "Are you coming?"));
    let app = App::unstarted();
    let account = server.sign_in(&app);
    wait(20, "the inbox synced", || inbox_has(&app, "Question"));

    // A Drafts folder that is gone from the server fails every round that saves a draft.
    app.jolta.store.write(|tx| Ok(tx.execute("UPDATE mailbox SET remote_id = 'Gone' WHERE role = 'drafts'", [])?)).unwrap();
    let mut draft = app.core.new_compose(None).unwrap();
    draft.account_id = Some(account.id.0);
    draft.subject = "Never saved".into();
    app.core.save_draft(draft).unwrap();
    wait(30, "four rounds failed", || count(&app, "sync failed") >= 4);

    let thread = app.list("in:inbox")[0].thread_id;
    let mut reply = crate::fixture::block_on(app.core.reply_compose(thread, jolta_ffi::ReplyMode::Reply, 0)).unwrap();
    reply.subject = format!("On time {}", now());
    let sent_at = std::time::Instant::now();
    app.core.send(reply.clone(), None, None).unwrap();
    wait(40, "Mailpit caught the reply", || caught(&reply.subject).contains("\"MessageID\""));
    let took = sent_at.elapsed().as_secs_f64();
    assert!(took < 14.0, "the message left {took:.1} s after Send; the undo window is 10 s");
}

/// A connection that goes silent without closing, as when a router forgets
/// it, held up every action and message behind it: the worker waited for
/// an answer that never came, until macOS gave up on the connection
/// minutes later.
#[test]
fn actions_and_mail_go_out_after_the_connection_goes_silent() {
    if !available() {
        return;
    }
    let relay = crate::server::Relay::start();
    let server = Mailbox::new();
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "Question", "Are you coming?"));
    let app = App::unstarted();
    let account = server.add(&app);
    app.jolta
        .store
        .write(|tx| Ok(tx.execute("UPDATE account SET imap_port = ?1 WHERE id = ?2", rusqlite::params![relay.port, account.id.0])?))
        .unwrap();
    app.core.start().unwrap();
    wait(20, "the inbox synced", || inbox_has(&app, "Question"));

    relay.freeze();
    let thread = app.list("in:inbox")[0].thread_id;
    app.core.perform(ThreadAction::Flag, vec![thread], ListContext::Inbox).unwrap();
    let mut reply = crate::fixture::block_on(app.core.reply_compose(thread, jolta_ffi::ReplyMode::Reply, 0)).unwrap();
    reply.subject = format!("Through the silence {}", now());
    app.core.send(reply.clone(), None, None).unwrap();
    wait(90, "the flag reached the server", || server.flags("INBOX", "Question").iter().any(|f| f.contains("Flagged")));
    wait(30, "Mailpit caught the reply", || caught(&reply.subject).contains("\"MessageID\""));
}

/// Found in review: a message that fell due while a failing round still
/// ran, here one waiting on Dovecot's slow answer about a Drafts folder
/// that is gone, waited out the retry wait earlier failures built up,
/// though the next round would send it before failing again.
#[test]
fn a_message_due_during_a_failing_round_leaves_when_it_ends() {
    if !available() {
        return;
    }
    let relay = crate::server::Relay::start();
    let server = Mailbox::new();
    server.append("INBOX", &Mail::new("ann@test.invalid", &server.user, "Question", "Are you coming?"));
    let app = App::unstarted();
    let account = server.add(&app);
    app.jolta
        .store
        .write(|tx| Ok(tx.execute("UPDATE account SET imap_port = ?1 WHERE id = ?2", rusqlite::params![relay.port, account.id.0])?))
        .unwrap();
    app.core.start().unwrap();
    wait(20, "the inbox synced", || inbox_has(&app, "Question"));

    app.jolta.store.write(|tx| Ok(tx.execute("UPDATE mailbox SET remote_id = 'Gone' WHERE role = 'drafts'", [])?)).unwrap();
    let mut draft = app.core.new_compose(None).unwrap();
    draft.account_id = Some(account.id.0);
    draft.subject = "Never saved".into();
    app.core.save_draft(draft).unwrap();
    wait(40, "four rounds failed", || count(&app, "sync failed") >= 4);

    relay.slow_select(std::time::Duration::from_secs(15));
    let thread = app.list("in:inbox")[0].thread_id;
    let mut reply = crate::fixture::block_on(app.core.reply_compose(thread, jolta_ffi::ReplyMode::Reply, 0)).unwrap();
    reply.subject = format!("Due mid-round {}", now());
    let sent_at = std::time::Instant::now();
    app.core.send(reply.clone(), None, None).unwrap();
    wait(120, "Mailpit caught the reply", || caught(&reply.subject).contains("\"MessageID\""));
    let took = sent_at.elapsed().as_secs_f64();
    assert!(took < 25.0, "the message left {took:.1} s after Send; the round it fell due in failed after about 15 s");
}
