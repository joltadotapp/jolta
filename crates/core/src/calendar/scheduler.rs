//! Keeps calendars in sync while the app runs, and keeps the occurrence
//! table's window and zone current.
//!
//! One task syncs every shown calendar that is due: account calendars every
//! 5 minutes and right after `sync_now`, feeds at their refresh interval.
//! Up to four calendars sync at once, and a failure backs off only that
//! calendar. Each round also checks the occurrence table: when its window
//! start is more than 30 days behind, or the Mac moved to another zone,
//! every series is expanded again from the stored events, without network.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use futures::StreamExt;
use tokio::sync::Notify;

use super::series::Horizon;
use super::{Calendar, CalendarId, CalendarSource, sync, tz};
use crate::engine::{Event, Sink};
use crate::{Jolta, Result, store};

const ACCOUNT_EVERY: Duration = Duration::from_secs(5 * 60);
const FEED_DEFAULT: i64 = 6 * 3600;
/// A feed asking for a shorter interval than this gets this one.
const FEED_MIN: i64 = 15 * 60;
/// Rounds run at least this often, so a new zone or a new day is noticed.
const ROUND_MAX: Duration = Duration::from_secs(5 * 60);
const AT_ONCE: usize = 4;
/// How far the stored window start may fall behind before a full rebuild.
const WINDOW_SLACK: chrono::Duration = chrono::Duration::days(30);
/// Series rebuilt per transaction, so the app's own writes get turns.
const REBUILD_BATCH: usize = 500;

#[derive(Default)]
struct State {
    /// Sync every calendar on the next round.
    all: bool,
    wanted: HashSet<CalendarId>,
    next: HashMap<CalendarId, Instant>,
    /// Failed attempts in a row and the last error.
    failures: HashMap<CalendarId, (u32, String)>,
}

pub struct Scheduler {
    jolta: Jolta,
    sink: Sink,
    wake: Notify,
    state: Mutex<State>,
}

impl Scheduler {
    pub fn start(jolta: Jolta, sink: Sink, runtime: &tokio::runtime::Handle) -> Arc<Scheduler> {
        let scheduler = Arc::new(Scheduler { jolta, sink, wake: Notify::new(), state: Mutex::default() });
        runtime.spawn(scheduler.clone().run());
        scheduler
    }

    pub fn sync_now(&self) {
        self.state().all = true;
        self.wake.notify_one();
    }

    /// Syncs one calendar on the next round, such as one just shown.
    pub fn sync(&self, id: CalendarId) {
        self.state().wanted.insert(id);
        self.wake.notify_one();
    }

