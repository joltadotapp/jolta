//! The room picker for the app: the rooms an account offers, the room
//! lists consent on Microsoft 365, and when rooms are free. Times cross as
//! unix seconds.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use jolta_core::AccountId;
use jolta_core::calendar::availability::Availability;
use jolta_core::calendar::freebusy;
use jolta_core::calendar::rooms::{self, Access, Room, RoomSource, Rooms};

use crate::find_time::{PersonRow, blocks};
use crate::{Core, CoreError, Result, SignIn, SignInFor};

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum RoomAccess {
    /// The provider has no room lists Jolta reads: Google, CalDAV.
    NotOffered,
    /// Microsoft 365, not asked yet: the picker asks when it first opens.
    NotAsked,
    /// Refused, with Microsoft's reason.
    Refused { reason: String },
    Granted,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoomOrigin {
    Directory,
    Recent,
    Typed,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct RoomInfo {
    pub address: String,
    pub name: String,
    pub capacity: Option<u32>,
    /// The room list it is in, which the picker groups by.
    pub list: Option<String>,
    /// "Brussels HQ · Floor 2", when the room lists say.
    pub place: Option<String>,
    /// "Display, Teams Room", when the room lists say.
    pub equipment: Option<String>,
    pub accessible: bool,
    pub origin: RoomOrigin,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct RoomsOutcome {
    pub account_id: i64,
    pub access: RoomAccess,
    /// Recent rooms first, then the listed rooms by list.
    pub rooms: Vec<RoomInfo>,
    /// The room lists are a day old or were never read: refresh them.
    pub stale: bool,
    /// Allowed and read, but the organization lists no room.
    pub none_listed: bool,
}

fn place(r: &Room) -> Option<String> {
    let floor = r.floor.as_ref().map(|f| if f.chars().all(|c| c.is_ascii_digit()) { format!("Floor {f}") } else { f.clone() });
    let parts: Vec<String> = r.building.iter().cloned().chain(floor).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn equipment(r: &Room) -> Option<String> {
    let parts: Vec<String> = r.display.iter().chain(r.video.iter()).cloned().chain(r.accessible.then(|| "Step-free".to_string())).collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

impl From<&Room> for RoomInfo {
    fn from(r: &Room) -> Self {
        RoomInfo {
            address: r.address.clone(),
            name: r.name.clone(),
            capacity: r.capacity,
            list: r.list.clone(),
            place: place(r),
            equipment: equipment(r),
            accessible: r.accessible,
            origin: match r.source {
                RoomSource::Directory => RoomOrigin::Directory,
                RoomSource::Recent => RoomOrigin::Recent,
                RoomSource::Typed => RoomOrigin::Typed,
            },
        }
    }
}

fn outcome(account: AccountId, rooms: &Rooms) -> RoomsOutcome {
    RoomsOutcome {
        account_id: account.0,
        access: match &rooms.access {
            Access::NotOffered => RoomAccess::NotOffered,
            Access::NotAsked => RoomAccess::NotAsked,
            Access::Refused(reason) => RoomAccess::Refused { reason: reason.clone() },
            Access::Granted => RoomAccess::Granted,
        },
        rooms: rooms.rooms.iter().map(RoomInfo::from).collect(),
        stale: rooms.stale(Utc::now()),
        none_listed: rooms.none_listed(),
    }
}

/// "Room · 8 people", or "Room" when its size isn't known.
fn room_note(capacity: Option<u32>) -> String {
    match capacity {
        Some(1) => "Room · 1 person".into(),
        Some(n) => format!("Room · {n} people"),
        None => "Room".into(),
    }
}

fn at(t: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(t, 0).unwrap_or_default()
}

#[uniffi::export]
impl Core {
    /// The rooms the account offers now, from the store. When `stale`,
    /// call `refresh_rooms` and show its answer when it comes.
    pub fn rooms(&self, account_id: i64) -> Result<RoomsOutcome> {
        let account = AccountId(account_id);
        let rooms = self.jolta.store.read(|c| rooms::known(c, account, Utc::now()))?;
        Ok(outcome(account, &rooms))
    }

    /// Reads the room lists again where they are allowed.
    pub async fn refresh_rooms(&self, account_id: i64) -> Result<RoomsOutcome> {
        let (jolta, account) = (self.jolta.clone(), AccountId(account_id));
        let rooms = self
            .runtime
            .spawn(async move { rooms::refresh(&jolta, account).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(outcome(account, &rooms))
    }

    /// Starts the browser consent to a Microsoft 365 account's room lists.
    /// `finish()` records the answer; a refusal is its error.
    pub async fn begin_rooms_consent(&self, account_id: i64) -> Result<Arc<SignIn>> {
        let jolta = self.jolta.clone();
        let (account, pending) = self
            .runtime
            .spawn(async move { rooms::begin_consent(&jolta, AccountId(account_id), 0).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(Arc::new(SignIn {
            url: pending.url.clone(),
            provider: jolta_core::oauth::Provider::Microsoft,
            purpose: SignInFor::Rooms(account),
            pending: Mutex::new(Some(pending)),
            jolta: self.jolta.clone(),
            runtime: self.runtime.handle().clone(),
            engine: self.engine.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        }))
    }

    /// When each room in `addresses` is busy from `from` to `to`, as rows
    /// for Find a time and the picker's bars, in the order asked. A room
    /// the account knows takes its name and size from there.
    pub async fn room_availability(&self, account_id: i64, addresses: Vec<String>, from: i64, to: i64) -> Result<Vec<PersonRow>> {
        let (jolta, account) = (self.jolta.clone(), AccountId(account_id));
        let known = self.jolta.store.read(|c| rooms::known(c, account, Utc::now()))?.rooms;
        let asked = addresses.clone();
        let schedules = self
            .runtime
            .spawn(async move { freebusy::room_schedules(&jolta, account, &asked, at(from), at(to)).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(schedules
            .into_iter()
            .map(|s| {
                let room = known.iter().find(|r| r.address.eq_ignore_ascii_case(&s.person));
                let (busy, unknown) = match &s.availability {
                    Availability::Known(b) => (blocks(b), None),
                    Availability::Unknown(why) => (Vec::new(), Some(why.reason())),
                };
                PersonRow {
                    name: room.map_or_else(|| s.person.clone(), |r| r.name.clone()),
                    address: s.person,
                    is_me: false,
                    is_room: true,
                    busy,
                    unknown,
                    hours: None,
                    outside_hours: Vec::new(),
                    note: Some(room_note(room.and_then(|r| r.capacity))),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_reads_as_its_place_and_equipment() {
        let r = Room {
            capacity: Some(8),
            building: Some("Brussels HQ".into()),
            floor: Some("2".into()),
            video: Some("Teams Room".into()),
            display: Some("Display".into()),
            ..Room::named("bruegel@rooms.example", "Bruegel", RoomSource::Directory)
        };
        let info = RoomInfo::from(&r);
        assert_eq!(info.place.as_deref(), Some("Brussels HQ · Floor 2"));
        assert_eq!(info.equipment.as_deref(), Some("Display, Teams Room"));
        assert_eq!(RoomInfo::from(&Room { floor: Some("Ground floor".into()), building: None, ..r }).place.as_deref(), Some("Ground floor"));
        assert_eq!(room_note(Some(8)), "Room · 8 people");
        assert_eq!(room_note(None), "Room");
    }
}
