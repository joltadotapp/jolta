//! The events that calendar parts in mail name, and what the store knows
//! about them: the copy in the user's calendars and whether the user ever
//! wrote to the sender.

use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params};

use super::calendar::moment_text;
use crate::calendar::invitation::{Invitation, Method};
use crate::calendar::series::copy_rank;
use crate::calendar::{CalendarId, CalendarSource, EventData, EventId, Moment, Response};
use crate::model::*;
use crate::{Error, Result, compose};

/// Replaces the invitation rows of a message with the events of its body.
pub fn set_invitations(conn: &Connection, id: MessageId, invitations: &[Invitation]) -> Result<()> {
    conn.prepare_cached("DELETE FROM invitation WHERE message_id = ?1")?.execute([id])?;
    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO invitation (message_id, uid, recurrence_id, sequence, method, organizer)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for i in invitations {
        let e = &i.event;
        insert.execute(params![
            id,
            e.uid,
            e.recurrence_id.as_ref().map(moment_text).unwrap_or_default(),
            e.sequence,
            i.method,
            e.organizer.as_ref().map(|o| o.address.as_str()).unwrap_or_default(),
        ])?;
    }
    Ok(())
}

pub fn invitation_scan_batch(conn: &Connection, limit: usize) -> Result<Vec<(MessageId, String)>> {
    let mut stmt = conn.prepare(
        "SELECT s.message_id, coalesce(m.blob, '') FROM invitation_scan s LEFT JOIN message m ON m.id = s.message_id LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records what the scan found in a message and takes it off the list. A
/// message deleted before its turn is only taken off.
pub fn finish_invitation_scan(conn: &Connection, id: MessageId, invitations: &[Invitation]) -> Result<()> {
    let exists: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM message WHERE id = ?1)", [id], |r| r.get(0))?;
    if exists {
        set_invitations(conn, id, invitations)?;
    }
    conn.execute("DELETE FROM invitation_scan WHERE message_id = ?1", [id])?;
    Ok(())
}

/// The copy of an invitation's event in the user's shown calendars.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub event_id: EventId,
    pub calendar_id: CalendarId,
    pub event: EventData,
}

/// The event with this UID and recurrence id in a shown calendar, of
/// `account` only when one is given. A recurrence id matches the same
/// moment written in another zone, as `Moment::names` decides. A meeting in
/// several calendars gives the copy the views show.
pub fn stored_event(conn: &Connection, uid: &str, recurrence_id: Option<&Moment>, account: Option<AccountId>, here: Tz) -> Result<Option<StoredEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.calendar_id, c.source, c.is_default, e.data
         FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.uid = ?1 AND (e.recurrence_id = '') = ?2 AND c.shown AND (?3 IS NULL OR c.account_id = ?3)",
    )?;
    let rows = stmt.query_map(params![uid, recurrence_id.is_none(), account], |r| {
        let (event_id, calendar_id, source, is_default): (EventId, CalendarId, CalendarSource, bool) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
        Ok((copy_rank(uid, source, is_default, calendar_id), event_id, calendar_id, r.get::<_, String>(4)?))
    })?;
    let mut best: Option<((bool, bool, i64), StoredEvent)> = None;
    for row in rows {
        let (rank, event_id, calendar_id, data) = row?;
        if best.as_ref().is_some_and(|(b, _)| *b <= rank) {
            continue;
        }
        let event: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {event_id}: {e}")))?;
        if recurrence_id.is_some_and(|rid| !event.recurrence_id.is_some_and(|stored| rid.names(&stored, here))) {
            continue;
        }
        best = Some((rank, StoredEvent { event_id, calendar_id, event }));
    }
    Ok(best.map(|(_, e)| e))
}

