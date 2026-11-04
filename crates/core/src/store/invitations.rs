//! The events that calendar parts in mail name, and what the store knows
//! about them: the copy in the user's calendars and whether the user ever
//! wrote to the sender.

use chrono_tz::Tz;
use rusqlite::{Connection, params};

use super::calendar::moment_text;
use crate::calendar::invitation::Invitation;
use crate::calendar::series::copy_rank;
use crate::calendar::{CalendarId, CalendarSource, EventData, EventId, EventStatus, Moment, Response};
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
    pub my_response: Option<Response>,
    pub sequence: i64,
    pub status: EventStatus,
}

/// The event with this UID and recurrence id in a shown calendar. A
/// recurrence id matches the same moment written in another zone, as
/// `Moment::names` decides. A meeting in several calendars gives the copy
/// the views show.
pub fn stored_event(conn: &Connection, uid: &str, recurrence_id: Option<&Moment>, here: Tz) -> Result<Option<StoredEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.id, e.calendar_id, e.my_response, e.sequence, e.status, c.source, c.is_default, e.data
         FROM event e JOIN calendar c ON c.id = e.calendar_id
         WHERE e.uid = ?1 AND (e.recurrence_id = '') = ?2 AND c.shown",
    )?;
    let rows = stmt.query_map(params![uid, recurrence_id.is_none()], |r| {
        let found = StoredEvent {
            event_id: r.get(0)?,
            calendar_id: r.get(1)?,
            my_response: r.get(2)?,
            sequence: r.get(3)?,
            status: r.get(4)?,
        };
        let (source, is_default): (CalendarSource, bool) = (r.get(5)?, r.get(6)?);
        Ok((copy_rank(uid, source, is_default, found.calendar_id), found, r.get::<_, String>(7)?))
    })?;
    let mut best: Option<((bool, bool, i64), StoredEvent)> = None;
    for row in rows {
        let (rank, found, data) = row?;
        if let Some(rid) = recurrence_id {
            let event: EventData = serde_json::from_str(&data).map_err(|e| Error::Other(format!("stored event {}: {e}", found.event_id)))?;
            if !event.recurrence_id.is_some_and(|stored| rid.names(&stored, here)) {
                continue;
            }
        }
        if best.as_ref().is_none_or(|(b, _)| rank < *b) {
            best = Some((rank, found));
        }
    }
    Ok(best.map(|(_, e)| e))
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
        let stored = jolta.store.read(|c| stored_event(c, &event.uid, None, chrono_tz::Europe::Brussels)).unwrap().unwrap();
        assert_eq!((stored.calendar_id, stored.my_response, stored.sequence), (calendar, Some(Response::Accepted), 2));
        assert_eq!(jolta.store.read(|c| stored_event(c, "someone-else", None, chrono_tz::Europe::Brussels)).unwrap(), None);
        jolta.store.write(|tx| crate::store::set_calendar_shown(tx, calendar, false)).unwrap();
        assert_eq!(jolta.store.read(|c| stored_event(c, &event.uid, None, chrono_tz::Europe::Brussels)).unwrap(), None, "hidden calendars don't count");
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
        let found = |rid: &Moment| jolta.store.read(|c| stored_event(c, &moved.uid, Some(rid), chrono_tz::Europe::Brussels)).unwrap();
        assert_eq!(found(&brussels).map(|s| s.sequence), Some(3));
        assert_eq!(found(&utc).map(|s| s.sequence), Some(3), "09:30 Brussels is 08:30 UTC in November");
        let later = Moment::Time(crate::calendar::Local { wall: local.wall + chrono::Duration::hours(1), ..local });
        assert_eq!(found(&later), None);
        assert_eq!(jolta.store.read(|c| stored_event(c, &moved.uid, None, chrono_tz::Europe::Brussels)).unwrap(), None, "no master stored");
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
