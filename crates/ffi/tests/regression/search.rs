use kite_core::MailboxRole;
use kite_ffi::{ListContext, ThreadAction};

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

/// "Needs reply" shows a message AI marked until the user answers it, and a
/// category term finds what AI sorted there.
#[test]
fn needs_reply_until_answered() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let ask = Mail::new("ann@test.invalid", "me@test.invalid", "Can you review?", "Before Thursday please.").at(now() - 7200);
    let message = app.deliver(&me, MailboxRole::Inbox, &ask);
    let bill = app.deliver(&me, MailboxRole::Inbox, &Mail::new("bank@test.invalid", "me@test.invalid", "Statement", "Nothing to do."));
    app.kite
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
