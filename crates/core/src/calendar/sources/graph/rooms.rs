//! The organization's rooms on Microsoft 365, from the Places API with
//! `Place.Read.All`: the room lists (usually buildings), each list's
//! rooms, and every room, so a room in no list is offered too.

use serde_json::{Value, json};

use super::{Api, GraphCalendar};
use crate::calendar::rooms::{Room, RoomSource};
use crate::{Error, Result};

/// Pages of one Places listing, followed to the end.
async fn every(api: &impl Api, first: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut url = first.to_string();
    for _ in 0..100 {
        let (status, page) = Api::request(api, "GET", &url, None, &[]).await?;
        match status {
            200 => {}
            // Without Place.Read.All Microsoft answers 403 with code
            // UnknownError and no message (Microsoft 365 test account,
            // 2026-11-07).
            401 | 403 => return Err(Error::Auth(format!("Microsoft refused the room lists ({status})"))),
            _ => return Err(Api::failure(api, status, &page)),
        }
        out.extend(page["value"].as_array().into_iter().flatten().cloned());
        match page["@odata.nextLink"].as_str() {
            Some(next) => url = next.to_string(),
            None => return Ok(out),
        }
    }
    Err(Error::Other("Microsoft kept sending more pages of rooms".into()))
}

impl GraphCalendar {
    /// Every bookable room, with the room list each is in. A tenant that
    /// hasn't set up Places answers with no lists and no rooms.
    pub async fn rooms(&self) -> Result<Vec<Room>> {
        list(self).await
    }
}

async fn list(api: &impl Api) -> Result<Vec<Room>> {
    let rooms = every(api, "/places/microsoft.graph.room?$top=100").await?;
    let mut lists = Vec::new();
    for list in every(api, "/places/microsoft.graph.roomlist?$top=100").await? {
        let Some(address) = list["emailAddress"].as_str().filter(|a| a.contains('@')) else { continue };
        let in_list = every(api, &format!("/places/{address}/microsoft.graph.roomlist/rooms?$top=100")).await?;
        lists.push(json!({ "displayName": list["displayName"], "emailAddress": address, "rooms": in_list }));
    }
    Ok(directory(&json!({ "rooms": rooms, "lists": lists })))
}

/// The rooms of a listing `{"rooms": [room…], "lists": [{"displayName",
/// "rooms": [room…]}…]}`, with Places' room objects: each room once, under
/// the first list that holds it, sorted by list and name, rooms in no list
/// last. A room Places marks `bookingType: reserved` can't be booked and is
/// left out, as is one without an address.
pub fn directory(listing: &Value) -> Vec<Room> {
    let mut out: Vec<Room> = Vec::new();
    let lists = listing["lists"].as_array().into_iter().flatten();
    let listed = lists.flat_map(|l| {
        let name = l["displayName"].as_str().or(l["emailAddress"].as_str()).map(str::to_string);
        l["rooms"].as_array().into_iter().flatten().map(move |r| (r, name.clone()))
    });
    let unlisted = listing["rooms"].as_array().into_iter().flatten().map(|r| (r, None));
    for (v, list) in listed.chain(unlisted) {
        let Some(room) = room(v, list) else { continue };
        if !out.iter().any(|r| r.address == room.address) {
            out.push(room);
        }
    }
    out.sort_by(|a, b| (a.list.is_none(), &a.list, a.name.to_lowercase()).cmp(&(b.list.is_none(), &b.list, b.name.to_lowercase())));
    out
}

