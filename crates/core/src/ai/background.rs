//! AI work nobody waits for: sorting new mail, and summaries of long new
//! conversations, so they are ready when opened. One job at a time, sorting
//! before summaries. With the local model it pauses while the app says the
//! Mac is on battery, in Low Power Mode or hot; cloud providers run anyway.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::Notify;

use rusqlite::OptionalExtension;

use super::{Ai, Cancel, Provider, tasks, triage};
use crate::model::*;
use crate::{Jolta, Result};

/// How far back sorting reaches when it is switched on or Jolta starts.
pub const CATCH_UP_DAYS: i64 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Job {
    Sort(MessageId),
    Summary(ThreadId),
}

#[derive(Default)]
struct Queue {
    sort: VecDeque<MessageId>,
    summaries: VecDeque<ThreadId>,
    queued: BTreeSet<Job>,
}

impl Queue {
    fn push(&mut self, job: Job) {
        if self.queued.insert(job) {
            match job {
                Job::Sort(m) => self.sort.push_back(m),
                Job::Summary(t) => self.summaries.push_back(t),
            }
        }
    }

    fn pop(&mut self) -> Option<Job> {
        let job = self.sort.pop_front().map(Job::Sort).or_else(|| self.summaries.pop_front().map(Job::Summary))?;
        self.queued.remove(&job);
        Some(job)
    }

    fn len(&self) -> usize {
        self.queued.len()
    }
}

/// Called after a batch of results lands, so lists can show them.
pub type Changed = Arc<dyn Fn() + Send + Sync>;

pub struct Background {
    queue: Mutex<Queue>,
    wake: Notify,
    /// False while the Mac should be left alone (battery, Low Power Mode, heat).
    allowed: AtomicBool,
}

impl Background {
    pub fn start(jolta: Jolta, ai: Arc<Ai>, changed: Changed, runtime: &tokio::runtime::Handle) -> Arc<Background> {
        let bg = Arc::new(Background { queue: Mutex::default(), wake: Notify::new(), allowed: AtomicBool::new(true) });
        let worker = bg.clone();
        runtime.spawn(async move { worker.run(jolta, ai, changed).await });
        bg
    }

    pub fn set_allowed(&self, allowed: bool) {
        self.allowed.store(allowed, Ordering::SeqCst);
        if allowed {
            self.wake.notify_one();
        }
    }

    /// Jobs waiting, for "Sorting 12 messages…".
    pub fn pending(&self) -> usize {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn push(&self, jobs: impl IntoIterator<Item = Job>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs {
            queue.push(job);
        }
        drop(queue);
        self.wake.notify_one();
    }

    /// A sync round stored these messages: sort each one that is in an
    /// inbox and not the user's own, and summarise the conversations they
    /// are in when those are long enough.
    pub fn arrived(&self, jolta: &Jolta, messages: &[MessageId]) {
        let Ok((settings, found)) = jolta.store.read(|c| {
            let settings = super::settings(c)?;
            let mut stmt = c.prepare_cached(
                "SELECT m.thread_id, NOT EXISTS (SELECT 1 FROM ai_triage t WHERE t.message_id = m.id) FROM message m
                 WHERE m.id = ?1
                   AND EXISTS (SELECT 1 FROM message_mailbox mm JOIN mailbox b ON b.id = mm.mailbox_id
                               WHERE mm.message_id = m.id AND b.role = 'inbox')
                   AND NOT EXISTS (SELECT 1 FROM own_address a WHERE a.address = m.from_addr COLLATE NOCASE)",
            )?;
            let mut found: Vec<(MessageId, ThreadId, bool)> = Vec::new();
            for &id in messages {
                if let Some((thread, unsorted)) = stmt.query_row([id], |r| Ok((r.get(0)?, r.get(1)?))).optional()? {
                    found.push((id, thread, unsorted));
                }
            }
            Ok((settings, found))
        }) else {
            return;
        };
        let mut jobs = Vec::new();
        if settings.sorts_new_mail() {
            jobs.extend(found.iter().filter(|(_, _, unsorted)| *unsorted).map(|(m, _, _)| Job::Sort(*m)));
        }
        if settings.auto_summary && settings.provider == Some(Provider::Local) {
            jobs.extend(found.iter().map(|(_, t, _)| Job::Summary(*t)));
        }
        self.push(jobs);
    }

    /// Sorting runs now: it is on, and the local model may use the Mac.
    pub fn sorts_now(&self, jolta: &Jolta) -> bool {
        jolta.store.read(super::settings).is_ok_and(|s| {
            s.sorts_new_mail() && (s.provider != Some(Provider::Local) || self.allowed.load(Ordering::SeqCst))
        })
    }

    /// Sorts the inbox mail of the last two weeks that isn't sorted yet:
    /// at start, and when sorting is switched on.
    pub fn catch_up(&self, jolta: &Jolta) {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) - CATCH_UP_DAYS * 86_400;
        let Ok(Some(ids)) = jolta.store.read(|c| {
            if !super::settings(c)?.sorts_new_mail() {
                return Ok(None);
            }
            Ok(Some(triage::unsorted_inbox(c, since)?))
        }) else {
            return;
        };
        self.push(ids.into_iter().map(Job::Sort));
    }

    async fn run(self: Arc<Self>, jolta: Jolta, ai: Arc<Ai>, changed: Changed) {
        let mut done_since_report = 0;
        loop {
            let job = self.queue.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let Some(job) = job else {
                if done_since_report > 0 {
                    changed();
                    done_since_report = 0;
                }
                self.wake.notified().await;
                continue;
            };
            let local = jolta.store.read(super::settings).map(|s| s.provider == Some(Provider::Local)).unwrap_or(false);
            while local && !self.allowed.load(Ordering::SeqCst) {
                if done_since_report > 0 {
                    changed();
                    done_since_report = 0;
                }
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            }
            if let Err(e) = Self::work(&jolta, &ai, job).await {
                tracing::warn!("background AI job {job:?} failed: {e}");
                // A provider that is down or refusing would fail every job; slow down.
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            done_since_report += 1;
            if done_since_report >= 10 {
                changed();
                done_since_report = 0;
            }
        }
    }

    async fn work(jolta: &Jolta, ai: &Ai, job: Job) -> Result<()> {
        let cancel = Cancel::default();
        match job {
            Job::Sort(message) => {
                triage::sort(jolta, ai, message, &cancel).await?;
            }
            Job::Summary(thread) => {
                tasks::summarise(jolta, ai, thread, true, false, &cancel, &mut |_| {}).await?;
            }
        }
        Ok(())
    }
}
