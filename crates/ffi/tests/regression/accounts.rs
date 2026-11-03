use std::time::Duration;

use jolta_core::{MailboxRole, secrets};
use jolta_ffi::{ListContext, ThreadAction};

use crate::fixture::{App, Mail, now};

/// Removing an account leaves no row, no secret and no message file behind,
/// and doesn't touch the other accounts. Before 2026-11-02 its message files
/// stayed on disk for good (233 MB on the mini).
#[test]
fn removing_an_account_leaves_nothing_behind() {
    let app = App::new();
    let gone = app.account("gone@test.invalid");
    let kept = app.account("kept@test.invalid");
    secrets::set_password(&gone.secret_ref, "imap password").unwrap();
    secrets::set_password(&secrets::smtp_ref(&gone.secret_ref), "smtp password").unwrap();

    let first = Mail::new("ann@test.invalid", "gone@test.invalid", "Invoice", "Here it is.").attach("invoice.txt", "total 12");
    let message = app.deliver(&gone, MailboxRole::Inbox, &first);
    app.deliver(&gone, MailboxRole::Inbox, &Mail::reply(&first, "gone@test.invalid", "Thanks."));
    let other = app.deliver(&gone, MailboxRole::Inbox, &Mail::new("bob@test.invalid", "gone@test.invalid", "Later", "Snooze me."));
    let survivor = app.deliver(&kept, MailboxRole::Inbox, &Mail::new("carl@test.invalid", "kept@test.invalid", "Still here", "Keep me."));

    let thread = app.thread_of(message);
    app.core.perform(ThreadAction::Archive, vec![thread.0], ListContext::Inbox).unwrap();
    app.core.perform(ThreadAction::Snooze { until: now() + 86_400 }, vec![app.thread_of(other).0], ListContext::Inbox).unwrap();
    let mut draft = app.core.new_compose().unwrap();
    draft.account_id = Some(gone.id.0);
    draft.subject = "Unfinished".into();
    app.core.save_draft(draft).unwrap();
    app.jolta
        .store
        .write(|tx| {
            tx.execute("INSERT INTO ai_triage (message_id, category, needs_reply, model, at) VALUES (?1, 'work', 1, 'test', 0)", [message])?;
            tx.execute(
                "INSERT INTO ai_summary (thread_id, text, last_message, model, at) VALUES (?1, 'summary', ?2, 'test', 0)",
                rusqlite_params(thread.0, message.0),
            )?;
            Ok(())
        })
        .unwrap();

    app.core.remove_account(gone.id.0).unwrap();

    for (table, column) in account_columns(&app) {
        let left = app.count(&format!("SELECT count(*) FROM {table} WHERE {column} = {}", gone.id.0));
        assert_eq!(left, 0, "{table}.{column} still has rows of the removed account");
    }
    assert_eq!(app.count("SELECT count(*) FROM ai_triage"), 0, "sorting results of removed mail");
    assert_eq!(app.count("SELECT count(*) FROM ai_summary"), 0, "summaries of removed threads");
    assert_eq!(app.count("SELECT count(*) FROM thread_key WHERE thread_id NOT IN (SELECT id FROM thread)"), 0);
    assert_eq!(app.count("SELECT count(*) FROM snooze WHERE thread_id NOT IN (SELECT id FROM thread)"), 0);
    assert!(!secrets::exists(&gone.secret_ref).unwrap(), "the password stays in the Keychain");
    assert!(!secrets::exists(&secrets::smtp_ref(&gone.secret_ref)).unwrap(), "the SMTP password stays in the Keychain");

    let collected = app.jolta.collect_blobs(Duration::ZERO).unwrap();
    assert_eq!(collected.files, 3, "the removed account's three message files are deleted");
    assert_eq!(app.count("SELECT count(*) FROM message"), 1);
    let view = app.open(app.thread_of(survivor));
    assert!(view.html.contains("Keep me."), "the other account's mail still opens");
    assert_eq!(app.core.accounts().unwrap().len(), 1);
}

/// Message files younger than the minimum age stay, because sync writes the
/// file before the row that names it.
#[test]
fn young_message_files_are_kept() {
    let app = App::new();
    let account = app.account("me@test.invalid");
    let message = app.deliver(&account, MailboxRole::Inbox, &Mail::new("ann@test.invalid", "me@test.invalid", "Hi", "Hello."));
    app.jolta.store.write(|tx| Ok(jolta_core::store::delete_messages(tx, &[message])?)).unwrap();
    assert_eq!(app.jolta.collect_blobs(Duration::from_secs(3600)).unwrap().files, 0);
    assert_eq!(app.jolta.collect_blobs(Duration::ZERO).unwrap().files, 1);
}

fn rusqlite_params(a: i64, b: i64) -> [i64; 2] {
    [a, b]
}

/// Every table column that holds an account id, found from the schema so a
/// new table is checked without changing this test.
fn account_columns(app: &App) -> Vec<(String, String)> {
    app.jolta
        .store
        .read(|c| {
            let tables: Vec<String> = c
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'message_fts%' AND name NOT LIKE 'sqlite_%'")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            let mut out = Vec::new();
            for table in tables {
                let columns: Vec<String> =
                    c.prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
                if table == "account" {
                    out.push((table, "id".into()));
                } else if columns.iter().any(|c| c == "account_id") {
                    out.push((table, "account_id".into()));
                }
            }
            Ok(out)
        })
        .unwrap()
}
