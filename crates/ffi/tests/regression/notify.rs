//! New-mail notifications: what counts as important, what the Dock badge
//! counts, and the settings, through the calls the Mac app makes.

use jolta_core::MailboxRole;
use jolta_ffi::notify::{BadgeScope, Notice, NoticeAction, NoticeContent, NoticeLevel, NotifyScope, NotifySettings};

use crate::fixture::{App, Mail, now};
use crate::server::{Mailbox, available, wait};

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

#[test]
fn notification_settings_live_in_the_core() {
    let app = App::new();
    let defaults = NotifySettings { scope: NotifyScope::Important, sound: true, badge: BadgeScope::ImportantUnread };
    assert_eq!(app.core.notify_settings().unwrap(), defaults, "Important mail and its count by default");
    let chosen = NotifySettings { scope: NotifyScope::AllNewMail, sound: false, badge: BadgeScope::AllUnread };
    app.core.set_notify_settings(chosen).unwrap();
    assert_eq!(app.core.notify_settings().unwrap(), chosen);
    let again = jolta_ffi::Core::open(app.dir.to_string_lossy().into(), app.events.clone()).unwrap();
    assert_eq!(again.notify_settings().unwrap(), chosen, "kept in the data folder");
}

/// The Mac app kept "New mail banners" in its own defaults. A choice made
/// on purpose moves over as it was; the old default, every new message,
/// becomes Important mail, the new default for everyone.
#[test]
fn the_old_banner_setting_moves_into_the_core_once() {
    for (old, scope) in [("vips", NotifyScope::Vips), ("none", NotifyScope::Nothing), ("all", NotifyScope::Important)] {
        let app = App::new();
        assert!(app.core.migrate_new_mail_banners(old.into()).unwrap());
        assert_eq!(app.core.notify_settings().unwrap().scope, scope, "{old}");
        assert!(!app.core.migrate_new_mail_banners("vips".into()).unwrap(), "only once");
        assert_eq!(app.core.notify_settings().unwrap().scope, scope);
    }
    let app = App::new();
    let chosen = NotifySettings { scope: NotifyScope::AllNewMail, sound: true, badge: BadgeScope::AllUnread };
    app.core.set_notify_settings(chosen).unwrap();
    assert!(!app.core.migrate_new_mail_banners("none".into()).unwrap(), "a choice made in the core stays");
    assert_eq!(app.core.notify_settings().unwrap(), chosen);
}

/// The badge counts unread inbox conversations: all of them, the important
/// ones (the same as `in:inbox is:unread is:important`), or none.
#[test]
fn the_badge_counts_by_its_own_setting() {
    let app = important_mailbox();
    sorting(&app, false);
    let set = |badge| {
        let settings = app.core.notify_settings().unwrap();
        app.core.set_notify_settings(NotifySettings { badge, ..settings }).unwrap();
    };
    set(BadgeScope::AllUnread);
    assert_eq!(app.core.badge_count().unwrap(), 10);
    set(BadgeScope::ImportantUnread);
    assert_eq!(app.core.badge_count().unwrap(), 4);
    assert_eq!(app.core.query_count("in:inbox is:unread is:important".into()).unwrap(), 4, "search agrees");
    let plan = app.list("subject:plan in:inbox")[0].thread_id;
    app.core.perform(jolta_ffi::ThreadAction::MarkRead, vec![plan], jolta_ffi::ListContext::Inbox).unwrap();
    assert_eq!(app.core.badge_count().unwrap(), 3);
    set(BadgeScope::Nothing);
    assert_eq!(app.core.badge_count().unwrap(), 0);
    let scope_alone = app.core.notify_settings().unwrap();
    app.core.set_notify_settings(NotifySettings { scope: NotifyScope::Nothing, badge: BadgeScope::AllUnread, ..scope_alone }).unwrap();
    assert_eq!(app.core.badge_count().unwrap(), 9, "the badge doesn't follow the notification choice");
}

/// A banner's Archive and Mark as Read act on its conversation, with undo.
#[test]
fn banner_buttons_act_on_the_conversation() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let id = app.deliver(&me, MailboxRole::Inbox, &Mail::new("Ann <ann@test.invalid>", "me@test.invalid", "Lunch?", "Friday."));
    let banner = format!("mail-{}", app.thread_of(id).0);

    let read = app.core.notice_action(banner.clone(), NoticeAction::MarkRead).unwrap();
    assert!(!read.is_empty());
    assert!(app.subjects("in:inbox is:unread").is_empty());
    let archived = app.core.notice_action(banner.clone(), NoticeAction::Archive).unwrap();
    assert!(app.subjects("in:inbox").is_empty());
    app.core.undo(archived).unwrap();
    assert_eq!(app.subjects("in:inbox"), ["Lunch?"], "Undo brings it back");
    assert!(app.core.notice_action(banner, NoticeAction::Reply).unwrap().is_empty(), "the app starts the reply");
    assert!(app.core.notice_action("mail-summary".into(), NoticeAction::Archive).unwrap().is_empty());
}

