//! This and following (C3 spec, section 10, U4): the series ends the day
//! before the occurrence the user changed, and a new series with a new UID
//! starts there as the user saved it. Its changed occurrences after that
//! day move to the new series with the user's change applied where they
//! didn't change the same field themselves; the ones taken out stay out.
//!
//! The new series is created first and the old one shortened only once the
//! new one exists (`ops::Link`), so a failure can leave both for a moment
//! but never neither. Google and Microsoft 365 drop the old series' changed
//! occurrences past its new end themselves; CalDAV loses them from the
//! resource with the same write.

use chrono::Duration;
use chrono_tz::Tz;
use rusqlite::Connection;

use super::ops::{self, CalendarOpId, Change, Edit, Extra, Link};
use super::series::{self, Horizon};
use super::{CalendarId, CalendarSource, EventData, EventStatus, Local, Moment, Recurrence, Span, merge, recur, repeat, tz};
use crate::{Error, Result, store};

/// Queues This and following for the occurrence `edited` names by its
/// recurrence id, saved as `edited`, the new series repeating by `rule`.
/// Returns the op that shortens the old series, Undo's token for both.
pub fn queue_following(conn: &Connection, calendar: CalendarId, edited: &EventData, rule: &Recurrence, now: i64, horizon: &Horizon) -> Result<Option<CalendarOpId>> {
    let here = horizon.here;
    let cal = store::calendar(conn, calendar)?;
    let at = edited.recurrence_id.ok_or_else(|| Error::Other("This and following needs an occurrence".into()))?;
    let master = ops::stored(conn, calendar, &edited.uid, None)?.ok_or_else(|| Error::NotFound(format!("event {} in {}", edited.uid, cal.name)))?.event;
    let old_rule = master.recurrence.clone().ok_or_else(|| Error::Other(format!("{} doesn't repeat", master.title)))?;
    if !ops::is_occurrence(&master, &at, here) {
        return Err(Error::NotFound(format!("that occurrence of {}", master.title)));
    }
    if master.span.start().names(&at, here) {
        return Err(Error::Other("The first occurrence starts the series: change all events instead".into()));
    }
    let at_span = series::occurrence_span(&master.span, &at, here).ok_or_else(|| Error::Other("An all-day series has no occurrence at a time".into()))?;
    let shift = Shift::between(&at_span, &edited.span);

    let rows: Vec<EventData> = ops::series(conn, calendar, &edited.uid)?;
    let later = |e: &EventData| e.recurrence_id.as_ref().is_some_and(|rid| !before(rid, &at, here));

    let new_uid = ops::new_uid()?;
    let mut new_rule = Recurrence { exdates: Vec::new(), ..rule.clone() };
    if rule.rules == old_rule.rules {
        new_rule.rules = old_rule.rules.iter().map(|r| remaining(r, &master, &at, here)).collect::<Result<_>>()?;
    }
    let taken_out = old_rule.exdates.iter().filter(|x| !before(x, &at, here)).copied();
    let cancelled = rows.iter().filter(|e| later(e) && e.status == EventStatus::Cancelled).filter_map(|e| e.recurrence_id);
    new_rule.exdates = taken_out.chain(cancelled).filter_map(|m| shift.moment(&m, here)).collect();
    let new_master = EventData { uid: new_uid.clone(), recurrence_id: None, recurrence: Some(new_rule), sequence: 0, master_id: None, ..edited.clone() };

    let mut events = vec![new_master.clone()];
    for theirs in rows.iter().filter(|e| later(e) && e.status != EventStatus::Cancelled && !e.recurrence_id.is_some_and(|r| r.names(&at, here))) {
        let rid = theirs.recurrence_id.expect("a changed occurrence");
        let Some(was) = series::occurrence(std::slice::from_ref(&master), &rid, here) else { continue };
        let Some(new_rid) = shift.moment(&rid, here) else { continue };
        let Some(now_there) = series::occurrence(std::slice::from_ref(&new_master), &new_rid, here) else { continue };
        let mine = EventData { uid: edited.uid.clone(), recurrence_id: Some(rid), ..now_there };
        let merged = merge::three_way(&was, &mine, theirs).event;
        events.push(EventData { uid: new_uid.clone(), recurrence_id: Some(new_rid), recurrence: None, master_id: None, ..merged });
    }

    let ended = ended(&master, &at, here);

    let extra = (cal.source == CalendarSource::CalDav).then(|| Extra::Lines(vec![format!("RELATED-TO:{}", edited.uid)]));
    let create = Edit { extra, ..Edit::quiet(calendar, Change::Create(events)) };
    let first = ops::queue_op(conn, &create, Link { after: None, following: true }, now, horizon)?;
    ops::queue_op(conn, &Edit::quiet(calendar, Change::Update(ended)), Link { after: first, following: true }, now, horizon)
}

