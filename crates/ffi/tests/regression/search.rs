use jolta_core::MailboxRole;
use jolta_ffi::{ListContext, ThreadAction};

use crate::fixture::{App, Mail, now};

/// The search terms people type, over a small mixed mailbox.
#[test]
fn search_terms() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let day = 86_400;
    app.deliver(&me, MailboxRole::Inbox, &Mail::new("Ann <ann@test.invalid>", "me@test.invalid", "Budget review", "The spreadsheet is ready.").at(now() - 3 * day));
    app.deliver(
        &me,
        MailboxRole::Inbox,
        &Mail::new("bob@test.invalid", "me@test.invalid", "Photos", "From the trip.").attach("beach.txt", "sand").at(now() - 2 * day),
    );
    app.deliver(
        &me,
        MailboxRole::Inbox,
        &Mail::new("news@shop.invalid", "me@test.invalid", "Weekly deals", "Save 20 percent.")
            .header("List-Unsubscribe", "<mailto:leave@shop.invalid>")
            .at(now() - day),
    );
    app.deliver(&me, MailboxRole::Sent, &Mail::new("me@test.invalid", "carl@test.invalid", "Budget for Carl", "Numbers attached."));

    assert_eq!(app.subjects("budget"), ["Budget for Carl", "Budget review"]);
    assert_eq!(app.subjects("from:ann"), ["Budget review"]);
    assert_eq!(app.subjects("to:carl"), ["Budget for Carl"]);
    assert_eq!(app.subjects("subject:photos"), ["Photos"]);
    assert_eq!(app.subjects("spread"), ["Budget review"], "the last word matches as a prefix while typing");
    assert_eq!(app.subjects("has:attachment"), ["Photos"]);
    assert_eq!(app.subjects("is:newsletter"), ["Weekly deals"]);
    assert_eq!(app.subjects("in:inbox -is:newsletter"), ["Photos", "Budget review"]);
    assert_eq!(app.subjects("budget -in:sent"), ["Budget review"]);

    let photos = app.list("subject:photos")[0].thread_id;
    app.core.perform(ThreadAction::MarkRead, vec![photos], ListContext::Inbox).unwrap();
    assert_eq!(app.subjects("in:inbox is:read"), ["Photos"]);
    assert!(app.core.query_count("is:sparkly".into()).is_err(), "an unknown term is an error, not an empty list");
}

/// As on Gmail, a search leaves out mail in Trash and Spam unless it names
/// that mailbox or `in:anywhere`; Trash itself still lists everything in it.
#[test]
fn search_leaves_out_trash_and_spam() {
    let app = App::new();
    let me = app.gmail_account("me@test.invalid");
    let mail = |subject: &str| Mail::new("ann@test.invalid", "me@test.invalid", subject, "The invoice for October.");
    app.deliver_gmail(&me, MailboxRole::Inbox, &mail("Invoice kept"));
    app.deliver(&me, MailboxRole::Trash, &mail("Invoice deleted"));
    app.deliver(&me, MailboxRole::Spam, &mail("Invoice spam"));

    assert_eq!(app.subjects("invoice"), ["Invoice kept"]);
    assert_eq!(app.subjects(""), ["Invoice kept"], "All mail leaves them out too");
    assert_eq!(app.subjects("is:unread"), ["Invoice kept"]);
    assert_eq!(app.core.query_count("invoice".into()).unwrap(), 1);
    assert_eq!(app.subjects("invoice in:trash"), ["Invoice deleted"]);
    assert_eq!(app.subjects("invoice in:spam"), ["Invoice spam"]);
    assert_eq!(app.subjects("invoice in:anywhere"), ["Invoice spam", "Invoice deleted", "Invoice kept"]);
    let trash = app.mailbox(me.id, MailboxRole::Trash);
    assert_eq!(app.subjects(&format!("mailbox:{}", trash.0)), ["Invoice deleted"]);
}

/// A search typed in a list looks in that list, unless it names a mailbox
/// itself: `in:anywhere` typed in the Inbox looks everywhere, Spam included.
#[test]
fn a_search_that_names_a_mailbox_looks_there_instead_of_the_open_list() {
    let app = App::new();
    let me = app.gmail_account("me@test.invalid");
    let mail = |subject: &str| Mail::new("ann@test.invalid", "me@test.invalid", subject, "Just this once.");
    app.deliver_gmail(&me, MailboxRole::Inbox, &mail("Once in the inbox"));
    app.deliver(&me, MailboxRole::All, &mail("Once archived"));
    app.deliver(&me, MailboxRole::Spam, &mail("Once in spam"));

    let from_inbox = |search: &str| app.subjects(&jolta_ffi::search_within("in:inbox".into(), search.into()));
    assert_eq!(from_inbox("once"), ["Once in the inbox"], "plain words search the open list");
    assert_eq!(from_inbox("once in:anywhere"), ["Once in spam", "Once archived", "Once in the inbox"]);
    assert_eq!(from_inbox("once in:any"), ["Once in spam", "Once archived", "Once in the inbox"]);
    assert_eq!(from_inbox("once in:spam"), ["Once in spam"]);
    assert_eq!(from_inbox("once -in:spam"), ["Once in the inbox"], "leaving a mailbox out still searches the open list");
    let spam = app.mailbox(me.id, MailboxRole::Spam);
    assert_eq!(from_inbox(&format!("once mailbox:{}", spam.0)), ["Once in spam"]);
}

/// "Needs reply" shows a message AI marked until the user answers it, and a
/// category term finds what AI sorted there.
#[test]
fn needs_reply_until_answered() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let ask = Mail::new("ann@test.invalid", "me@test.invalid", "Can you review?", "Before Thursday please.").at(now() - 7200);
    let message = app.deliver(&me, MailboxRole::Inbox, &ask);
    let bill = app.deliver(&me, MailboxRole::Inbox, &Mail::new("bank@test.invalid", "me@test.invalid", "Statement", "Nothing to do."));
    app.jolta
        .store
        .write(|tx| {
            tx.execute("INSERT INTO ai_triage (message_id, category, needs_reply, model, at) VALUES (?1, 'work', 1, 'test', 0)", [message])?;
            tx.execute("INSERT INTO ai_triage (message_id, category, needs_reply, model, at) VALUES (?1, 'finance', 0, 'test', 0)", [bill])?;
            Ok(())
        })
        .unwrap();

    assert_eq!(app.subjects("is:needs-reply"), ["Can you review?"]);
    assert!(app.list("in:inbox").iter().any(|r| r.subject == "Can you review?" && r.needs_reply));
    assert_eq!(app.subjects("category:finance"), ["Statement"]);

    app.deliver(&me, MailboxRole::Sent, &Mail::reply(&ask, "me@test.invalid", "Done, looks good."));
    assert!(app.subjects("is:needs-reply").is_empty(), "a reply in the conversation clears it");
}