/// Every stored event of the series with this UID, the master and its
/// overrides, from the one calendar the views show it from, of `account`
/// only when one is given.
pub fn stored_series(conn: &Connection, uid: &str, account: Option<AccountId>) -> Result<Vec<EventData>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.calendar_id, c.source, c.is_default, e.data
         FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.uid = ?1 AND c.shown AND (?2 IS NULL OR c.account_id = ?2)",
    )?;
    let rows = stmt
        .query_map(params![uid, account], |r| {
            let (event_id, calendar_id, source, is_default): (EventId, CalendarId, CalendarSource, bool) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            Ok((copy_rank(uid, source, is_default, calendar_id), event_id, calendar_id, r.get::<_, String>(4)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some(best) = rows.iter().map(|(rank, _, calendar, _)| (*rank, *calendar)).min() else { return Ok(Vec::new()) };
    rows.into_iter()
        .filter(|(_, _, calendar, _)| *calendar == best.1)
        .map(|(_, id, _, data)| serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {id}: {e}"))))
        .collect()
}

/// An answer as the user gave it from Jolta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GivenAnswer {
    pub answer: Response,
    /// Empty without one.
    pub note: String,
    /// False for "Don't send a reply".
    pub notify: bool,
}

/// The reply mail an answer queued, while the outbox still means to send
/// it or has sent it. One that failed for good or was taken back is none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyMail {
    Waiting(i64),
    Sent(i64),
}

/// Keeps the answer the user gave an event's invitation on an account:
/// the whole event, or with `recurrence_id` one occurrence, with the outbox
/// row of its reply mail.
pub fn remember_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>, given: &GivenAnswer, reply: Option<i64>) -> Result<()> {
    conn.execute(
        "INSERT INTO invitation_answer (account_id, uid, recurrence_id, answer, note, notify, reply) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (account_id, uid, recurrence_id) DO UPDATE SET answer = excluded.answer, note = excluded.note,
           notify = excluded.notify, reply = excluded.reply",
        params![account, uid, recurrence_id.map(moment_text).unwrap_or_default(), given.answer, given.note, given.notify, reply],
    )?;
    // A whole series answered again replaces what was said for single
    // occurrences, and takes back their replies still waiting.
    if recurrence_id.is_none() {
        let mut stmt = conn.prepare(
            "SELECT o.id FROM invitation_answer a JOIN outbox o ON o.id = a.reply
             WHERE a.account_id = ?1 AND a.uid = ?2 AND a.recurrence_id != '' AND o.state = 'queued'",
        )?;
        let waiting = stmt.query_map(params![account, uid], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for id in waiting {
            super::cancel_send(conn, id)?;
        }
        conn.execute("DELETE FROM invitation_answer WHERE account_id = ?1 AND uid = ?2 AND recurrence_id != ''", params![account, uid])?;
    }
    Ok(())
}

/// The answer kept for exactly this event or occurrence, and its reply mail.
pub fn given_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<(GivenAnswer, Option<ReplyMail>)>> {
    Ok(conn
        .query_row(
            "SELECT a.answer, a.note, a.notify, o.id, o.state FROM invitation_answer a LEFT JOIN outbox o ON o.id = a.reply
             WHERE a.account_id = ?1 AND a.uid = ?2 AND a.recurrence_id = ?3",
            params![account, uid, recurrence_id.map(moment_text).unwrap_or_default()],
            |r| {
                let reply = match (r.get::<_, Option<i64>>(3)?, r.get::<_, Option<String>>(4)?.as_deref()) {
                    (Some(id), Some("queued")) => Some(ReplyMail::Waiting(id)),
                    (Some(id), Some("sent")) => Some(ReplyMail::Sent(id)),
                    _ => None,
                };
                Ok((GivenAnswer { answer: r.get(0)?, note: r.get(1)?, notify: r.get(2)? }, reply))
            },
        )
        .optional()?)
}

/// The answer kept for an occurrence, else for its whole event.
pub fn remembered_answer(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<Response>> {
    let rid = recurrence_id.map(moment_text).unwrap_or_default();
    Ok(conn
        .query_row(
            "SELECT answer FROM invitation_answer WHERE account_id = ?1 AND uid = ?2 AND recurrence_id IN (?3, '')
             ORDER BY recurrence_id = '' LIMIT 1",
            params![account, uid, rid],
            |r| r.get(0),
        )
        .optional()?)
}

/// The reply mail Jolta sent with the answer `remembered_answer` finds:
/// its blob, which names the version answered. None for an answer given
/// without a reply, or a reply that failed for good.
pub fn remembered_reply(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>) -> Result<Option<String>> {
    let rid = recurrence_id.map(moment_text).unwrap_or_default();
    Ok(conn
        .query_row(
            "SELECT o.blob FROM invitation_answer a JOIN outbox o ON o.id = a.reply AND o.state IN ('queued', 'sent')
             WHERE a.account_id = ?1 AND a.uid = ?2 AND a.recurrence_id IN (?3, '')
             ORDER BY a.recurrence_id = '' LIMIT 1",
            params![account, uid, rid],
            |r| r.get(0),
        )
        .optional()?)
}

/// One message on an account that carries a version of an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRow {
    pub message: MessageId,
    pub method: Method,
    pub sequence: i64,
    /// The stored body.
    pub blob: Option<String>,
}

/// The requests, additions and cancellations for this UID and recurrence
/// id in the account's messages other than `skip`, highest SEQUENCE first.
pub fn invitation_versions(conn: &Connection, account: AccountId, uid: &str, recurrence_id: Option<&Moment>, skip: MessageId) -> Result<Vec<VersionRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT i.message_id, i.method, i.sequence, m.blob FROM invitation i JOIN message m ON m.id = i.message_id
         WHERE i.uid = ?1 AND i.recurrence_id = ?2 AND m.account_id = ?3 AND i.message_id != ?4
           AND i.method IN ('REQUEST', 'ADD', 'CANCEL') AND (m.blob IS NULL OR m.blob NOT LIKE 'synth:%')
         ORDER BY i.sequence DESC, i.message_id DESC LIMIT 20",
    )?;
    let rows = stmt.query_map(params![uid, recurrence_id.map(moment_text).unwrap_or_default(), account, skip], |r| {
        Ok(VersionRow { message: r.get(0)?, method: r.get(1)?, sequence: r.get(2)?, blob: r.get(3)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Mail from the address that the user answered, through the partial
/// index `message_answered`, which needs this exact `flags & 4` term.
const REPLIED: &str = "SELECT EXISTS (SELECT 1 FROM message WHERE from_addr = ?1 COLLATE NOCASE AND (flags & 4) != 0)";
const _: () = assert!(Flags::ANSWERED == 4);
/// The recipients of the user's own mail that the search index finds the
/// address in, newest first: a contact the user writes to is found in the
/// first rows, and only a stranger's own messages are read to the end.
const SENT: &str = "SELECT m.to_addrs || ', ' || m.cc_addrs FROM message_fts f
     JOIN message m ON m.id = f.rowid JOIN account a ON a.id = m.account_id
     WHERE message_fts MATCH ?1 AND m.from_addr = a.address COLLATE NOCASE ORDER BY f.rowid DESC";

/// Whether the user ever sent mail to `address` from one of their
/// accounts, or replied to mail from it. A search phrase also matches
/// longer addresses ("jan@x.com" in "jan@x.com.au"), so each hit is
/// checked against the parsed recipients.
pub fn has_written_to(conn: &Connection, address: &str) -> Result<bool> {
    let address = address.trim().to_lowercase();
    if !address.contains(char::is_alphanumeric) {
        return Ok(false);
    }
    let replied: bool = conn.query_row(REPLIED, [&address], |r| r.get(0))?;
    if replied {
        return Ok(true);
    }
    let mut stmt = conn.prepare_cached(SENT)?;
    let mut rows = stmt.query([format!("recipients : \"{}\"", address.replace('"', "\"\""))])?;
    while let Some(r) = rows.next()? {
        let list: String = r.get(0)?;
        if compose::parse_addresses(&list).iter().any(|(_, a)| a.trim().eq_ignore_ascii_case(&address)) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::calendar::Response;
    use crate::calendar::invitation::tests::{import, jolta, mail};
    use crate::calendar::series::Horizon;

    fn rows(jolta: &crate::Jolta) -> Vec<(String, String, i64, String, String)> {
        jolta
            .store
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT uid, recurrence_id, sequence, method, organizer FROM invitation ORDER BY uid, recurrence_id",
                )?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .unwrap()
    }

    #[test]
    fn a_stored_body_records_its_invitations() {
        let (jolta, _) = jolta("stored");
        import(&jolta, mail!("cancel.eml"));
        import(&jolta, mail!("series.eml"));
        import(&jolta, b"Subject: plain\r\n\r\nNo calendar.\r\n");
        let uid = "7kq2m4p9r1s3t5v7x9z0b2d4f6@google.com";
        assert_eq!(
            rows(&jolta),
            [
                (uid.into(), "".into(), 1, "CANCEL".into(), "ann@example.com".into()),
                ("standup-series@example.com".into(), "".into(), 0, "REQUEST".into(), "ann@example.com".into()),
                ("standup-series@example.com".into(), "20261105T093000 Europe/Brussels".into(), 0, "REQUEST".into(), "ann@example.com".into()),
            ]
        );
        jolta.store.write(|tx| Ok(tx.execute("DELETE FROM message", [])?)).unwrap();
        assert!(rows(&jolta).is_empty(), "rows go with their message");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn the_scan_fills_in_bodies_stored_before_and_runs_once() {
        let (jolta, _) = jolta("scan");
        let request = import(&jolta, mail!("google-request.eml"));
        let reply = import(&jolta, mail!("reply.eml"));
        let gone = import(&jolta, mail!("outlook-request.eml"));
        jolta
            .store
            .write(|tx| {
                tx.execute("DELETE FROM invitation", [])?;
                tx.execute("INSERT INTO invitation_scan SELECT id FROM message", [])?;
                tx.execute("DELETE FROM message WHERE id = ?1", [gone])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(crate::engine::scan_invitations(&jolta).unwrap(), 2);
        let found: Vec<(String, String)> = rows(&jolta).into_iter().map(|r| (r.0, r.3)).collect();
        assert_eq!(
            found,
            [("7kq2m4p9r1s3t5v7x9z0b2d4f6@google.com".into(), "REQUEST".into()), ("design-crit-1113@example.com".into(), "REPLY".into())]
        );
        assert_eq!(crate::engine::scan_invitations(&jolta).unwrap(), 0, "the queue is empty");
        assert_eq!(rows(&jolta).len(), 2);
        let _ = (request, reply);
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn an_invitation_finds_its_event_in_the_calendars() {
        let (jolta, calendar) = jolta("match");
        let found = crate::calendar::invitation::find(mail!("outlook-request.eml"), &["me@example.com".into()]);
        let mut event: EventData = found[0].event.clone();
        event.attendees[0].response = Response::Accepted;
        event.sequence = 2;
        let horizon = Horizon::around(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(), chrono_tz::Europe::Brussels);
        jolta
            .store
            .write(|tx| crate::store::apply_item(tx, calendar, "remote-1", None, &[event.clone()], None, &horizon))
            .unwrap();
        let stored = jolta.store.read(|c| stored_event(c, &event.uid, None, None, chrono_tz::Europe::Brussels)).unwrap().unwrap();
        assert_eq!((stored.calendar_id, stored.event.my_response(), stored.event.sequence), (calendar, Some(Response::Accepted), 2));
        assert_eq!(jolta.store.read(|c| stored_event(c, "someone-else", None, None, chrono_tz::Europe::Brussels)).unwrap(), None);
        jolta.store.write(|tx| crate::store::set_calendar_shown(tx, calendar, false)).unwrap();
        assert_eq!(jolta.store.read(|c| stored_event(c, &event.uid, None, None, chrono_tz::Europe::Brussels)).unwrap(), None, "hidden calendars don't count");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn an_occurrence_matches_its_moment_in_any_zone() {
        let (jolta, calendar) = jolta("match-moment");
        let mut series = crate::calendar::invitation::find(mail!("series.eml"), &["me@example.com".into()]);
        let mut moved = series.remove(1).event;
        moved.sequence = 3;
        let horizon = Horizon::around(Utc.with_ymd_and_hms(2026, 11, 4, 0, 0, 0).unwrap(), chrono_tz::Europe::Brussels);
        jolta.store.write(|tx| crate::store::apply_item(tx, calendar, "remote-1", None, &[moved.clone()], None, &horizon)).unwrap();
        let brussels = moved.recurrence_id.unwrap();
        let Moment::Time(local) = brussels else { panic!("a time") };
        let utc = Moment::Time(crate::calendar::Local {
            wall: local.instant(chrono_tz::UTC).naive_utc(),
            zone: crate::calendar::Zone::Tz(chrono_tz::UTC),
        });
        let found = |rid: &Moment| jolta.store.read(|c| stored_event(c, &moved.uid, Some(rid), None, chrono_tz::Europe::Brussels)).unwrap();
        assert_eq!(found(&brussels).map(|s| s.event.sequence), Some(3));
        assert_eq!(found(&utc).map(|s| s.event.sequence), Some(3), "09:30 Brussels is 08:30 UTC in November");
        let later = Moment::Time(crate::calendar::Local { wall: local.wall + chrono::Duration::hours(1), ..local });
        assert_eq!(found(&later), None);
        assert_eq!(jolta.store.read(|c| stored_event(c, &moved.uid, None, None, chrono_tz::Europe::Brussels)).unwrap(), None, "no master stored");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn written_to_reads_no_message_it_doesnt_need() {
        let (jolta, _) = jolta("written-plan");
        let plans: Vec<String> = jolta
            .store
            .read(|c| {
                let mut details = Vec::new();
                for (sql, n) in [(REPLIED, 1), (SENT, 1)] {
                    let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let args = vec![rusqlite::types::Value::Text("x".into()); n];
                    let mut rows = stmt.query(rusqlite::params_from_iter(args))?;
                    while let Some(r) = rows.next()? {
                        details.push(r.get::<_, String>(3)?);
                    }
                }
                Ok(details)
            })
            .unwrap();
        assert!(!plans.iter().any(|d| d == "SCAN m" || d == "SCAN message" || d.starts_with("SCAN m ") || d.starts_with("SCAN message ")), "{plans:#?}");
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }

    #[test]
    fn written_to_means_sent_to_or_replied_to() {
        let (jolta, _) = jolta("written");
        jolta
            .store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO message (id, account_id, remote_id, thread_id, from_addr, to_addrs, cc_addrs, date, flags)
                     VALUES (1, 1, 'a', 1, 'ME@example.com', 'Jan Peeters <jan@example.com>', 'Cleo <cleo@example.com>', 0, 0),
                            (2, 1, 'b', 2, 'bart@example.com', 'me@example.com', '', 0, 4),
                            (3, 1, 'c', 3, 'news@example.com', 'me@example.com', '', 0, 0);
                     INSERT INTO message_fts (rowid, subject, sender, recipients, body)
                     VALUES (1, '', ' ME@example.com', 'Jan Peeters <jan@example.com> Cleo <cleo@example.com>', ''),
                            (2, '', ' bart@example.com', 'me@example.com ', ''),
                            (3, '', ' news@example.com', 'me@example.com ', '');",
                )?;
                Ok(())
            })
            .unwrap();
        let written = |a: &str| jolta.store.read(|c| has_written_to(c, a)).unwrap();
        assert!(written("jan@example.com") && written("Cleo@Example.com"), "sent to, in To or Cc");
        assert!(written("bart@example.com"), "replied to");
        assert!(!written("an@example.com") && !written("example.com"), "only whole addresses");
        assert!(!written("news@example.com") && !written(""));
        std::fs::remove_dir_all(&jolta.data_dir).unwrap();
    }
}
