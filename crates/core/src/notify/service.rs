//! Runs the planner for the engine: feeds it the mail sync rounds store,
//! closes each gather on time, takes back notifications whose mail was read
//! anywhere, and keeps the badge number current. What it decides goes to
//! the shell as `Event::Notice`.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::Notify;

use super::planner::{Planner, Reminder};
use super::{Notice, Settings, badge_count, candidates, still_waiting};
use crate::ai::background::Background;
use crate::engine::{Event, Sink};
use crate::model::*;
use crate::{Jolta, Result};

/// Lists change in bursts while a sync stores mail; a check waits this
/// long after the first change so one check covers the burst.
const SETTLE: Duration = Duration::from_millis(300);

pub struct Notices {
    jolta: Jolta,
    sink: Sink,
    state: Mutex<State>,
    wake: Notify,
    background: Mutex<Option<Arc<Background>>>,
}

struct State {
    planner: Planner,
    settings: Settings,
    /// The badge number the shell has, once it has one.
    badge: Option<u32>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Notices {
    /// `sink` gets the notices; it must not be one that calls `changed`.
    pub fn start(jolta: Jolta, sink: Sink, runtime: &tokio::runtime::Handle) -> Result<Arc<Notices>> {
        let settings = jolta.store.read(Settings::read)?;
        let state = State { planner: Planner::new(settings), settings, badge: None };
        let notices = Arc::new(Notices { jolta, sink, state: Mutex::new(state), wake: Notify::new(), background: Mutex::new(None) });
        let runner = notices.clone();
        runtime.spawn(async move { runner.run().await });
        notices.wake.notify_one();
        Ok(notices)
    }

    /// AI sorting's queue, so mail it hasn't sorted yet waits for it.
    pub fn set_background(&self, background: Arc<Background>) {
        *lock(&self.background) = Some(background);
    }

    /// Mail a sync round stored for the first time.
    pub fn arrived(&self, ids: &[MessageId]) {
        let now = Utc::now();
        let sorting = self.sorting();
        let mut state = lock(&self.state);
        let fresh = match self.jolta.store.read(|c| candidates(c, ids, now, sorting)) {
            Ok(fresh) => fresh,
            Err(e) => return tracing::warn!("new mail wasn't looked at for notifications: {e}"),
        };
        let out = state.planner.arrive(fresh, now.timestamp());
        drop(state);
        self.send(out);
        self.wake.notify_one();
    }

    /// Stored mail changed: read, archived, sorted, here or elsewhere.
    pub fn changed(&self) {
        self.wake.notify_one();
    }

    pub fn set_foreground(&self, active: bool) {
        lock(&self.state).planner.set_foreground(active);
    }

    /// The user changed the settings in the store.
    pub fn settings_changed(&self) -> Result<()> {
        let settings = self.jolta.store.read(Settings::read)?;
        let mut state = lock(&self.state);
        state.settings = settings;
        state.planner.set_settings(settings);
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    /// Reminders that fell due: back from snooze, or no reply yet.
    pub fn remind(&self, kind: Reminder, threads: Vec<(AccountId, ThreadId, String)>) {
        let out = lock(&self.state).planner.remind(kind, threads, Utc::now().timestamp());
        self.send(out);
    }

    fn sorting(&self) -> bool {
        lock(&self.background).as_ref().is_some_and(|b| b.sorts_now(&self.jolta))
    }

    fn send(&self, notices: Vec<Notice>) {
        for n in notices {
            (self.sink)(Event::Notice(n));
        }
    }

    async fn run(self: Arc<Self>) {
        loop {
            let due = lock(&self.state).planner.due();
            let wait = due.map_or(Duration::from_secs(3600), |d| Duration::from_secs((d - Utc::now().timestamp()).max(0) as u64));
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
            tokio::time::sleep(SETTLE).await;
            let me = self.clone();
            match tokio::task::spawn_blocking(move || me.check()).await {
                Ok(Err(e)) => tracing::warn!("checking notifications stopped: {e}"),
                Err(e) => tracing::warn!("checking notifications stopped: {e}"),
                Ok(Ok(())) => {}
            }
        }
    }

    /// Looks at the waiting and the shown mail again: takes back what was
    /// read, closes a gather that is due, and sends a changed badge number.
    fn check(&self) -> Result<()> {
        let now = Utc::now();
        let sorting = self.sorting();
        let mut state = lock(&self.state);
        let (pending, shown, settings) = (state.planner.pending(), state.planner.shown(), state.settings);
        let (fresh, still, badge) = self.jolta.store.read(|c| {
            Ok((candidates(c, &pending, now, sorting)?, still_waiting(c, &shown)?, badge_count(c, &settings)?))
        })?;
        state.planner.refresh(fresh);
        let mut out = state.planner.withdraw(&still);
        out.extend(state.planner.tick(now.timestamp()));
        if state.badge != Some(badge) {
            state.badge = Some(badge);
            out.push(Notice::Badge(badge));
        }
        drop(state);
        self.send(out);
        Ok(())
    }
}
