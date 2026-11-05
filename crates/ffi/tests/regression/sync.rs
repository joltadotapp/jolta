//! Round trips through a real IMAP server (Dovecot) and SMTP catcher
//! (Mailpit). Start them with `scripts/test-servers.sh up`; without them
//! these tests skip.

use jolta_ffi::{ListContext, ThreadAction};

use crate::fixture::{App, Mail, block_on, now};
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

    let mut compose = app.core.new_compose().unwrap();
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