fn room(v: &Value, list: Option<String>) -> Option<Room> {
    let address = v["emailAddress"].as_str().map(str::trim).filter(|a| a.contains('@'))?.to_ascii_lowercase();
    if v["bookingType"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("reserved")) {
        return None;
    }
    let text = |k: &str| v[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let floor = text("floorLabel").or_else(|| v["floorNumber"].as_i64().map(|n| n.to_string()));
    Some(Room {
        name: text("displayName").unwrap_or_else(|| address.clone()),
        capacity: v["capacity"].as_u64().and_then(|c| u32::try_from(c).ok()).filter(|c| *c > 0),
        building: text("building"),
        floor,
        list,
        accessible: v["isWheelChairAccessible"].as_bool().unwrap_or(false),
        video: text("videoDeviceName"),
        display: text("displayDeviceName"),
        source: RoomSource::Directory,
        address,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use chrono_tz::Tz;

    use super::*;

    /// Places room objects as the v1.0 room resource documents them.
    fn bruegel() -> Value {
        json!({"id": "3162F1E1", "emailAddress": "Bruegel@rooms.example", "displayName": "Bruegel", "capacity": 8, "building": "Brussels HQ",
               "floorNumber": 2, "floorLabel": null, "isWheelChairAccessible": true, "bookingType": "standard",
               "videoDeviceName": "Teams Room", "displayDeviceName": "Display", "audioDeviceName": null})
    }

    #[test]
    fn rooms_from_lists_and_without_one_skipping_reserved_ones() {
        let listing = json!({
            "lists": [{"displayName": "Brussels HQ · Floor 2", "emailAddress": "hq2@rooms.example", "rooms": [
                bruegel(),
                {"emailAddress": "brel@rooms.example", "displayName": "Brel", "capacity": null, "floorLabel": "2B", "bookingType": "standard"},
                {"emailAddress": "ceo@rooms.example", "displayName": "CEO office", "capacity": 4, "bookingType": "reserved"}
            ]}],
            "rooms": [
                bruegel(),
                {"emailAddress": "magritte@rooms.example", "displayName": "Magritte", "capacity": 0, "bookingType": "unknown"},
                {"displayName": "No address"}
            ]
        });
        let rooms = directory(&listing);
        let got: Vec<(&str, Option<&str>, Option<u32>, Option<&str>)> =
            rooms.iter().map(|r| (r.name.as_str(), r.list.as_deref(), r.capacity, r.floor.as_deref())).collect();
        assert_eq!(
            got,
            [
                ("Brel", Some("Brussels HQ · Floor 2"), None, Some("2B")),
                ("Bruegel", Some("Brussels HQ · Floor 2"), Some(8), Some("2")),
                ("Magritte", None, None, None),
            ],
            "Bruegel once, under its list; the reserved room and the one without an address are left out; 0 people is no size"
        );
        let b = &rooms[1];
        assert_eq!((b.address.as_str(), b.building.as_deref(), b.accessible), ("bruegel@rooms.example", Some("Brussels HQ"), true));
        assert_eq!((b.video.as_deref(), b.display.as_deref()), (Some("Teams Room"), Some("Display")));
        assert!(directory(&json!({"rooms": [], "lists": []})).is_empty(), "a tenant without Places set up");
    }

    struct Pages {
        asked: RefCell<Vec<String>>,
        refuse: bool,
    }

    impl Api for Pages {
        async fn request(&self, _: &str, url: &str, _: Option<&Value>, _: &[(&str, &str)]) -> Result<(u16, Value)> {
            self.asked.borrow_mut().push(url.to_string());
            if self.refuse {
                return Ok((403, json!({"error": {"code": "UnknownError", "message": ""}})));
            }
            Ok(match url {
                "/places/microsoft.graph.room?$top=100" => (200, json!({"value": [bruegel()], "@odata.nextLink": "https://graph.microsoft.com/v1.0/places/microsoft.graph.room?$skip=1"})),
                "https://graph.microsoft.com/v1.0/places/microsoft.graph.room?$skip=1" => (200, json!({"value": [{"emailAddress": "ensor@rooms.example", "displayName": "Ensor"}]})),
                "/places/microsoft.graph.roomlist?$top=100" => (200, json!({"value": [{"displayName": "HQ", "emailAddress": "hq@rooms.example"}]})),
                "/places/hq@rooms.example/microsoft.graph.roomlist/rooms?$top=100" => (200, json!({"value": [bruegel()]})),
                _ => (404, json!({})),
            })
        }

        fn failure(&self, status: u16, _: &Value) -> Error {
            Error::Other(format!("status {status}"))
        }

        fn zone(&self) -> Tz {
            chrono_tz::UTC
        }
    }

    #[tokio::test]
    async fn every_page_and_list_is_read_and_a_refusal_is_an_auth_error() {
        let api = Pages { asked: RefCell::new(Vec::new()), refuse: false };
        let rooms = list(&api).await.unwrap();
        let got: Vec<(&str, Option<&str>)> = rooms.iter().map(|r| (r.name.as_str(), r.list.as_deref())).collect();
        assert_eq!(got, [("Bruegel", Some("HQ")), ("Ensor", None)], "Ensor from the second page of rooms, in no list");
        assert_eq!(api.asked.borrow().len(), 4);
        let api = Pages { asked: RefCell::new(Vec::new()), refuse: true };
        assert!(matches!(list(&api).await, Err(Error::Auth(_))));
    }
}
