//! Rooms for a meeting (C4 U5). On Microsoft 365 the organization's room
//! lists give every room with its size and building, once a tenant admin
//! consents to `Place.Read.All`. Without that, and on Google, the rooms of
//! the user's own meetings ("recent rooms") and a typed address still
//! work: booking a room is adding its address as an attendee, and
//! `getSchedule` or `freeBusy` say when it is free with the calendar
//! scopes Jolta already has.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use super::sources::graph::{GraphCalendar, directory};
use super::{EventData, Role};
use crate::oauth::{self, Consent, Pending, Provider};
use crate::store;
use crate::{Account, AccountId, AccountKind, Error, Jolta, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomSource {
    /// The organization's room lists.
    Directory,
    /// The user's meetings of the last 180 days.
    Recent,
    /// An address the user typed.
    Typed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Room {
    pub address: String,
    pub name: String,
    pub capacity: Option<u32>,
    pub building: Option<String>,
    /// "2", or the floor's label.
    pub floor: Option<String>,
    /// The room list it is in, usually a building.
    pub list: Option<String>,
    pub accessible: bool,
    pub video: Option<String>,
    pub display: Option<String>,
    pub source: RoomSource,
}

impl Room {
    /// A room known only by its address and the name on a meeting.
    pub fn named(address: &str, name: &str, source: RoomSource) -> Room {
        Room {
            address: address.to_ascii_lowercase(),
            name: if name.trim().is_empty() { address.to_string() } else { name.trim().to_string() },
            capacity: None,
            building: None,
            floor: None,
            list: None,
            accessible: false,
            video: None,
            display: None,
            source,
        }
    }
}

/// What the account answered to the room lists consent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// The provider has no room lists Jolta reads: Google, CalDAV.
    NotOffered,
    /// Microsoft 365, not asked yet: the picker asks when it first opens.
    NotAsked,
    /// Refused, with Microsoft's reason. The picker offers to ask again.
    Refused(String),
    Granted,
}

/// The rooms the picker offers: recent rooms first, with what the room
/// lists say about them, then the listed rooms by list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rooms {
    pub access: Access,
    pub rooms: Vec<Room>,
    /// When the room lists were last read; None when none are stored.
    pub fetched: Option<DateTime<Utc>>,
}

/// The room lists are read again when a day old.
const FRESH: Duration = Duration::days(1);
/// How far back meetings count for recent rooms.
const RECENT_DAYS: i64 = 180;
/// Rooms booked for the coming weeks count too: the user picked them.
const AHEAD_DAYS: i64 = 30;

impl Rooms {
    /// Whether the picker should read the room lists again.
    pub fn stale(&self, now: DateTime<Utc>) -> bool {
        self.access == Access::Granted && self.fetched.is_none_or(|f| now - f >= FRESH)
    }

    /// Whether the room lists are allowed but list no room, as in a tenant
    /// that hasn't set up its rooms in Places.
    pub fn none_listed(&self) -> bool {
        self.access == Access::Granted && self.fetched.is_some() && !self.rooms.iter().any(|r| r.list.is_some() || r.source == RoomSource::Directory)
    }
}

/// The rooms on the user's meetings from `RECENT_DAYS` ago to `AHEAD_DAYS`
/// ahead, most used first, then most recently used. A room is an attendee
/// with the room or resource role, as Microsoft 365, Google and CalDAV
/// mark them; `mine` and the attendee the source marks as the user never
/// count. Each room takes the newest name a meeting gave it.
pub fn recent(events: &[(EventData, Vec<DateTime<Utc>>)], mine: &[String], now: DateTime<Utc>) -> Vec<Room> {
    let (from, to) = (now - Duration::days(RECENT_DAYS), now + Duration::days(AHEAD_DAYS));
    struct Seen {
        uses: usize,
        last: DateTime<Utc>,
        name: String,
        named_at: DateTime<Utc>,
    }
    let mut seen: HashMap<String, Seen> = HashMap::new();
    for (event, starts) in events {
        let starts: Vec<DateTime<Utc>> = starts.iter().copied().filter(|t| *t >= from && *t < to).collect();
        let Some(last) = starts.iter().max().copied() else { continue };
        let rooms = event.attendees.iter().filter(|a| matches!(a.role, Role::Room | Role::Resource) && !a.is_me);
        for a in rooms {
            let address = a.person.address.trim().to_ascii_lowercase();
            if !address.contains('@') || mine.iter().any(|m| m.eq_ignore_ascii_case(&address)) {
                continue;
            }
            let s = seen.entry(address).or_insert(Seen { uses: 0, last, name: String::new(), named_at: last });
            s.uses += starts.len();
            s.last = s.last.max(last);
            if !a.person.name.trim().is_empty() && (s.name.is_empty() || last >= s.named_at) {
                (s.name, s.named_at) = (a.person.name.trim().to_string(), last);
            }
        }
    }
    let mut out: Vec<(String, Seen)> = seen.into_iter().collect();
    out.sort_by(|(a, x), (b, y)| y.uses.cmp(&x.uses).then(y.last.cmp(&x.last)).then(a.cmp(b)));
    out.into_iter().map(|(address, s)| Room::named(&address, &s.name, RoomSource::Recent)).collect()
}

