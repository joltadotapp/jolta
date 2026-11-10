use rusqlite::{Connection, params};

use crate::Result;
use crate::model::*;

/// Asks to be reminded at `remind_at` if nobody answers the message with
/// this Message-ID, sent at `sent_at`.
pub fn add_reminder(conn: &Connection, account: AccountId, message_id: &str, sent_at: i64, remind_at: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO reminder (account_id, message_id, sent_at, remind_at) VALUES (?1, ?2, ?3, ?4)",
        params![account, message_id.trim_matches(['<', '>']), sent_at, remind_at],
    )?;
    Ok(())
}

/// What a due reminder found: a reply (drop it), no reply yet (fire it), or
/// the sent message not synced yet (try again later).
#[derive(Debug, PartialEq, Eq)]
pub enum ReminderCheck {
    Replied,
    NoReply { thread: ThreadId, subject: String },
    NotYetSynced,
}

/// Settles every reminder of this account that is due: drops answered ones
/// and fires the rest. Returns what fired.
pub fn fire_due_reminders(conn: &Connection, account: AccountId, now: i64) -> Result<Vec<(ThreadId, String)>> {
    let due: Vec<(i64, String, i64)> = conn
        .prepare("SELECT id, message_id, sent_at FROM reminder WHERE account_id = ?1 AND fired = 0 AND remind_at <= ?2")?
        .query_map(params![account, now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut fired = Vec::new();
    for (id, message_id, sent_at) in due {
        match check_reminder(conn, account, &message_id, sent_at)? {
            ReminderCheck::Replied => {
                conn.execute("DELETE FROM reminder WHERE id = ?1", [id])?;
            }
            ReminderCheck::NoReply { thread, subject } => {
                conn.execute("UPDATE reminder SET fired = 1, thread_id = ?2 WHERE id = ?1", params![id, thread])?;
                fired.push((thread, subject));
            }
            ReminderCheck::NotYetSynced => {}
        }
    }
    Ok(fired)
}

fn check_reminder(conn: &Connection, account: AccountId, message_id: &str, sent_at: i64) -> Result<ReminderCheck> {
    let found: Option<(ThreadId, String)> = conn
        .query_row(
            "SELECT thread_id, subject FROM message WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
            params![account, message_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((thread, subject)) = found else { return Ok(ReminderCheck::NotYetSynced) };
    let replied: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM message m WHERE m.thread_id = ?1 AND m.date > ?2
                AND NOT EXISTS (SELECT 1 FROM own_address a WHERE a.address = m.from_addr COLLATE NOCASE))",
        params![thread, sent_at],
        |r| r.get(0),
    )?;
    Ok(if replied { ReminderCheck::Replied } else { ReminderCheck::NoReply { thread, subject } })
}

/// Threads a fired reminder keeps in the inbox.
pub fn reminded_threads(conn: &Connection) -> Result<Vec<ThreadId>> {
    let mut stmt = conn.prepare_cached("SELECT DISTINCT thread_id FROM reminder WHERE fired = 1")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Takes a thread out of the inbox that a fired reminder put it in (it
/// was triaged). Reminders still waiting stay. Returns how many went.
pub fn clear_fired_reminders(conn: &Connection, thread: ThreadId) -> Result<usize> {
    Ok(conn.execute("DELETE FROM reminder WHERE fired = 1 AND thread_id = ?1", [thread])?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search;

    const NOW: i64 = 1_800_000_000;

    fn inbox(s: &crate::store::Store, query: &str) -> Vec<ThreadId> {
        s.read(|c| search::query_order(c, &search::parse(query)?, search::Sort::default()))
            .unwrap()
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    #[test]
    fn unanswered_mail_comes_back_to_the_inbox_until_triaged() {
        let dir = std::env::temp_dir().join(format!("jolta-remind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = crate::store::Store::open(&dir.join("jolta.sqlite")).unwrap();
        s.write(|tx| {
            tx.execute_batch(
                "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'imap', 'me@home.test', 'me', 'a1', 0);
                 INSERT INTO mailbox (id, account_id, remote_id, name, role) VALUES (1, 1, 'INBOX', 'INBOX', 'inbox'), (2, 1, 'Sent', 'Sent', 'sent');",
            )?;
            let mail = |id: i64, thread: i64, from: &str, date: i64, header: &str, mailbox: i64| -> rusqlite::Result<()> {
                tx.execute(
                    "INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, date, message_id, subject)
                     VALUES (?1, 1, ?1, ?2, ?3, ?4, ?5, 'Quote')",
                    params![id, thread, from, date, header],
                )?;
                tx.execute("INSERT INTO message_mailbox (mailbox_id, message_id) VALUES (?1, ?2)", params![mailbox, id])?;
                Ok(())
            };
            mail(10, 1, "ann@x.test", NOW - 900, "a@x", 1)?;
            mail(20, 2, "ME@home.test", NOW - 600, "quote@home.test", 2)?;
            mail(30, 3, "me@home.test", NOW - 600, "answered@home.test", 2)?;
            mail(31, 3, "bob@x.test", NOW - 300, "re@x", 1)?;
            tx.execute("UPDATE message SET flags = 1 WHERE id = 10", [])?;
            add_reminder(tx, AccountId(1), "<quote@home.test>", NOW - 600, NOW - 60)?;
            add_reminder(tx, AccountId(1), "answered@home.test", NOW - 600, NOW - 60)?;
            add_reminder(tx, AccountId(1), "later@home.test", NOW, NOW + 3600)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(inbox(&s, "in:inbox"), [ThreadId(3), ThreadId(1)]);

        let fired = s.write(|tx| fire_due_reminders(tx, AccountId(1), NOW)).unwrap();
        assert_eq!(fired, [(ThreadId(2), "Quote".to_string())], "the answered thread's reminder is dropped");
        assert_eq!(inbox(&s, "in:inbox"), [ThreadId(3), ThreadId(2), ThreadId(1)], "newest first, the reminded thread included");
        assert_eq!(inbox(&s, "mailbox:1"), [ThreadId(3), ThreadId(2), ThreadId(1)], "the account's own inbox too");
        assert_eq!(inbox(&s, "in:inbox is:read"), [ThreadId(1)], "the list's other terms still apply");
        assert_eq!(inbox(&s, "in:sent"), [ThreadId(3), ThreadId(2)], "other lists are unchanged");

        let left: i64 = s.read(|c| Ok(c.query_row("SELECT count(*) FROM reminder", [], |r| r.get(0))?)).unwrap();
        assert_eq!(left, 2, "the fired one and the one still waiting");
        s.write(|tx| clear_fired_reminders(tx, ThreadId(2))).unwrap();
        assert_eq!(inbox(&s, "in:inbox"), [ThreadId(3), ThreadId(1)]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
