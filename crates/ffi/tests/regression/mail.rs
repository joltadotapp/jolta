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
