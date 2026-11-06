use jolta_core::MailboxRole;
use jolta_ffi::{ListContext, ThreadAction};

use crate::fixture::{App, Mail, now};

const PENDING: &str = "SELECT count(*) FROM op WHERE state = 'pending'";

/// A morning of triage: read a conversation, archive one, change your mind,
/// trash another, empty the trash.
#[test]
fn triage_an_inbox() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let ask = Mail::new("ann@test.invalid", "me@test.invalid", "Quarterly numbers", "Can you send the Q3 numbers?").at(now() - 7200);
    let answer = Mail::reply(&ask, "me@test.invalid", "Attached, see the second tab.");
    let thanks = Mail::reply(&answer, "ann@test.invalid", "Got them, thanks!");
    let first = app.deliver(&me, MailboxRole::Inbox, &ask);
    app.deliver(&me, MailboxRole::Sent, &answer);
    app.deliver(&me, MailboxRole::Inbox, &thanks);
    let lunch = app.deliver(&me, MailboxRole::Inbox, &Mail::new("bob@test.invalid", "me@test.invalid", "Lunch?", "Friday at noon?"));
    let (numbers, lunch) = (app.thread_of(first), app.thread_of(lunch));

    let inbox = app.list("in:inbox");
    assert_eq!(inbox.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(), ["Lunch?", "Re: Quarterly numbers"]);
    assert_eq!(inbox[1].message_count, 3, "the reply from Sent joins the conversation");

    let view = app.open(numbers);
    for text in ["Can you send the Q3 numbers?", "Attached, see the second tab.", "Got them, thanks!"] {
        assert!(view.html.contains(text), "the reader shows {text:?}");
    }

    let read = app.core.perform(ThreadAction::MarkRead, vec![numbers.0], ListContext::Inbox).unwrap();
    assert_eq!(app.list("in:inbox is:unread").len(), 1);
    app.core.undo(read).unwrap();
    assert_eq!(app.list("in:inbox is:unread").len(), 2);
    assert_eq!(app.count(PENDING), 0, "undoing an unsent change drops it instead of sending its opposite");

    app.events.0.lock().unwrap().clear();
    let archived = app.core.perform(ThreadAction::Archive, vec![lunch.0], ListContext::Inbox).unwrap();
    assert!(app.events.seen("lists"), "the app is told to reload its lists");
    assert_eq!(app.subjects("in:inbox"), ["Re: Quarterly numbers"]);
    assert_eq!(app.subjects("in:archive"), ["Lunch?"]);
    assert_eq!(app.count(PENDING), 1);
    app.core.undo(archived).unwrap();
    assert_eq!(app.subjects("in:inbox"), ["Lunch?", "Re: Quarterly numbers"]);
    assert!(app.subjects("in:archive").is_empty());
    assert_eq!(app.count(PENDING), 0);

    app.core.perform(ThreadAction::Trash, vec![numbers.0], ListContext::Inbox).unwrap();
    assert_eq!(app.subjects("in:inbox"), ["Lunch?"]);
    assert_eq!(app.subjects("in:trash"), ["Re: Quarterly numbers"]);
    let trash = app.mailbox(me.id, MailboxRole::Trash);
    app.core.empty_mailboxes(vec![trash.0]).unwrap();
    assert!(app.subjects("in:trash").is_empty());
    assert_eq!(app.subjects("in:sent"), ["Re: Quarterly numbers"], "emptying Trash leaves the sent copy alone");
    assert!(!app.events.seen("action failed"));
}

/// Label… on Gmail adds a label and leaves the conversation in the inbox,
/// choosing it again takes the label off, and undo reverses each. On an
/// IMAP account there are no labels to offer.
#[test]
fn label_a_gmail_conversation() {
    let app = App::new();
    let work = app.gmail_account("work@test.invalid");
    let ask = Mail::new("client@test.invalid", "work@test.invalid", "Kickoff", "Monday at ten?").at(now() - 3600);
    let first = app.deliver_gmail(&work, MailboxRole::Inbox, &ask);
    app.deliver_gmail(&work, MailboxRole::Inbox, &Mail::reply(&ask, "client@test.invalid", "Or Tuesday."));
    let kickoff = app.thread_of(first);
    let clients = app.named_mailbox(work.id, "Clients");
    let in_clients = format!("mailbox:{}", clients.0);

    let account = app.core.accounts().unwrap().into_iter().find(|a| a.id == work.id.0).unwrap();
    assert!(account.has_labels);
    let names = |labels: Vec<jolta_ffi::LabelChoice>| labels.into_iter().map(|l| (l.name, l.applied)).collect::<Vec<_>>();
    assert_eq!(names(app.core.thread_labels(kickoff.0).unwrap()), [("Clients".into(), false), ("Receipts".into(), false)]);

    let added = app.core.perform(ThreadAction::AddLabel { mailbox_id: clients.0 }, vec![kickoff.0], ListContext::Inbox).unwrap();
    assert_eq!(app.subjects("in:inbox"), ["Re: Kickoff"], "still in the inbox");
    assert_eq!(app.subjects(&in_clients), ["Re: Kickoff"], "and in Clients");
    assert_eq!(app.list(&in_clients)[0].message_count, 2);
    assert_eq!(names(app.core.thread_labels(kickoff.0).unwrap())[0], ("Clients".into(), true));
    assert_eq!(app.count("SELECT count(*) FROM op WHERE state = 'pending' AND kind = 'move'"), 1, "one op for the server");
    app.core.undo(added).unwrap();
    assert!(app.subjects(&in_clients).is_empty());
    assert_eq!(app.subjects("in:inbox"), ["Re: Kickoff"]);
    assert_eq!(app.count(PENDING), 0);

    app.core.perform(ThreadAction::AddLabel { mailbox_id: clients.0 }, vec![kickoff.0], ListContext::Inbox).unwrap();
    let removed = app.core.perform(ThreadAction::RemoveLabel { mailbox_id: clients.0 }, vec![kickoff.0], ListContext::Inbox).unwrap();
    assert!(app.subjects(&in_clients).is_empty(), "the label is off");
    assert_eq!(app.subjects("in:inbox"), ["Re: Kickoff"], "still in the inbox");
    app.core.undo(removed).unwrap();
    assert_eq!(app.subjects(&in_clients), ["Re: Kickoff"], "undo puts the label back");
    assert_eq!(app.subjects("in:inbox"), ["Re: Kickoff"]);

    let home = app.account("home@test.invalid");
    let note = app.deliver(&home, MailboxRole::Inbox, &Mail::new("ann@test.invalid", "home@test.invalid", "Dinner", "At eight."));
    let account = app.core.accounts().unwrap().into_iter().find(|a| a.id == home.id.0).unwrap();
    assert!(!account.has_labels);
    let refused = app.core.thread_labels(app.thread_of(note).0).map(|_| ()).unwrap_err().to_string();
    assert!(refused.contains("Labels are a Gmail feature"), "{refused}");
    assert!(!app.events.seen("action failed"));
}

