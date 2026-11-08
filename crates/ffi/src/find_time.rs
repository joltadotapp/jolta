//! Find a time for the app: who is busy when over the next working days,
//! the slots that suit everyone known, and the text a picked slot or the
//! user's free times turn into. Times cross as unix seconds.

use chrono::{DateTime, Duration, NaiveTime, Utc, Weekday};
use jolta_core::calendar::availability::{self, Availability, Busy, Schedule, SuggestOptions, Unknown, Weekdays, WorkingHours};
use jolta_core::calendar::freebusy::{self, Found, Lookup};
use jolta_core::calendar::{Person, tz};
use jolta_core::{AccountId, MessageId, ThreadId, compose, store};

use crate::calendar::TimeSlot;
use crate::{ComposeData, Core, CoreError, ReplyMode, Result};

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusyKind {
    Busy,
    Tentative,
    OutOfOffice,
    Elsewhere,
}

impl From<availability::BusyKind> for BusyKind {
    fn from(k: availability::BusyKind) -> Self {
        use availability::BusyKind as K;
        match k {
            K::Busy => BusyKind::Busy,
            K::Tentative => BusyKind::Tentative,
            K::OutOfOffice => BusyKind::OutOfOffice,
            K::Elsewhere => BusyKind::Elsewhere,
        }
    }
}