/// Switching AI sorting changes what is important, so the badge follows at
/// once (found by review on 2026-11-10: it waited for the next mail change).
#[test]
fn the_badge_follows_switching_ai_sorting() {
    let app = App::new();
    let me = app.account("me@test.invalid");
    let asks = app.deliver(&me, MailboxRole::Inbox, &Mail::new("Fay <fay@test.invalid>", "me@test.invalid", "Can you check this?", "?"));
    app.jolta
        .store
        .write(|tx| Ok(tx.execute("INSERT INTO ai_triage (message_id, category, needs_reply, model, at) VALUES (?1, 'work', 1, 'test', 0)", [asks])?))
        .unwrap();
    let badge = || {
        app.events.1.lock().unwrap().iter().rev().find_map(|n| match n {
            Notice::Badge { count } => Some(*count),
            _ => None,
        })
    };
    wait(10, "the badge starts at 0 with sorting off", || badge() == Some(0));
    sorting(&app, true);
    app.core.set_ai_settings(app.core.ai_settings().unwrap()).unwrap();
    wait(10, "the badge counts the mail that asks for a reply", || badge() == Some(1));
}

fn shown(app: &App) -> Vec<NoticeContent> {
    app.events.1.lock().unwrap().iter().filter_map(|n| match n {
        Notice::Show { content } => Some(content.clone()),
        _ => None,
    }).collect()
}

fn withdrawn(app: &App) -> Vec<String> {
    app.events.1.lock().unwrap().iter().flat_map(|n| match n {
        Notice::Withdraw { ids } => ids.clone(),
        _ => Vec::new(),
    }).collect()
}

/// New mail reaching Dovecot: urgent mail at once, important mail after
/// the quiet spell, a burst as one summary, and a banner taken away when
/// its mail is read elsewhere.
#[test]
fn notifications_for_mail_from_the_server() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    let app = App::unstarted();
    app.core.set_vips(vec!["vera@test.invalid".into()], true).unwrap();
    server.sign_in(&app);
    wait(20, "the first sync finished", || app.events.seen("synced"));

    server.append("INBOX", &Mail::new("Vera <vera@test.invalid>", &server.user, "Call me", "When you can."));
    let sent = std::time::Instant::now();
    wait(20, "an urgent banner", || shown(&app).iter().any(|n| n.subtitle == "Call me"));
    let urgent = shown(&app).into_iter().find(|n| n.subtitle == "Call me").unwrap();
    assert_eq!((urgent.title.as_str(), urgent.level, urgent.sound), ("Vera", NoticeLevel::Urgent, true));
    assert!(sent.elapsed().as_secs() < 5, "urgent mail skips the 5-second wait");

    server.append("INBOX", &Mail::new("Vera <vera@test.invalid>", "team@test.invalid", "Team news", "For everyone."));
    wait(30, "a banner for important mail", || shown(&app).iter().any(|n| n.subtitle == "Team news"));
    let banner = shown(&app).into_iter().find(|n| n.subtitle == "Team news").unwrap();
    assert_eq!(banner.level, NoticeLevel::Normal, "a VIP not writing to me is important, not urgent");
    assert_eq!(banner.actions, [NoticeAction::Archive, NoticeAction::MarkRead, NoticeAction::Reply]);

    server.store("INBOX", "Team news", "+FLAGS (\\Seen)");
    wait(30, "the banner goes when the mail is read elsewhere", || withdrawn(&app).contains(&banner.id));

    let before = shown(&app).len();
    app.core.set_notify_settings(NotifySettings { scope: NotifyScope::AllNewMail, sound: true, badge: BadgeScope::AllUnread }).unwrap();
    for i in 0..12 {
        server.append("INBOX", &Mail::new(&format!("shop{i}@test.invalid"), &server.user, &format!("Deal {i}"), "Save."));
    }
    wait(40, "one summary for the burst", || shown(&app)[before..].iter().any(|n| n.id == "mail-summary" && n.title == "12 new messages"));
    let burst = &shown(&app)[before..];
    assert!(burst.iter().all(|n| n.id == "mail-summary"), "no banners: {:?}", burst.iter().map(|n| &n.title).collect::<Vec<_>>());
    assert!(burst.iter().all(|n| n.level == NoticeLevel::Quiet && !n.sound));
    wait(10, "the badge counts the unread inbox", || {
        app.events.1.lock().unwrap().iter().rev().find_map(|n| match n {
            Notice::Badge { count } => Some(*count),
            _ => None,
        }) == Some(13)
    });
}

/// Every new inbox message is queued for AI sorting, not only the ones
/// that got a notification (found 2026-11-10: only the first 5 of a round
/// were, and only those announced).
#[test]
fn every_new_message_is_sorted() {
    if !available() {
        return;
    }
    let server = Mailbox::new();
    for i in 0..7 {
        server.append("INBOX", &Mail::new(&format!("p{i}@test.invalid"), &server.user, &format!("Question {i}"), "?"));
    }
    let app = App::unstarted();
    app.jolta
        .store
        .write(|tx| Ok(tx.execute("INSERT INTO setting (key, value) VALUES ('ai', ?1)", [r#"{"provider":"local","auto_summary":false}"#])?))
        .unwrap();
    app.core.set_ai_background_allowed(false);
    server.sign_in(&app);
    wait(20, "the first sync finished", || app.events.seen("synced"));
    // The worker holds one job while the Mac says no; the other six wait.
    wait(10, "all seven queued", || app.core.ai_background_pending() == 6);
}