/// Flags and "mark all as read" act on what the list shows, and undo
/// restores exactly what changed.
#[test]
fn flags_and_mark_all_read() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    for (i, subject) in ["One", "Two", "Three"].iter().enumerate() {
        app.deliver(&me, MailboxRole::Inbox, &Mail::new("ann@test.invalid", "me@test.invalid", subject, "Body.").at(now() - 600 * i as i64));
    }
    let two = app.list("in:inbox").into_iter().find(|r| r.subject == "Two").unwrap();
    app.core.perform(ThreadAction::Flag, vec![two.thread_id], ListContext::Inbox).unwrap();
    assert_eq!(app.subjects("is:starred"), ["Two"]);

    let tokens = app.core.mark_all_read("in:inbox".into()).unwrap();
    assert!(app.list("is:unread").is_empty());
    app.core.undo(tokens).unwrap();
    assert_eq!(app.list("is:unread").len(), 3);
    assert_eq!(app.subjects("is:starred"), ["Two"], "undoing read leaves the flag alone");
}

/// Snoozed mail leaves the inbox at once and comes back at its time.
#[test]
fn snooze_and_wake() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let message = app.deliver(&me, MailboxRole::Inbox, &Mail::new("ann@test.invalid", "me@test.invalid", "Later", "Remind me."));
    let thread = app.thread_of(message);
    app.core.perform(ThreadAction::Snooze { until: now() + 60 }, vec![thread.0], ListContext::Inbox).unwrap();
    assert!(app.subjects("in:inbox").is_empty());
    assert_eq!(app.core.snoozed().unwrap().len(), 1);

    let due = app.jolta.store.read(|c| jolta_core::store::due_snoozes(c, me.id, now() + 120)).unwrap();
    assert_eq!(due.len(), 1);
    app.jolta
        .store
        .write(|tx| {
            for (snooze, thread) in &due {
                jolta_core::actions::wake(tx, *snooze, *thread)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(app.subjects("in:inbox"), ["Later"]);
    assert!(app.core.snoozed().unwrap().is_empty());
}

/// A snoozed thread trashed from Snoozed leaves "Next to wake" and stays in
/// the Trash at its time; undoing the trash brings the snooze back.
#[test]
fn trashing_a_snoozed_thread_cancels_its_wake_up() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let message = app.deliver(&me, MailboxRole::Inbox, &Mail::new("ann@test.invalid", "me@test.invalid", "Later", "Remind me."));
    let thread = app.thread_of(message);
    app.core.perform(ThreadAction::Snooze { until: now() + 60 }, vec![thread.0], ListContext::Inbox).unwrap();
    let snoozed = || ListContext::Role { role: "snoozed".into() };

    let tokens = app.core.perform(ThreadAction::Trash, vec![thread.0], snoozed()).unwrap();
    assert!(app.core.snoozed().unwrap().is_empty(), "Next to wake drops it");
    app.core.undo(tokens).unwrap();
    assert_eq!(app.core.snoozed().unwrap().len(), 1, "undoing the trash brings it back");

    app.core.perform(ThreadAction::Trash, vec![thread.0], snoozed()).unwrap();
    let due = app.jolta.store.read(|c| jolta_core::store::due_snoozes(c, me.id, now() + 120)).unwrap();
    let woke = app
        .jolta
        .store
        .write(|tx| Ok(due.iter().map(|(snooze, thread)| jolta_core::actions::wake(tx, *snooze, *thread)).collect::<Result<Vec<_>, _>>()?))
        .unwrap();
    assert_eq!(woke.len(), 1);
    assert!(woke[0].is_none(), "nothing wakes");
    assert!(app.subjects("in:inbox").is_empty());
    assert_eq!(app.subjects("in:trash"), ["Later"]);
    let due = app.jolta.store.read(|c| jolta_core::store::due_snoozes(c, me.id, now() + 120)).unwrap();
    assert!(due.is_empty(), "the retired snooze is not due again");
}