/// The series `master` ended with the occurrence before `at`: for This
/// and following, and for deleting an occurrence and the ones after it.
pub fn ended(master: &EventData, at: &Moment, here: Tz) -> EventData {
    let mut ended = master.clone();
    if let Some(rule) = &master.recurrence {
        ended.recurrence = Some(Recurrence {
            rules: rule.rules.iter().map(|r| ending(r, &master.span, at, here)).collect(),
            rdates: rule.rdates.iter().filter(|m| before(m, at, here)).copied().collect(),
            exdates: rule.exdates.iter().filter(|m| before(m, at, here)).copied().collect(),
        });
    }
    ended
}

/// Whether `m` comes before the occurrence `at`, as moments.
fn before(m: &Moment, at: &Moment, here: Tz) -> bool {
    let instant = |m: &Moment| match m {
        Moment::Time(t) => t.instant(here),
        Moment::Date(d) => tz::instant(d.and_time(chrono::NaiveTime::MIN), here),
    };
    instant(m) < instant(at) && !m.names(at, here)
}

/// `rule` with its end replaced so the last occurrence is the one before
/// `at`: the end of the day before for a rule daily or slower, which is
/// what Microsoft 365's range end can say, else the second before.
fn ending(rule: &str, span: &Span, at: &Moment, here: Tz) -> String {
    let sub_daily = rule.split(';').any(|p| matches!(p, "FREQ=HOURLY" | "FREQ=MINUTELY" | "FREQ=SECONDLY"));
    let until = match (at, sub_daily) {
        (Moment::Time(t), true) => (t.instant(here) - Duration::seconds(1)).format("%Y%m%dT%H%M%SZ").to_string(),
        (Moment::Time(t), false) => repeat::until(t.wall.date() - Duration::days(1), span),
        (Moment::Date(d), _) => repeat::until(*d - Duration::days(1), span),
    };
    let kept = rule.split(';').filter(|p| !p.is_empty() && !p.starts_with("UNTIL=") && !p.starts_with("COUNT="));
    kept.map(str::to_string).chain(std::iter::once(format!("UNTIL={until}"))).collect::<Vec<_>>().join(";")
}

/// `rule` for the new series from `at` on: a COUNT less the occurrences
/// before `at`, RFC 5545 counting those taken out too.
fn remaining(rule: &str, master: &EventData, at: &Moment, here: Tz) -> Result<String> {
    let Some(count) = rule.split(';').find_map(|p| p.strip_prefix("COUNT=")).and_then(|n| n.parse::<usize>().ok()) else { return Ok(rule.to_string()) };
    let single = EventData { recurrence: Some(Recurrence { rules: vec![rule.to_string()], ..Recurrence::default() }), ..master.clone() };
    let start = master.span.bounds(here);
    let from = match start {
        super::Bounds::Utc { start, .. } => start,
        super::Bounds::Days { start, .. } => tz::instant(chrono::NaiveDate::default().and_time(chrono::NaiveTime::MIN) + Duration::days(start.into()), here),
    } - Duration::days(1);
    let until = match at {
        Moment::Time(t) => t.instant(here),
        Moment::Date(d) => tz::instant(d.and_time(chrono::NaiveTime::MIN), here),
    };
    let done = recur::expand(&single, &[], recur::Window { start: from, end: until }, here).iter().filter(|o| before(&o.span.start(), at, here)).count();
    let left = count.checked_sub(done).filter(|n| *n > 0).ok_or_else(|| Error::Other("No occurrences are left after this one".into()))?;
    Ok(rule.split(';').map(|p| if p.starts_with("COUNT=") { format!("COUNT={left}") } else { p.to_string() }).collect::<Vec<_>>().join(";"))
}

/// How the new series' clock stands to the old one's: the occurrence at
/// the split moved from `from` to `to`, and every later one with it.
enum Shift {
    Wall(chrono::TimeDelta, super::Zone),
    Days(chrono::TimeDelta),
}

impl Shift {
    fn between(from: &Span, to: &Span) -> Shift {
        match (from, to) {
            (Span::Timed { start: a, .. }, Span::Timed { start: b, .. }) => Shift::Wall(b.wall - a.wall, b.zone),
            (Span::AllDay { start: a, .. }, Span::AllDay { start: b, .. }) => Shift::Days(*b - *a),
            (Span::Timed { start: a, .. }, Span::AllDay { start: b, .. }) => Shift::Days(*b - a.wall.date()),
            (Span::AllDay { start: a, .. }, Span::Timed { start: b, .. }) => Shift::Wall(b.wall - a.and_time(chrono::NaiveTime::MIN), b.zone),
        }
    }

    /// The old series' moment `m` as the new series has it.
    fn moment(&self, m: &Moment, here: Tz) -> Option<Moment> {
        Some(match (self, m) {
            (Shift::Wall(by, zone), Moment::Time(t)) => {
                let wall = tz::wall(t.instant(here), t.zone, here) + *by;
                Moment::Time(Local { wall, zone: *zone })
            }
            (Shift::Wall(by, zone), Moment::Date(d)) => Moment::Time(Local { wall: d.and_time(chrono::NaiveTime::MIN) + *by, zone: *zone }),
            (Shift::Days(by), Moment::Date(d)) => Moment::Date(*d + *by),
            (Shift::Days(by), Moment::Time(t)) => Moment::Date(t.wall.date() + *by),
        })
    }
}