    /// Why the calendar's last sync failed, until one succeeds.
    pub fn error(&self, id: CalendarId) -> Option<String> {
        self.state().failures.get(&id).map(|(_, e)| e.clone())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn run(self: Arc<Self>) {
        loop {
            match refresh_occurrences(&self.jolta, Utc::now(), tz::here()).await {
                Ok(rebuilt) if !rebuilt.is_empty() => (self.sink)(Event::CalendarsChanged(rebuilt)),
                Ok(_) => {}
                Err(e) => tracing::warn!("rebuilding calendar occurrences stopped: {e}"),
            }
            let wait = self.round().await.unwrap_or_else(|e| {
                tracing::warn!("calendar sync round stopped: {e}");
                ROUND_MAX
            });
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Syncs every calendar that is due. Returns how long until the next one is.
    async fn round(&self) -> Result<Duration> {
        let calendars = self.jolta.store.read_async(store::calendars).await?;
        let calendars: Vec<Calendar> = calendars.into_iter().filter(|c| c.shown && syncs(c.source)).collect();
        let (now, unix) = (Instant::now(), Utc::now().timestamp());
        let due: Vec<Calendar> = {
            let mut state = self.state();
            let all = std::mem::take(&mut state.all);
            let wanted = std::mem::take(&mut state.wanted);
            let present: HashSet<CalendarId> = calendars.iter().map(|c| c.id).collect();
            state.next.retain(|id, _| present.contains(id));
            state.failures.retain(|id, _| present.contains(id));
            calendars
                .into_iter()
                .filter(|c| {
                    // After a restart, the last sync's time says when the next is due.
                    let next = *state.next.entry(c.id).or_insert_with(|| {
                        let left = c.synced.map_or(0, |s| s + interval(c).as_secs() as i64 - unix);
                        now + Duration::from_secs(left.max(0) as u64)
                    });
                    all || wanted.contains(&c.id) || next <= now
                })
                .collect()
        };
        futures::stream::iter(due).for_each_concurrent(AT_ONCE, |c| self.sync_one(c)).await;
        let next = self.state().next.values().min().map(|t| t.saturating_duration_since(Instant::now()));
        Ok(next.map_or(ROUND_MAX, |n| n.min(ROUND_MAX)))
    }

    async fn sync_one(&self, calendar: Calendar) {
        let result = sync::sync_calendar(&self.jolta, calendar.id).await;
        let every = interval(&calendar);
        let error = {
            let mut state = self.state();
            match &result {
                Ok(_) => {
                    state.failures.remove(&calendar.id);
                    state.next.insert(calendar.id, Instant::now() + every);
                    None
                }
                Err(e) => {
                    let attempts = state.failures.get(&calendar.id).map_or(0, |(n, _)| *n) + 1;
                    state.failures.insert(calendar.id, (attempts, e.to_string()));
                    state.next.insert(calendar.id, Instant::now() + backoff(attempts, every));
                    Some(e.to_string())
                }
            }
        };
        match &result {
            Ok(r) if r.upserted + r.removed > 0 => (self.sink)(Event::CalendarsChanged(vec![calendar.id])),
            Ok(_) => {}
            Err(e) => tracing::info!("calendar {} ({}) didn't sync: {e}", calendar.id, calendar.name),
        }
        (self.sink)(Event::CalendarSynced { calendar: calendar.id, error });
    }
}

/// Local and EventKit calendars have nothing to download.
fn syncs(source: CalendarSource) -> bool {
    matches!(source, CalendarSource::CalDav | CalendarSource::Webcal | CalendarSource::Google | CalendarSource::Graph)
}

fn interval(c: &Calendar) -> Duration {
    match c.source {
        CalendarSource::Webcal => Duration::from_secs(c.refresh.unwrap_or(FEED_DEFAULT).max(FEED_MIN) as u64),
        _ => ACCOUNT_EVERY,
    }
}

/// One minute after the first failure, doubling, never longer than the
/// calendar's own interval.
fn backoff(attempts: u32, every: Duration) -> Duration {
    Duration::from_secs(60 << attempts.saturating_sub(1).min(10)).min(every)
}

/// Expands every series again from the stored events when the table's
/// window start is more than 30 days behind `now` minus a year, or it was
/// built in another zone than `here` (floating events and all-day ordering
/// depend on it). Returns the calendars it rebuilt.
pub async fn refresh_occurrences(jolta: &Jolta, now: DateTime<Utc>, here: Tz) -> Result<Vec<CalendarId>> {
    let horizon = Horizon::around(now, here);
    let built = jolta.store.read_async(store::occurrences_built).await?;
    let oldest = (horizon.window.start - WINDOW_SLACK).timestamp();
    if built.is_some_and(|b| b.zone == here.name() && b.start >= oldest) {
        return Ok(Vec::new());
    }
    let keys = jolta.store.read_async(store::series_keys).await?;
    // Without events there is nothing to rebuild, and a mail-only user's
    // store gets no calendar write at every start.
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    for batch in keys.chunks(REBUILD_BATCH) {
        let batch = batch.to_vec();
        jolta
            .store
            .write_async(move |tx| batch.iter().try_for_each(|(c, uid)| store::rebuild_series(tx, *c, uid, &horizon)))
            .await?;
    }
    jolta.store.write_async(move |tx| store::set_occurrences_built(tx, &horizon)).await?;
    let mut calendars: Vec<CalendarId> = keys.into_iter().map(|(c, _)| c).collect();
    calendars.dedup();
    Ok(calendars)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as Days, TimeZone};
    use chrono_tz::{America::New_York, Europe::Brussels};

    use super::*;
    use crate::calendar::{Access, EventData, Local, Span, Zone};
    use crate::store::RemoteCalendar;

    fn starts(jolta: &Jolta) -> Vec<i64> {
        jolta
            .store
            .read(|c| Ok(c.prepare("SELECT start_utc FROM occurrence ORDER BY 1")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?))
            .unwrap()
    }

    /// A floating 09:00 moves with the zone, and the window follows the date.
    #[tokio::test]
    async fn occurrences_follow_the_zone_and_the_date() {
        let dir = std::env::temp_dir().join(format!("jolta-cal-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jolta = Jolta::open(&dir).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap();
        let wall = chrono::NaiveDate::from_ymd_opt(2026, 11, 9).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let floating = Local { wall, zone: Zone::Floating };
        let event = EventData::new("float", Span::Timed { start: floating, end: Local { wall: wall + Days::hours(1), ..floating } });
        let feed = RemoteCalendar {
            remote_id: "feed".into(),
            name: "Feed".into(),
            color: None,
            zone: None,
            access: Access::Read,
            primary: true,
            refresh: None,
        };
        jolta
            .store
            .write(|tx| {
                let cal = store::add_calendar(tx, CalendarSource::Local, &feed)?;
                store::apply_item(tx, cal.id, "float", None, &[event], None, &Horizon::around(now, Brussels))
            })
            .unwrap();
        assert_eq!(starts(&jolta), [Brussels.from_local_datetime(&wall).unwrap().timestamp()]);

        assert_eq!(refresh_occurrences(&jolta, now, Brussels).await.unwrap().len(), 1, "never built before: rebuilt once");
        assert!(refresh_occurrences(&jolta, now, Brussels).await.unwrap().is_empty(), "same zone and day: nothing to do");

        assert_eq!(refresh_occurrences(&jolta, now, New_York).await.unwrap().len(), 1);
        assert_eq!(starts(&jolta), [New_York.from_local_datetime(&wall).unwrap().timestamp()], "the floating event moved");

        let later = now + Days::days(20);
        assert!(refresh_occurrences(&jolta, later, New_York).await.unwrap().is_empty(), "20 days on, the window is close enough");
        let much_later = now + Days::days(400);
        assert_eq!(refresh_occurrences(&jolta, much_later, New_York).await.unwrap().len(), 1);
        assert!(starts(&jolta).is_empty(), "a year and a month on, the event left the window");
        let built = jolta.store.read(store::occurrences_built).unwrap().unwrap();
        assert_eq!((built.start, built.zone.as_str()), ((much_later - Days::days(365)).timestamp(), "America/New_York"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_back_off_up_to_the_interval() {
        let waits: Vec<u64> = (1..=6).map(|n| backoff(n, ACCOUNT_EVERY).as_secs()).collect();
        assert_eq!(waits, [60, 120, 240, 300, 300, 300]);
        assert_eq!(backoff(40, Duration::from_secs(6 * 3600)).as_secs(), 6 * 3600);
    }
}