/// The rooms Jolta knows for the account now, without asking anyone.
pub fn known(conn: &rusqlite::Connection, account: AccountId, now: DateTime<Utc>) -> Result<Rooms> {
    let access = store::rooms_access(conn, account)?;
    let mine: Vec<String> = store::accounts(conn)?.into_iter().map(|a| a.address).collect();
    let uses = store::room_uses(conn, account, now - Duration::days(RECENT_DAYS), now + Duration::days(AHEAD_DAYS))?;
    let (listed, fetched) = store::listed_rooms(conn, account)?;
    Ok(Rooms { access, rooms: joined(recent(&uses, &mine, now), listed), fetched })
}

/// Recent rooms first, each with the room list's details when listed, then
/// the listed rooms not used lately.
fn joined(recent: Vec<Room>, mut listed: Vec<Room>) -> Vec<Room> {
    let mut out = Vec::with_capacity(recent.len() + listed.len());
    for r in recent {
        match listed.iter().position(|l| l.address.eq_ignore_ascii_case(&r.address)) {
            Some(i) => out.push(Room { source: RoomSource::Recent, ..listed.remove(i) }),
            None => out.push(r),
        }
    }
    out.extend(listed);
    out
}

/// Reads the room lists again where they are allowed, stores them, and
/// returns the rooms then known. Microsoft refusing the room lists' token
/// or the lists themselves records the refusal. A data folder holding
/// `rooms.json` (the demo and UI tests) answers in place of Microsoft 365,
/// in the shape `directory` reads.
pub async fn refresh(jolta: &Jolta, account: AccountId) -> Result<Rooms> {
    let a = jolta.store.read_async(move |c| store::account(c, account)).await?;
    let access = jolta.store.read_async(move |c| store::rooms_access(c, account)).await?;
    if access == Access::Granted {
        let fetched = Utc::now();
        let listed = match recorded(&jolta.data_dir)? {
            Some(recording) => Ok(directory(&recording)),
            None => GraphCalendar::for_rooms(&a)?.rooms().await,
        };
        match listed {
            Ok(rooms) => jolta.store.write(|tx| store::save_rooms(tx, account, &rooms, fetched))?,
            Err(Error::Auth(reason)) => {
                let access = Access::Refused(oauth::refusal_reason(&reason));
                jolta.store.write(|tx| store::set_rooms_access(tx, account, &access))?;
            }
            Err(e) => return Err(e),
        }
    }
    jolta.store.read_async(move |c| known(c, account, Utc::now())).await
}

