//! Turning calendar on and off per account, feed subscriptions, and syncing
//! one calendar from its source into the store.

use std::time::{Duration, Instant};

use super::series::Horizon;
use super::sources::{Change, Source, caldav::CalDav, google::Google, graph::GraphCalendar, webcal::Webcal};
use super::{Calendar, CalendarId, CalendarSource};
use crate::store::{self, RemoteCalendar};
use crate::{Account, AccountId, AccountKind, Error, Jolta, Result};

/// Whether turning calendar on needs the user to grant calendar access in
/// the browser first (`oauth::begin_calendar`). Google and Microsoft accounts
/// do until calendar is on; CalDAV uses the mail password.
pub fn needs_consent(account: &Account) -> bool {
    matches!(account.kind, AccountKind::Gmail | AccountKind::Graph) && !account.calendar
}

/// The source an account's calendars come from.
async fn account_source(jolta: &Jolta, account: &Account) -> Result<Source> {
    match account.kind {
        AccountKind::Imap => {
            let id = account.id;
            let url = jolta.store.read_async(move |c| store::caldav_url(c, id)).await?;
            Ok(Source::CalDav(CalDav::new(account, url.as_deref())?))
        }
        AccountKind::Gmail => Ok(Source::Google(Google::new(account)?)),
        AccountKind::Graph => Ok(Source::Graph(GraphCalendar::new(account)?)),
    }
}

pub(super) async fn calendar_source(jolta: &Jolta, calendar: &Calendar) -> Result<Source> {
    match (calendar.source, calendar.account_id) {
        (CalendarSource::Webcal, _) => Ok(Source::Webcal(Webcal::new(&calendar.remote_id)?)),
        (CalendarSource::CalDav | CalendarSource::Google | CalendarSource::Graph, Some(id)) => {
            let account = jolta.store.read_async(move |c| store::account(c, id)).await?;
            account_source(jolta, &account).await
        }
        (source, _) => Err(Error::Other(format!("syncing {source} calendars isn't built yet"))),
    }
}

/// Turns calendar on for an account and lists its calendars: the primary
/// one shown, the others hidden until the user picks them. Calling it again
/// refreshes the list and keeps those choices. When the server can't be
/// reached, calendar stays off. For an account that `needs_consent`, run
/// the calendar sign-in first; calendar turns on only when the list comes
/// back, so a refused grant leaves the account as it was.
pub async fn enable(jolta: &Jolta, account: AccountId) -> Result<Vec<Calendar>> {
    let a = jolta.store.read_async(move |c| store::account(c, account)).await?;
    let source = account_source(jolta, &Account { calendar: true, ..a }).await?;
    let list = source.list_calendars().await?;
    let kind = source.kind();
    jolta
        .store
        .write_async(move |tx| {
            store::set_account_calendar(tx, account, true)?;
            store::sync_calendar_list(tx, account, kind, &list)
        })
        .await
}

/// Turns calendar off: the account's calendars, events and sync state go.
/// Nothing changes on the server.
pub fn disable(jolta: &Jolta, account: AccountId) -> Result<()> {
    jolta.store.write(|tx| store::set_account_calendar(tx, account, false))
}

/// Adds a feed subscription, shown and not counting as busy.
pub async fn subscribe(jolta: &Jolta, url: &str) -> Result<Calendar> {
    let feed = Webcal::new(url)?;
    let list = feed.list_calendars().await?;
    let feed: RemoteCalendar = list.into_iter().next().ok_or_else(|| Error::Other("the feed has no calendar".into()))?;
    jolta.store.write_async(move |tx| store::add_calendar(tx, CalendarSource::Webcal, &feed)).await
}

/// What a sync changed in the store. Objects the source sent again
/// unchanged don't count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub upserted: usize,
    pub removed: usize,
    pub elapsed: Duration,
}

/// Brings one calendar up to date. The changes and the cursor that follows
/// them land in one transaction, so a sync cut off halfway starts again
/// from the old cursor, and applying the same changes twice is harmless.
pub async fn sync_calendar(jolta: &Jolta, id: CalendarId) -> Result<SyncReport> {
    let started = Instant::now();
    let calendar = jolta.store.read_async(move |c| store::calendar(c, id)).await?;
    let source = calendar_source(jolta, &calendar).await?;
    let known = jolta.store.read_async(move |c| store::known_items(c, id)).await?;
    let batch = source.sync(&calendar, &known).await?;

    let mut changes = Vec::with_capacity(batch.changes.len());
    for change in batch.changes {
        changes.push(match change {
            Change::Upsert { remote_id, etag, events, raw } => {
                let blob = raw.map(|r| jolta.blobs.put(&r)).transpose()?;
                (remote_id, Some((etag, events, blob)))
            }
            Change::Remove { remote_id } => (remote_id, None),
        });
    }
    let horizon = Horizon::now();
    let report = jolta
        .store
        .write_async(move |tx| {
            let mut report = SyncReport::default();
            for (remote_id, change) in &changes {
                match change {
                    Some((etag, events, blob)) => {
                        report.upserted += usize::from(store::apply_item(
                            tx,
                            id,
                            remote_id,
                            etag.as_deref(),
                            events,
                            blob.as_deref(),
                            &horizon,
                        )?);
                    }
                    None => report.removed += usize::from(store::remove_item(tx, id, remote_id, &horizon)?),
                }
            }
            store::set_calendar_cursor(tx, id, batch.cursor.as_deref(), batch.refresh)?;
            Ok(report)
        })
        .await?;
    Ok(SyncReport { elapsed: started.elapsed(), ..report })
}

/// The calendars a sync covers: every shown one, since hidden calendars
/// aren't synced at all.
pub fn shown(jolta: &Jolta) -> Result<Vec<Calendar>> {
    Ok(jolta.store.read(store::calendars)?.into_iter().filter(|c| c.shown).collect())
}
