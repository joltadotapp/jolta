//! The automatic reply state each provider last reported (`calendar::away`).

use chrono::DateTime;
use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::calendar::away::{AutoReply, Away};
use crate::model::AccountId;

/// Stores what the provider said at `fetched` (unix milliseconds), unless
/// a later read is stored already, as when Turn off finished while a
/// refresh was on its way. Returns whether the stored state changed.
pub fn save_away(conn: &Connection, away: &Away, fetched: i64) -> Result<bool> {
    let before = away_of(conn, away.account)?;
    let (state, starts, ends) = match away.reply {
        AutoReply::Off => ("off", None, None),
        AutoReply::On { from, until } => ("on", from.map(|t| t.timestamp()), until.map(|t| t.timestamp())),
    };
    let wrote = conn.execute(
        "INSERT INTO auto_reply (account_id, state, starts, ends, internal, external, audience, editable, fetched)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (account_id) DO UPDATE SET state = excluded.state, starts = excluded.starts, ends = excluded.ends,
             internal = excluded.internal, external = excluded.external, audience = excluded.audience,
             editable = excluded.editable, fetched = excluded.fetched
         WHERE excluded.fetched >= auto_reply.fetched",
        params![away.account, state, starts, ends, away.internal, away.external, away.audience, away.editable, fetched],
    )?;
    Ok(wrote > 0 && before.as_ref() != Some(away))
}

/// Drops the account's state, which Jolta can no longer read. Returns
/// whether there was one.
pub fn forget_away(conn: &Connection, account: AccountId) -> Result<bool> {
    Ok(conn.execute("DELETE FROM auto_reply WHERE account_id = ?1", [account])? > 0)
}

pub fn away_of(conn: &Connection, account: AccountId) -> Result<Option<Away>> {
    Ok(conn.query_row(&format!("{SELECT} WHERE account_id = ?1"), [account], row).optional()?)
}

/// Every account's stored state.
pub fn aways(conn: &Connection) -> Result<Vec<Away>> {
    let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY account_id"))?;
    let rows = stmt.query_map([], row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

const SELECT: &str = "SELECT account_id, state, starts, ends, internal, external, audience, editable FROM auto_reply";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Away> {
    let at = |i: usize| -> rusqlite::Result<_> { Ok(r.get::<_, Option<i64>>(i)?.and_then(|s| DateTime::from_timestamp(s, 0))) };
    let reply = match r.get::<_, String>(1)?.as_str() {
        "on" => AutoReply::On { from: at(2)?, until: at(3)? },
        _ => AutoReply::Off,
    };
    Ok(Away { account: r.get(0)?, reply, internal: r.get(4)?, external: r.get(5)?, audience: r.get(6)?, editable: r.get(7)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::away::Audience;
    use crate::store::Store;

    fn store() -> Store {
        let dir = std::env::temp_dir().join(format!("jolta-away-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        s.write(|tx| {
            tx.execute_batch("INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'graph', 'me@example.com', 'me', 'a1', 0)")?;
            Ok(())
        })
        .unwrap();
        s
    }

    #[test]
    fn a_state_round_trips_and_an_older_read_never_replaces_a_newer() {
        let s = store();
        let on = Away {
            account: AccountId(1),
            reply: AutoReply::On { from: DateTime::from_timestamp(1_800_000_000, 0), until: None },
            internal: "<p>Away</p>".into(),
            external: String::new(),
            audience: Audience::Contacts,
            editable: true,
        };
        let off = Away { reply: AutoReply::Off, ..on.clone() };
        assert!(s.write(|tx| save_away(tx, &on, 1_000)).unwrap(), "a new state");
        assert!(!s.write(|tx| save_away(tx, &on, 2_000)).unwrap(), "the same state again changes nothing");
        assert_eq!(s.read(|c| aways(c)).unwrap(), vec![on.clone()]);
        assert!(s.write(|tx| save_away(tx, &off, 3_000)).unwrap(), "Turn off");
        assert!(!s.write(|tx| save_away(tx, &on, 2_500)).unwrap(), "a refresh read before Turn off finished");
        assert_eq!(s.read(|c| away_of(c, AccountId(1))).unwrap(), Some(off));
        assert!(s.write(|tx| forget_away(tx, AccountId(1))).unwrap());
        assert_eq!(s.read(|c| aways(c)).unwrap(), Vec::new());
    }
}