fn recorded(dir: &Path) -> Result<Option<Value>> {
    match std::fs::read(dir.join("rooms.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| Error::Other(format!("rooms.json: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Starts the consent to the room lists for a Microsoft 365 account.
/// `port` 0 picks a free port.
pub async fn begin_consent(jolta: &Jolta, account: AccountId, port: u16) -> Result<(Account, Pending)> {
    let a = jolta.store.read_async(move |c| store::account(c, account)).await?;
    if a.kind != AccountKind::Graph {
        return Err(Error::Other(format!("{} has no room lists to ask for", a.address)));
    }
    let client = Provider::Microsoft.client(&jolta.data_dir)?;
    let pending = oauth::begin_rooms(&a, client, port).await?;
    Ok((a, pending))
}

/// Records how the consent ended, then reads the room lists when granted;
/// when that read fails, the picker reads them on its next open. A sign-in
/// that never came back, or came back for another request, is an error of
/// `Pending::finish_consent` and records nothing.
pub async fn finish_consent(jolta: &Jolta, account: &Account, consent: Consent) -> Result<Rooms> {
    let id = account.id;
    match consent {
        Consent::Granted(signed) => {
            oauth::remember_rooms(account, &signed);
            jolta.store.write(|tx| store::set_rooms_access(tx, id, &Access::Granted))?;
            match refresh(jolta, id).await {
                Ok(rooms) => Ok(rooms),
                Err(_) => jolta.store.read_async(move |c| known(c, id, Utc::now())).await,
            }
        }
        Consent::Refused(reason) => {
            let access = Access::Refused(oauth::refusal_reason(&reason));
            jolta.store.write(|tx| store::set_rooms_access(tx, id, &access))?;
            jolta.store.read_async(move |c| known(c, id, Utc::now())).await
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::calendar::{Attendee, Person, Response};

    fn at(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, 13, 0, 0).unwrap()
    }

    fn with(attendees: Vec<Attendee>) -> EventData {
        let plain = serde_json::json!({"id": "A", "iCalUId": "u", "subject": "Plan",
            "start": {"dateTime": "2026-11-05T13:00:00", "timeZone": "UTC"}, "end": {"dateTime": "2026-11-05T14:00:00", "timeZone": "UTC"}});
        EventData { attendees, ..crate::calendar::sources::graph::tests::mapped(plain) }
    }

    fn attendee(address: &str, name: &str, role: Role) -> Attendee {
        Attendee { person: Person { address: address.into(), name: name.into() }, role, response: Response::Accepted, comment: String::new(), is_me: false }
    }

    #[test]
    fn recent_rooms_are_the_rooms_on_the_users_meetings_most_used_first() {
        let now = at(8);
        let events = vec![
            (with(vec![attendee("ann@example.com", "Ann", Role::Required), attendee("Bruegel@Rooms.example", "Bruegel", Role::Resource)]), vec![at(2)]),
            (with(vec![attendee("magritte@rooms.example", "Magritte (6)", Role::Resource)]), vec![at(3), at(4), at(5)]),
            (with(vec![attendee("bruegel@rooms.example", "Bruegel 2.14", Role::Room)]), vec![at(6)]),
            (with(vec![attendee("brel@rooms.example", "Brel", Role::Resource)]), vec![now - Duration::days(200)]),
            (with(vec![attendee("me@example.com", "Me", Role::Resource), Attendee { is_me: true, ..attendee("alias@example.com", "", Role::Room) }]), vec![at(6)]),
            (with(vec![attendee("ensor@rooms.example", "", Role::Resource)]), vec![at(7)]),
        ];
        let rooms = recent(&events, &["ME@example.com".to_string()], now);
        let got: Vec<(&str, &str)> = rooms.iter().map(|r| (r.address.as_str(), r.name.as_str())).collect();
        assert_eq!(
            got,
            [("magritte@rooms.example", "Magritte (6)"), ("bruegel@rooms.example", "Bruegel 2.14"), ("ensor@rooms.example", "ensor@rooms.example")],
            "Magritte three times, then Bruegel twice under its newest name, then Ensor once; Brel was 200 days ago, and the user is never a room"
        );
        assert!(rooms.iter().all(|r| r.source == RoomSource::Recent));
    }

    #[test]
    fn rooms_from_graph_and_google_events_count_as_recent() {
        let graph = crate::calendar::sources::graph::tests::mapped(serde_json::json!({
            "id": "AAMk1", "iCalUId": "g1", "subject": "Plan", "start": {"dateTime": "2026-11-05T13:00:00", "timeZone": "UTC"},
            "end": {"dateTime": "2026-11-05T14:00:00", "timeZone": "UTC"},
            "attendees": [
                {"type": "required", "status": {"response": "accepted"}, "emailAddress": {"name": "Ann", "address": "ann@example.com"}},
                {"type": "resource", "status": {"response": "accepted"}, "emailAddress": {"name": "Bruegel", "address": "bruegel@rooms.example"}}
            ]
        }));
        let google = crate::calendar::sources::google::tests::mapped(serde_json::json!({
            "id": "e1", "iCalUID": "g2", "status": "confirmed", "summary": "Review",
            "start": {"dateTime": "2026-11-06T13:00:00Z"}, "end": {"dateTime": "2026-11-06T14:00:00Z"},
            "attendees": [
                {"email": "me@example.com", "self": true, "responseStatus": "accepted"},
                {"email": "c_188@resource.calendar.google.com", "displayName": "HQ-2-Magritte (6)", "resource": true, "responseStatus": "accepted"}
            ]
        }));
        let rooms = recent(&[(graph, vec![at(5)]), (google, vec![at(6)])], &["me@example.com".into()], at(8));
        let got: Vec<&str> = rooms.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(got, ["HQ-2-Magritte (6)", "Bruegel"], "one use each, the newest first");
    }

    #[test]
    fn recent_rooms_take_the_room_lists_details_and_lead() {
        let listed = |address: &str, list: &str| Room { capacity: Some(8), list: Some(list.into()), source: RoomSource::Directory, ..Room::named(address, address, RoomSource::Directory) };
        let joined = joined(
            vec![Room::named("b@rooms.example", "B", RoomSource::Recent), Room::named("z@rooms.example", "Z", RoomSource::Recent)],
            vec![listed("a@rooms.example", "HQ"), listed("B@rooms.example", "HQ")],
        );
        let got: Vec<(&str, RoomSource, Option<u32>)> = joined.iter().map(|r| (r.address.as_str(), r.source, r.capacity)).collect();
        assert_eq!(
            got,
            [("b@rooms.example", RoomSource::Recent, Some(8)), ("z@rooms.example", RoomSource::Recent, None), ("a@rooms.example", RoomSource::Directory, Some(8))]
        );
    }

    #[test]
    fn the_room_lists_are_read_again_a_day_later_and_only_when_allowed() {
        let rooms = |access, fetched| Rooms { access, rooms: Vec::new(), fetched };
        let now = at(8);
        assert!(rooms(Access::Granted, None).stale(now));
        assert!(!rooms(Access::Granted, Some(now - Duration::hours(23))).stale(now));
        assert!(rooms(Access::Granted, Some(now - Duration::hours(25))).stale(now));
        assert!(!rooms(Access::NotAsked, None).stale(now));
        assert!(!rooms(Access::Refused("No".into()), None).stale(now));
        assert!(rooms(Access::Granted, Some(now)).none_listed(), "allowed, read, and no room in any list");
        assert!(!rooms(Access::Granted, None).none_listed(), "not read yet");
    }

    #[test]
    fn recent_rooms_come_from_the_accounts_stored_meetings() {
        let (j, calendar) = crate::calendar::invitation::tests::jolta("rooms-recent");
        let me = crate::calendar::invitation::tests::ME;
        let horizon = crate::calendar::series::Horizon::around(at(8), chrono_tz::Europe::Brussels);
        for (uid, day, room) in [("a", "20261103", "bruegel"), ("b", "20261104", "bruegel"), ("c", "20261105", "magritte"), ("old", "20260301", "brel")] {
            let ics = format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:{day}T130000Z\r\nDTEND:{day}T140000Z\r\nSUMMARY:{uid}\r\n\
                 ORGANIZER:mailto:{me}\r\nATTENDEE:mailto:{me}\r\nATTENDEE;CUTYPE=ROOM;CN={room}:mailto:{room}@rooms.example\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            );
            let events = crate::calendar::ical::read(&ics, &[me.to_string()]).unwrap();
            j.store.write(|tx| store::apply_item(tx, calendar, uid, None, &events, None, &horizon)).unwrap();
        }
        let rooms = j.store.read(|c| known(c, AccountId(1), at(8))).unwrap();
        let got: Vec<&str> = rooms.rooms.iter().map(|r| r.address.as_str()).collect();
        assert_eq!(got, ["bruegel@rooms.example", "magritte@rooms.example"], "Brel's meeting was in March");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }

    #[test]
    fn the_consent_answer_is_stored_per_account_and_a_refusal_drops_the_listed_rooms() {
        let (j, _) = crate::calendar::invitation::tests::jolta("rooms-access");
        let account = AccountId(1);
        j.store.write(|tx| Ok(tx.execute("UPDATE account SET kind = 'graph' WHERE id = ?1", [account])?)).unwrap();
        let read = || j.store.read(|c| store::rooms_access(c, account)).unwrap();
        assert_eq!(read(), Access::NotAsked);
        j.store.write(|tx| store::set_rooms_access(tx, account, &Access::Granted)).unwrap();
        j.store.write(|tx| store::save_rooms(tx, account, &[], at(8) - Duration::hours(1))).unwrap();
        let empty = j.store.read(|c| known(c, account, at(8))).unwrap();
        assert_eq!(empty.fetched, Some(at(8) - Duration::hours(1)), "a read that listed no room is still a read");
        assert!(empty.none_listed() && !empty.stale(at(8)), "so the picker says so and doesn't ask again for a day");
        let listed = Room { list: Some("HQ".into()), capacity: Some(8), ..Room::named("Bruegel@rooms.example", "Bruegel", RoomSource::Directory) };
        j.store.write(|tx| store::save_rooms(tx, account, &[listed], at(8))).unwrap();
        let now = j.store.read(|c| known(c, account, at(8))).unwrap();
        assert_eq!((now.access, now.rooms.len(), now.fetched), (Access::Granted, 1, Some(at(8))));
        assert_eq!(now.rooms[0].address, "bruegel@rooms.example");
        j.store.write(|tx| store::set_rooms_access(tx, account, &Access::Refused("Need admin approval".into()))).unwrap();
        let later = j.store.read(|c| known(c, account, at(8))).unwrap();
        assert_eq!((later.access, later.rooms.len(), later.fetched), (Access::Refused("Need admin approval".into()), 0, None));
        j.store.write(|tx| Ok(tx.execute("UPDATE account SET kind = 'gmail' WHERE id = ?1", [account])?)).unwrap();
        assert_eq!(read(), Access::NotOffered, "Google has no room lists Jolta reads");
        std::fs::remove_dir_all(&j.data_dir).unwrap();
    }
}