impl From<BusyKind> for availability::BusyKind {
    fn from(k: BusyKind) -> Self {
        use availability::BusyKind as K;
        match k {
            BusyKind::Busy => K::Busy,
            BusyKind::Tentative => K::Tentative,
            BusyKind::OutOfOffice => K::OutOfOffice,
            BusyKind::Elsewhere => K::Elsewhere,
        }
    }
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct BusyBlock {
    pub start: i64,
    pub end: i64,
    pub kind: BusyKind,
}

/// One row of the panel: the user first, then everyone looked up.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct PersonRow {
    pub address: String,
    /// "Ann Berg", or the address.
    pub name: String,
    pub is_me: bool,
    pub busy: Vec<BusyBlock>,
    /// Why the busy times aren't known; the row is hatched. None when known.
    pub unknown: Option<String>,
    /// Their working hours, when the provider says (Microsoft 365).
    pub hours: Option<WorkingHoursInfo>,
    /// The shown times outside their working hours, shaded.
    pub outside_hours: Vec<TimeSlot>,
    /// "Out of office Fri" or "Works 09:00 to 15:00", under the name.
    pub note: Option<String>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct WorkingDay {
    /// "Tue 10 Nov".
    pub title: String,
    pub start: i64,
    pub end: i64,
}

/// Working hours as Settings › Calendar edits them, on `zone`'s clock.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct WorkingHoursInfo {
    /// Monday first.
    pub days: Vec<bool>,
    /// Minutes after midnight.
    pub start: u32,
    pub end: u32,
    /// "Europe/Brussels".
    pub zone: String,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FindTimeInfo {
    pub account_id: i64,
    pub people: Vec<PersonRow>,
    pub days: Vec<WorkingDay>,
    pub hours: WorkingHoursInfo,
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum FindTimeOutcome {
    Ready { info: FindTimeInfo },
    /// Other people's times need calendar on for the account.
    /// `needs_consent`: turning it on opens the browser first.
    CalendarOff { account_id: i64, address: String, needs_consent: bool },
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FindTimeSlot {
    pub start: i64,
    pub end: i64,
    /// Names of the people whose times aren't known.
    pub missing: Vec<String>,
}

fn at(t: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(t, 0).unwrap_or_default()
}

fn hours_info(h: &WorkingHours) -> WorkingHoursInfo {
    let minutes = |t: NaiveTime| (t - NaiveTime::MIN).num_minutes() as u32;
    let days = std::iter::successors(Some(Weekday::Mon), |d| Some(d.succ())).take(7).map(|d| h.days.has(d)).collect();
    WorkingHoursInfo { days, start: minutes(h.start), end: minutes(h.end), zone: h.zone.name().to_string() }
}

fn hours_of(info: &WorkingHoursInfo) -> Result<WorkingHours> {
    let time = |m: u32| {
        NaiveTime::from_num_seconds_from_midnight_opt(m.min(24 * 60 - 1) * 60, 0).ok_or_else(|| CoreError::Query { message: format!("{m} minutes is no time") })
    };
    let days = info.days.iter().zip(std::iter::successors(Some(Weekday::Mon), |d| Some(d.succ()))).filter(|(on, _)| **on).map(|(_, d)| d);
    let zone = tz::zone(&info.zone).ok_or_else(|| CoreError::Query { message: format!("{} is no time zone", info.zone) })?;
    Ok(WorkingHours { days: Weekdays::of(days), start: time(info.start)?, end: time(info.end)?, zone })
}

fn blocks(busy: &[Busy]) -> Vec<BusyBlock> {
    busy.iter().map(|b| BusyBlock { start: b.start.timestamp(), end: b.end.timestamp(), kind: b.kind.into() }).collect()
}

fn info(found: Found, people: &[Person]) -> FindTimeInfo {
    let name = |address: &str| {
        people.iter().find(|p| p.address.eq_ignore_ascii_case(address)).map(|p| p.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| address.to_string())
    };
    let me = PersonRow { address: found.me.clone(), name: "You".into(), is_me: true, busy: blocks(&found.own), unknown: None, hours: None, outside_hours: Vec::new(), note: None };
    let mut rows = vec![me];
    rows.extend(found.others.iter().map(|s| {
        let (busy, unknown) = match &s.availability {
            Availability::Known(b) => (blocks(b), None),
            Availability::Unknown(why) => (Vec::new(), Some(why.reason())),
        };
        let outside_hours = s
            .hours
            .iter()
            .flat_map(|h| found.windows.iter().flat_map(|w| h.off(w.start, w.end)))
            .map(|(start, end)| TimeSlot { start: start.timestamp(), end: end.timestamp() })
            .collect();
        PersonRow {
            address: s.person.clone(),
            name: name(&s.person),
            is_me: false,
            busy,
            unknown,
            hours: s.hours.as_ref().map(hours_info),
            outside_hours,
            note: availability::note(s, &found.windows, &found.hours),
        }
    }));
    let days = found
        .windows
        .iter()
        .map(|w| WorkingDay { title: w.day.format("%a %-d %b").to_string(), start: w.start.timestamp(), end: w.end.timestamp() })
        .collect();
    FindTimeInfo { account_id: found.account.0, people: rows, days, hours: hours_info(&found.hours) }
}

/// The own busy times and the others' schedules back from the panel's copy.
fn parts(info: &FindTimeInfo) -> Result<(Vec<Busy>, Vec<Schedule>)> {
    let busy = |row: &PersonRow| row.busy.iter().map(|b| Busy { start: at(b.start), end: at(b.end), kind: b.kind.into() }).collect::<Vec<_>>();
    let own = info.people.iter().filter(|r| r.is_me).flat_map(busy).collect();
    let others = info
        .people
        .iter()
        .filter(|r| !r.is_me)
        .map(|r| {
            Ok(Schedule {
                person: r.name.clone(),
                availability: match &r.unknown {
                    None => Availability::Known(busy(r)),
                    Some(why) => Availability::Unknown(Unknown::Failed(why.clone())),
                },
                hours: r.hours.as_ref().map(hours_of).transpose()?,
            })
        })
        .collect::<Result<_>>()?;
    Ok((own, others))
}

fn free(info: &FindTimeInfo, minutes: i64) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
    let (own, _) = parts(info)?;
    let hours = hours_of(&info.hours)?;
    Ok(availability::free_times(&own, &hours, Utc::now(), info.days.len() as u32, Duration::minutes(minutes)))
}

impl Core {
    async fn find_time(&self, account: AccountId, people: Vec<Person>, days: u32) -> Result<FindTimeOutcome> {
        let jolta = self.jolta.clone();
        let addresses: Vec<String> = people.iter().map(|p| p.address.clone()).collect();
        let lookup = self
            .runtime
            .spawn(async move { freebusy::look_up(&jolta, account, &addresses, Utc::now(), days).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(match lookup {
            Lookup::Found(found) => FindTimeOutcome::Ready { info: info(found, &people) },
            Lookup::CalendarOff { account, address, needs_consent } => FindTimeOutcome::CalendarOff { account_id: account.0, address, needs_consent },
        })
    }
}

#[uniffi::export]
impl Core {
    /// Find a time with the people on a conversation, through the account
    /// of its newest message.
    pub async fn find_time_for_thread(&self, thread_id: i64, days: u32) -> Result<FindTimeOutcome> {
        let (account, people) = self.jolta.store.read(|c| freebusy::thread_people(c, ThreadId(thread_id)))?;
        self.find_time(account, people, days).await
    }

    /// Find a time with the organizer and guests of the invitation on a
    /// message, through the account it came to.
    pub async fn find_time_for_invitation(&self, message_id: i64, days: u32) -> Result<FindTimeOutcome> {
        let jolta = self.jolta.clone();
        let (account, people) = self
            .runtime
            .spawn(async move { freebusy::invitation_people(&jolta, MessageId(message_id)).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        self.find_time(account, people, days).await
    }

    /// The user's own busy times, for Insert my availability.
    pub async fn find_time_for_me(&self, account_id: i64, days: u32) -> Result<FindTimeOutcome> {
        self.find_time(AccountId(account_id), Vec::new(), days).await
    }

    /// The slots of `minutes` that suit everyone known, inside each known
    /// person's working hours when `respect_others_hours`.
    pub fn find_time_slots(&self, info: FindTimeInfo, minutes: i64, respect_others_hours: bool) -> Result<Vec<FindTimeSlot>> {
        let (own, others) = parts(&info)?;
        let hours = hours_of(&info.hours)?;
        let days = info.days.len() as u32;
        Ok(availability::suggest(&own, &others, &hours, Duration::minutes(minutes), Utc::now(), days, SuggestOptions { respect_others_hours })
            .into_iter()
            .map(|s| FindTimeSlot { start: s.start.timestamp(), end: s.end.timestamp(), missing: s.missing })
            .collect())
    }

    /// The user's free stretches of at least `minutes`, for Insert my availability.
    pub fn free_times(&self, info: FindTimeInfo, minutes: i64) -> Result<Vec<FindTimeSlot>> {
        Ok(free(&info, minutes)?.into_iter().map(|(s, e)| FindTimeSlot { start: s.timestamp(), end: e.timestamp(), missing: Vec::new() }).collect())
    }

    /// The user's free stretches of at least `minutes` as lines for a mail,
    /// on this Mac's clock: "Tue 10 Nov (CET): 10:00 to 11:30, 14:00 to 16:00".
    pub fn availability_text(&self, info: FindTimeInfo, minutes: i64) -> Result<String> {
        let lines = availability::availability_text(&free(&info, minutes)?, tz::here());
        Ok(format!("I'm free at these times:\n{lines}"))
    }

    /// The body with `text` added after what the writer typed, above the
    /// signature and the quote.
    pub fn add_to_written(&self, body: String, account_id: Option<i64>, text: String) -> Result<String> {
        let signature = match account_id {
            Some(id) => self.jolta.store.read(|c| store::account(c, AccountId(id)))?.signature,
            None => String::new(),
        };
        let written = compose::written(&body, &signature);
        let text = if written.is_empty() { text } else { format!("{written}\n\n{text}") };
        Ok(compose::with_written(&body, &signature, &text))
    }

    /// A reply to the people Find a time looked up on the conversation
    /// proposing the slot, on this Mac's clock: "Would Tuesday 10 November,
    /// 14:00 to 14:30 (CET) work for you?". It stays a draft until the user
    /// sends it.
    pub async fn reply_proposing(&self, thread_id: i64, start: i64, end: i64, utc_offset: i32) -> Result<ComposeData> {
        let mut reply = self.reply_compose(thread_id, ReplyMode::ReplyAll, utc_offset).await?;
        (reply.to, reply.cc) = self.jolta.store.read(|c| freebusy::proposal_recipients(c, ThreadId(thread_id)))?;
        let line = availability::proposal_line(at(start), at(end), tz::here());
        reply.body = self.add_to_written(reply.body, reply.account_id, line)?;
        Ok(reply)
    }

    /// The working hours Find a time uses when the user set them, else
    /// None for Outlook's or Monday to Friday, 9 to 5.
    pub fn working_hours(&self) -> Result<Option<WorkingHoursInfo>> {
        Ok(self.jolta.store.read(store::working_hours)?.as_ref().map(hours_info))
    }

    /// Sets the working hours, or with None goes back to Outlook's or the default.
    pub fn set_working_hours(&self, hours: Option<WorkingHoursInfo>) -> Result<()> {
        let hours = hours.as_ref().map(hours_of).transpose()?;
        Ok(self.jolta.store.write(|tx| store::set_working_hours(tx, hours.as_ref()))?)
    }

    /// The default working hours on this Mac's clock, for Settings to start from.
    pub fn standard_working_hours(&self) -> WorkingHoursInfo {
        hours_info(&WorkingHours::standard(tz::here()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_hours_cross_and_come_back_unchanged() {
        let h = WorkingHours { days: Weekdays::of([Weekday::Mon, Weekday::Wed, Weekday::Sun]), ..WorkingHours::standard(chrono_tz::Europe::Brussels) };
        let info = hours_info(&h);
        assert_eq!(info.days, [true, false, true, false, false, false, true]);
        assert_eq!((info.start, info.end, info.zone.as_str()), (540, 1020, "Europe/Brussels"));
        assert_eq!(hours_of(&info).unwrap(), h);
    }

    #[test]
    fn a_persons_hours_shade_their_row_name_it_and_come_back_for_suggestions() {
        use chrono::TimeZone;
        let brussels = WorkingHours::standard(chrono_tz::Europe::Brussels);
        let from = Utc.with_ymd_and_hms(2026, 11, 9, 6, 0, 0).unwrap();
        let windows = brussels.windows(from, 2);
        let ann = WorkingHours { end: NaiveTime::from_hms_opt(15, 0, 0).unwrap(), ..WorkingHours::standard(chrono_tz::America::New_York) };
        let found = Found {
            account: AccountId(1),
            me: "me@example.com".into(),
            own: Vec::new(),
            others: vec![Schedule { hours: Some(ann), ..Schedule::new("ann@example.com", Availability::Known(Vec::new())) }],
            hours: brussels,
            windows,
        };
        let info = info(found, &[Person { address: "ann@example.com".into(), name: "Ann Berg".into() }]);
        let row = &info.people[1];
        assert_eq!(row.note.as_deref(), Some("Works 09:00 to 15:00 in New York"));
        let shaded: Vec<(i64, i64)> = row.outside_hours.iter().map(|s| (s.start, s.end)).collect();
        let utc = |d, h| Utc.with_ymd_and_hms(2026, 11, d, h, 0, 0).unwrap().timestamp();
        assert_eq!(shaded, [(utc(9, 8), utc(9, 14)), (utc(10, 8), utc(10, 14))], "09:00 to 15:00 in Brussels is before Ann starts");
        assert_eq!(info.people[0].note, None, "the user's own row");
        let (_, others) = parts(&info).unwrap();
        assert_eq!(others[0].hours, Some(ann), "the hours come back for suggestions");
    }
}
