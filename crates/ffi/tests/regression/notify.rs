//! New-mail notifications: what counts as important, what the Dock badge
//! counts, and the settings, through the calls the Mac app makes.

use jolta_core::MailboxRole;

use crate::fixture::{App, Mail, now};

const INVITATION: &[u8] = include_bytes!("../../../../apple/JoltaUITests/Fixtures/07-invitation.eml");

/// A mailbox with one message per importance rule, and the near misses
/// each rule must leave out.
fn important_mailbox() -> App {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let hour = 3600;
    let inbox = |mail: Mail| app.deliver(&me, MailboxRole::Inbox, &mail);

    app.core.set_vips(vec!["vip@test.invalid".into()], true).unwrap();
    inbox(Mail::new("Vera <vip@test.invalid>", "someone@test.invalid", "From a VIP", "Rule 1.").at(now() - 9 * hour));

    let plan = Mail::new("me@test.invalid", "carl@test.invalid", "Plan", "My first message.").at(now() - 8 * hour);
    app.deliver(&me, MailboxRole::Sent, &plan);
    inbox(Mail::reply(&plan, "Carl <carl@test.invalid>", "Rule 2: a reply where I wrote."));
    let ticket = Mail::new("me@test.invalid", "help@desk.test.invalid", "Printer", "It jams.").at(now() - 7 * hour);
    app.deliver(&me, MailboxRole::Sent, &ticket);
    inbox(Mail::reply(&ticket, "noreply@desk.test.invalid", "An automated answer in my thread."));

    app.deliver(&me, MailboxRole::Sent, &Mail::new("me@test.invalid", "Dana <dana@test.invalid>", "Hello Dana", "Earlier.").at(now() - 30 * hour));
    inbox(Mail::new("Dana <dana@test.invalid>", "me@test.invalid", "From someone I wrote to", "Rule 3.").at(now() - 6 * hour));
    inbox(Mail::new("Dana <dana@test.invalid>", "team@test.invalid", "Dana copies me", "Only on Cc.").cc("me@test.invalid").at(now() - 5 * hour));
    inbox(
        Mail::new("Dana <dana@test.invalid>", "me@test.invalid", "Dana's newsletter", "A list.")
            .header("List-Unsubscribe", "<mailto:leave@test.invalid>")
            .at(now() - 4 * hour),
    );
    inbox(Mail::new("Eve <eve@test.invalid>", "me@test.invalid", "From a stranger", "Never written to.").at(now() - 3 * hour));

    jolta_core::import::import_raw(&app.jolta, me.id, app.mailbox(me.id, MailboxRole::Inbox), INVITATION).unwrap();
    let asks = inbox(Mail::new("Fay <fay@test.invalid>", "me@test.invalid", "Can you check this?", "Rule 5.").at(now() - 2 * hour));
    let promo = inbox(Mail::new("Gus <gus@test.invalid>", "me@test.invalid", "Reply to win", "A promotion.").at(now() - hour));
    app.jolta
        .store
        .write(|tx| {
            tx.execute("INSERT INTO ai_triage (message_id, category, needs_reply, model, at) VALUES (?1, 'work', 1, 'test', 0)", [asks])?;
            tx.execute("INSERT INTO ai_triage (message_id, category, needs_reply, model, at) VALUES (?1, 'newsletter', 1, 'test', 0)", [promo])?;
            Ok(())
        })
        .unwrap();
    app
}

fn sorting(app: &App, on: bool) {
    let value = if on { r#"{"provider":"local"}"# } else { r#"{"provider":null}"# };
    app.jolta
        .store
        .write(|tx| Ok(tx.execute("INSERT OR REPLACE INTO setting (key, value) VALUES ('ai', ?1)", [value])?))
        .unwrap();
}

/// `is:important` is the rule the banners and the badge use: a VIP, a
/// reply where the user wrote, mail To the user from someone they wrote to,
/// an invitation, and with AI sorting on, mail the model says needs a reply.
#[test]
fn important_mail() {
    let app = important_mailbox();
    sorting(&app, false);
    let without_ai = ["From someone I wrote to", "Re: Plan", "From a VIP", "Invitation: Design crit"];
    assert_eq!(app.subjects("in:inbox is:important"), without_ai);

    sorting(&app, true);
    assert_eq!(
        app.subjects("in:inbox is:important"),
        ["Can you check this?", "From someone I wrote to", "Re: Plan", "From a VIP", "Invitation: Design crit"],
        "the model's needs a reply counts while sorting is on, but not for a newsletter"
    );
    assert_eq!(
        app.subjects("in:inbox -is:important"),
        ["Reply to win", "From a stranger", "Dana's newsletter", "Dana copies me", "Re: Printer"],
    );
    assert!(app.subjects("in:sent is:important").is_empty(), "the user's own mail is never important");
}
