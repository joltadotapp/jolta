//! New-mail notifications and the Dock badge. The core decides which mail
//! is worth a notification and words it; each shell only delivers.

use rusqlite::{Connection, params};

use crate::search::NOTIFICATION;
use crate::{Result, compose};

/// SQL: the message `alias` is from one of the user's addresses.
fn from_me(alias: &str) -> String {
    format!("EXISTS (SELECT 1 FROM own_address a WHERE a.address = {alias}.from_addr COLLATE NOCASE)")
}

/// SQL: one of the user's addresses is a whole address in the list
/// `column` holds ("Ann <ann@x>, bob@y"). Brackets, quotes and spaces
/// become commas, so `ann@x` doesn't match `joann@x`.
fn mine_in(column: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM own_address a WHERE instr(
             ',' || lower(replace(replace(replace(replace({column}, '<', ','), '>', ','), ' ', ','), '\"', ',')) || ',',
             ',' || lower(a.address) || ',') > 0)"
    )
}

/// SQL over message `m`: the mail is important. A VIP wrote it; it is a
/// reply in a conversation the user wrote in, not from an automated sender;
/// someone the user has written to sent it To the user, and it is neither
/// a newsletter nor automated; it carries an invitation, change or
/// cancellation; or, with AI sorting on, the model says it needs a reply
/// and isn't a newsletter or a notification. The user's own mail never is.
/// The banners, the badge and `is:important` all use this.
pub fn important(conn: &Connection) -> Result<String> {
    let sorting = crate::ai::settings(conn)?.sorts_new_mail();
    let mut rules = vec![
        "EXISTS (SELECT 1 FROM vip v WHERE v.address = m.from_addr)".to_string(),
        format!(
            "(NOT {NOTIFICATION} AND EXISTS (SELECT 1 FROM message o WHERE o.thread_id = m.thread_id AND o.id != m.id AND {}))",
            from_me("o")
        ),
        format!(
            "(EXISTS (SELECT 1 FROM correspondent c WHERE c.address = m.from_addr) AND {}
              AND m.list_id IS NULL AND m.list_unsubscribe IS NULL AND NOT {NOTIFICATION})",
            mine_in("m.to_addrs")
        ),
        "EXISTS (SELECT 1 FROM invitation i WHERE i.message_id = m.id AND i.method IN ('REQUEST', 'CANCEL'))".to_string(),
    ];
    if sorting {
        rules.push(
            "EXISTS (SELECT 1 FROM ai_triage t WHERE t.message_id = m.id AND t.needs_reply = 1
                     AND t.category NOT IN ('newsletter', 'notification'))"
                .into(),
        );
    }
    Ok(format!("(NOT {} AND ({}))", from_me("m"), rules.join(" OR ")))
}

/// Records the recipients of a message from one of the user's addresses
/// as people the user has written to.
pub(crate) fn note_correspondents(conn: &Connection, from: &str, to: &str, cc: &str) -> Result<()> {
    let mine: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM own_address WHERE address = ?1 COLLATE NOCASE)", [from], |r| r.get(0))?;
    if !mine {
        return Ok(());
    }
    let mut insert = conn.prepare_cached("INSERT OR IGNORE INTO correspondent (address) VALUES (?1)")?;
    for (_, address) in compose::parse_addresses(&format!("{to}, {cc}")) {
        let address = address.trim();
        if address.contains('@') {
            insert.execute(params![address])?;
        }
    }
    Ok(())
}

/// Records everyone the stored mail from the user's addresses went to, for
/// mail stored before the table existed or before an alias was known.
pub fn fill_correspondents(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(&format!("SELECT m.from_addr, m.to_addrs, m.cc_addrs FROM message m WHERE {}", from_me("m")))?;
    let sent: Vec<(String, String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    for (from, to, cc) in sent {
        note_correspondents(conn, &from, &to, &cc)?;
    }
    Ok(())
}
