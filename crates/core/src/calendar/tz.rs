//! Time zones: names from every source to IANA zones, and wall-clock times
//! to instants.

use chrono::{DateTime, NaiveDateTime, Utc};
use chrono_tz::Tz;

use super::{Bounds, Local, Span};

/// The IANA zone for a TZID: an IANA name, a Windows name as Graph and
/// Outlook use (`Romance Standard Time`), or a known alias.
pub fn zone(name: &str) -> Option<Tz> {
    let _ = name;
    todo!()
}

/// The instant a wall-clock time names in `tz`. A time the clocks skip moves
/// forward by the gap (RFC 5545 section 3.3.5); a time that happens twice
/// means the first.
pub fn instant(wall: NaiveDateTime, tz: Tz) -> DateTime<Utc> {
    let _ = (wall, tz);
    todo!()
}

impl Local {
    /// `here` is the zone the Mac is in, used for floating times.
    pub fn instant(&self, here: Tz) -> DateTime<Utc> {
        let _ = here;
        todo!()
    }
}

impl Span {
    pub fn bounds(&self, here: Tz) -> Bounds {
        let _ = here;
        todo!()
    }
}
