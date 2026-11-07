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

/// The scopes through the editor's own path: a draft of one occurrence,
/// saved or deleted for This event, This and following or All events, on
/// a calendar on this Mac. Check 2 of the plan in full: the whole series,
/// one unchanged occurrence, one changed one, and a change to the whole
/// series over a changed occurrence.
#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
    use chrono_tz::Europe::Brussels;

    use super::*;
    use crate::calendar::draft::{self, EventDraft};
    use crate::calendar::ops::{Scope, Undone};
    use crate::calendar::{Access, Bounds, EventId, Zone};
    use crate::store::{RemoteCalendar, Store};

    const NOW: i64 = 1_795_000_000;

    fn horizon() -> Horizon {
        Horizon::around(Utc.with_ymd_and_hms(2026, 11, 7, 12, 0, 0).unwrap(), Brussels)
    }

    fn at(s: &str) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone: Zone::Tz(Brussels) }
    }

    fn timed(start: &str, minutes: i64) -> Span {
        let start = at(start);
        Span::Timed { start, end: Local { wall: start.wall + Duration::minutes(minutes), ..start } }
    }

    /// A weekly Standup of six from Tue 20 Oct 2026 09:00, across the end
    /// of summer time on 25 October, with the 3 November one moved to 14:00.
    fn store(name: &str, source: CalendarSource) -> (Store, CalendarId) {
        let dir = std::env::temp_dir().join(format!("jolta-cal-split-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir.join("jolta.sqlite")).unwrap();
        let cal = match source {
            CalendarSource::Local => s.write(|tx| ops::new_local_calendar(tx, "Test", None)).unwrap().id,
            _ => s
                .write(|tx| store::add_calendar(tx, source, &RemoteCalendar { remote_id: "cal".into(), name: "Work".into(), color: None, zone: None, access: Access::Owner, primary: true, refresh: None }))
                .unwrap()
                .id,
        };
        let series = EventData {
            title: "Standup".into(),
            recurrence: Some(Recurrence { rules: vec!["FREQ=WEEKLY;COUNT=6".into()], ..Recurrence::default() }),
            ..EventData::new("s1", timed("2026-10-20 09:00", 30))
        };
        let moved = EventData { recurrence_id: Some(Moment::Time(at("2026-11-03 09:00"))), recurrence: None, span: timed("2026-11-03 14:00", 30), ..series.clone() };
        s.write(|tx| store::apply_item(tx, cal, "s1.ics", Some("1"), &[series.clone(), moved.clone()], None, &horizon())).unwrap();
        (s, cal)
    }

    /// (start on this Mac's clock, title, which series) of every occurrence.
    fn shown(s: &Store) -> Vec<(String, String, bool)> {
        let day = |m, d| NaiveDate::from_ymd_opt(2026, m, d).unwrap();
        s.read(|c| store::occurrences(c, day(10, 1), day(12, 31), Brussels))
            .unwrap()
            .into_iter()
            .map(|o| match o.bounds {
                Bounds::Utc { start, .. } => (start.with_timezone(&Brussels).format("%d %b %H:%M").to_string(), o.title, o.uid == "s1"),
                Bounds::Days { start, .. } => (format!("day {start}"), o.title, o.uid == "s1"),
            })
            .collect()
    }

    fn rows(list: &[(&str, &str, bool)]) -> Vec<(String, String, bool)> {
        list.iter().map(|(a, b, c)| (a.to_string(), b.to_string(), *c)).collect()
    }

    fn master_id(s: &Store, cal: CalendarId) -> EventId {
        s.read(|c| Ok(c.query_row("SELECT id FROM event WHERE calendar_id = ?1 AND uid = 's1' AND recurrence_id = ''", [cal], |r| r.get(0))?)).unwrap()
    }

    /// The draft the editor opens on the occurrence starting `start`.
    fn opened(s: &Store, cal: CalendarId, start: &str) -> EventDraft {
        let b = timed(start, 30).bounds(Brussels);
        let row = s.read(|c| store::occurrences(c, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 12, 31).unwrap(), Brussels)).unwrap();
        let id = row.iter().find(|o| o.bounds == b).map(|o| o.event_id).unwrap_or_else(|| master_id(s, cal));
        s.read(|c| draft::draft_for_event(c, id, Some(b), Brussels)).unwrap()
    }

    fn save(s: &Store, d: &EventDraft, scope: Scope) -> Option<CalendarOpId> {
        s.write(|tx| draft::save_draft(tx, d, Some(scope), NOW, &horizon(), Brussels)).unwrap().op
    }

    fn scopes(s: &Store, d: &EventDraft) -> Vec<(Scope, Option<String>)> {
        s.read(|c| draft::plan_save(c, d, Brussels)).unwrap().scopes.into_iter().map(|c| (c.scope, c.detail)).collect()
    }

    const BEFORE: [(&str, &str, bool); 6] = [
        ("20 Oct 09:00", "Standup", true),
        ("27 Oct 09:00", "Standup", true),
        ("03 Nov 14:00", "Standup", true),
        ("10 Nov 09:00", "Standup", true),
        ("17 Nov 09:00", "Standup", true),
        ("24 Nov 09:00", "Standup", true),
    ];

    #[test]
    fn this_event_changes_one_unchanged_occurrence_and_undo_takes_it_back() {
        let (s, cal) = store("this", CalendarSource::Local);
        assert_eq!(shown(&s), rows(&BEFORE));
        let series = s.read(|c| ops::stored(c, cal, "s1", None)).unwrap().unwrap().event;
        let mut d = opened(&s, cal, "2026-11-10 09:00");
        assert!(d.locks.is_empty() && d.rule.is_some(), "a series without guests opens for editing with its rule");
        d.span = timed("2026-11-10 11:00", 30);
        assert_eq!(scopes(&s, &d).iter().map(|c| c.0).collect::<Vec<_>>(), [Scope::This, Scope::Following, Scope::All]);
        let op = save(&s, &d, Scope::This).unwrap();
        let mut after = rows(&BEFORE);
        after[3].0 = "10 Nov 11:00".into();
        assert_eq!(shown(&s), after, "only that occurrence moved");
        assert_eq!(s.read(|c| ops::stored(c, cal, "s1", None)).unwrap().unwrap().event, series, "the series row is untouched");

        assert_eq!(s.write(|tx| ops::undo(tx, op, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(shown(&s), rows(&BEFORE));
        assert_eq!(s.read(|c| ops::series(c, cal, "s1")).unwrap().len(), 2, "the series and its one changed occurrence, nothing more");
    }

    #[test]
    fn all_events_reaches_a_changed_occurrence_but_keeps_its_own_time() {
        let (s, cal) = store("all-title", CalendarSource::Local);
        let mut d = opened(&s, cal, "2026-11-17 09:00");
        d.title = "Daily sync".into();
        assert_eq!(scopes(&s, &d).last(), Some(&(Scope::All, None)), "a new title removes nothing");
        save(&s, &d, Scope::All).unwrap();
        let expected: Vec<_> = BEFORE.iter().map(|(t, _, mine)| (*t, "Daily sync", *mine)).collect();
        assert_eq!(shown(&s), rows(&expected), "the moved occurrence takes the new title and keeps 14:00");
    }

    #[test]
    fn all_events_with_a_new_time_says_what_it_removes() {
        let (s, cal) = store("all-time", CalendarSource::Local);
        let mut d = opened(&s, cal, "2026-11-17 09:00");
        d.span = timed("2026-11-17 10:00", 30);
        assert_eq!(scopes(&s, &d).last(), Some(&(Scope::All, Some("Removes the changes made to 1 single event.".into()))));
        let op = save(&s, &d, Scope::All).unwrap();
        let moved: Vec<_> = BEFORE.iter().map(|(t, n, m)| (format!("{}10:00", &t[..7]), n.to_string(), *m)).collect();
        assert_eq!(shown(&s), moved, "every occurrence an hour later, on the wall clock across the clock change; the moved one's change is gone");
        assert!(s.read(|c| store::calendar_op(c, op)).unwrap().send_after > NOW, "a change that removes occurrences waits the undo window");
        assert_eq!(s.write(|tx| ops::undo(tx, op, NOW, &horizon())).unwrap(), Undone::Dropped);
        assert_eq!(shown(&s), rows(&BEFORE), "and undo brings the changed occurrence back");
    }

    #[test]
    fn this_and_following_splits_without_losing_or_doubling_an_occurrence() {
        let (s, cal) = store("following", CalendarSource::Local);
        let mut d = opened(&s, cal, "2026-10-27 09:00");
        d.span = timed("2026-10-27 10:00", 30);
        assert_eq!(scopes(&s, &d)[1], (Scope::Following, Some("Starts a new series from Tue 27 Oct.".into())));
        let op = save(&s, &d, Scope::Following).unwrap();
        assert_eq!(
            shown(&s),
            rows(&[
                ("20 Oct 09:00", "Standup", true),
                ("27 Oct 10:00", "Standup", false),
                ("03 Nov 14:00", "Standup", false),
                ("10 Nov 10:00", "Standup", false),
                ("17 Nov 10:00", "Standup", false),
                ("24 Nov 10:00", "Standup", false),
            ]),
            "one before the split in the old series, five after it in the new one, the moved one still at 14:00"
        );
        let old = s.read(|c| ops::stored(c, cal, "s1", None)).unwrap().unwrap().event;
        assert_eq!(old.recurrence.unwrap().rules, ["FREQ=WEEKLY;UNTIL=20261026T225959Z"], "the old series ends the day before, in UTC");
        let pair = s.read(|c| store::calendar_op(c, op)).unwrap();
        let first = s.read(|c| store::calendar_op(c, pair.after.unwrap())).unwrap();
        assert_eq!((pair.scope, first.write.kind()), (Scope::Following, "create"), "the old series waits for the new one");
        let new_rule = first.write.event().unwrap().recurrence.clone().unwrap();
        assert_eq!(new_rule.rules, ["FREQ=WEEKLY;COUNT=5"], "COUNT less the occurrence before the split");
        let claimed = s.write(|tx| store::claim_calendar_ops(tx, NOW + 3600, 4)).unwrap();
        assert_eq!(claimed.iter().map(|o| o.id).collect::<Vec<_>>(), [first.id], "the shortening waits until the create is done");
        s.write(|tx| store::release_calendar_claims(tx)).unwrap();
        s.write(|tx| ops::undo(tx, op, NOW, &horizon())).unwrap();
        assert_eq!(shown(&s), rows(&BEFORE), "undo takes back both");
        assert!(s.read(|c| store::calendar_ops(c, 10)).unwrap().iter().all(|o| o.id != op), "the pair is dropped");
    }

    #[test]
    fn a_failed_create_leaves_the_old_series_whole() {
        let (s, cal) = store("following-fails", CalendarSource::Local);
        let mut d = opened(&s, cal, "2026-11-10 09:00");
        d.title = "Planning".into();
        let op = save(&s, &d, Scope::Following).unwrap();
        let pair = s.read(|c| store::calendar_op(c, op)).unwrap();
        let create = s.read(|c| store::calendar_op(c, pair.after.unwrap())).unwrap();
        for _ in 0..ops::MAX_ATTEMPTS {
            let claimed = s.write(|tx| store::claim_calendar_ops(tx, NOW + 100_000, 4)).unwrap();
            assert_eq!(claimed.iter().map(|o| o.id).collect::<Vec<_>>(), [create.id]);
            s.write(|tx| ops::settle(tx, &claimed[0], Err("offline".into()), NOW, &horizon())).unwrap();
        }
        assert_eq!(s.read(|c| store::calendar_op(c, op)).unwrap().state, ops::OpState::Failed);
        assert_eq!(shown(&s), rows(&BEFORE), "nothing lost and nothing twice");
    }

    #[test]
    fn deleting_one_occurrence_takes_out_that_one_alone() {
        let (s, cal) = store("delete-one", CalendarSource::Local);
        let del = |d: &EventDraft, scope| s.write(|tx| draft::delete_drafted(tx, d, Some(scope), NOW, &horizon(), Brussels)).unwrap();
        let unchanged = opened(&s, cal, "2026-11-10 09:00");
        let plan = s.read(|c| draft::plan_delete(c, master_id(&s, cal), Some(timed("2026-11-10 09:00", 30).bounds(Brussels)), Brussels)).unwrap();
        assert_eq!(plan.scopes.iter().map(|c| c.scope).collect::<Vec<_>>(), [Scope::This, Scope::Following, Scope::All]);
        let first = del(&unchanged, Scope::This);
        assert_eq!(first.message, "Deleted Standup on Tue 10 Nov");
        let mut after = rows(&BEFORE);
        after.remove(3);
        assert_eq!(shown(&s), after);
        let changed = opened(&s, cal, "2026-11-03 14:00");
        assert!(changed.base.as_ref().unwrap().recurrence_id.is_some());
        del(&changed, Scope::This);
        after.remove(2);
        assert_eq!(shown(&s), after, "a changed occurrence goes with its row");
        let master = s.read(|c| ops::stored(c, cal, "s1", None)).unwrap().unwrap().event;
        assert_eq!(master.recurrence.unwrap().exdates.len(), 2, "both as EXDATEs on the series");

        let undone = s.write(|tx| ops::undo(tx, first.op.unwrap(), NOW, &horizon()));
        assert!(undone.is_err(), "only the newest op of the event can be undone");
        let ops = s.read(|c| store::calendar_ops(c, 10)).unwrap();
        s.write(|tx| ops::undo(tx, ops[0].id, NOW, &horizon())).unwrap();
        s.write(|tx| ops::undo(tx, first.op.unwrap(), NOW, &horizon())).unwrap();
        assert_eq!(shown(&s), rows(&BEFORE), "both come back, the changed one at its own time");

        let from_nov = opened(&s, cal, "2026-11-17 09:00");
        del(&from_nov, Scope::Following);
        assert_eq!(shown(&s), rows(&BEFORE[..4]), "this and following ends the series before it");
    }

    #[test]
    fn deleting_a_sent_occurrence_delete_says_it_cant_be_undone() {
        let (s, cal) = store("delete-sent", CalendarSource::Local);
        let d = opened(&s, cal, "2026-11-10 09:00");
        let op = s.write(|tx| draft::delete_drafted(tx, &d, Some(Scope::This), NOW, &horizon(), Brussels)).unwrap().op.unwrap();
        let claimed = s.write(|tx| store::claim_calendar_ops(tx, NOW + 60, 4)).unwrap();
        s.write(|tx| ops::settle(tx, &claimed[0], Ok(ops::Sent::Done { version: None, merged: false }), NOW, &horizon())).unwrap();
        let err = s.write(|tx| ops::undo(tx, op, NOW, &horizon())).unwrap_err().to_string();
        assert!(err.contains("can't be taken back"), "{err}");
    }

    #[test]
    fn the_prompt_offers_what_fits_the_occurrence() {
        let (s, cal) = store("prompt", CalendarSource::Local);
        let mut first = opened(&s, cal, "2026-10-20 09:00");
        first.title = "Renamed".into();
        assert_eq!(scopes(&s, &first).iter().map(|c| c.0).collect::<Vec<_>>(), [Scope::This, Scope::All], "no split on the first occurrence");
        let mut d = opened(&s, cal, "2026-11-10 09:00");
        d.rule = Some(Recurrence { rules: vec!["FREQ=WEEKLY;BYDAY=TU,TH".into()], ..Recurrence::default() });
        assert_eq!(scopes(&s, &d).iter().map(|c| c.0).collect::<Vec<_>>(), [Scope::Following, Scope::All], "a new rule is never for one event");
        assert!(s.write(|tx| draft::save_draft(tx, &d, None, NOW, &horizon(), Brussels)).is_err(), "a series occurrence needs its scope");
        let unchanged = opened(&s, cal, "2026-11-10 09:00");
        assert!(s.read(|c| draft::plan_save(c, &unchanged, Brussels)).unwrap().scopes.is_empty(), "nothing changed, nothing to ask");
    }

    /// Microsoft 365 refuses an occurrence moved onto or past its neighbour
    /// (`ErrorOccurrenceCrossingBoundary`), and drops every changed
    /// occurrence when the series' time changes.
    #[test]
    fn microsoft_365_limits_are_said_before_saving() {
        let (s, cal) = store("graph", CalendarSource::Graph);
        let mut d = opened(&s, cal, "2026-11-10 09:00");
        d.span = timed("2026-11-17 11:00", 30);
        let problems = s.read(|c| draft::check_draft(c, &d, Brussels)).unwrap();
        assert!(
            problems.iter().any(|p| p.blocking && p.text == "Microsoft 365 can't move this occurrence onto or past the next one (Tue 17 Nov). Change the series instead."),
            "{problems:?}"
        );
        d.span = timed("2026-11-03 18:00", 30);
        assert!(s.read(|c| draft::check_draft(c, &d, Brussels)).unwrap().iter().any(|p| p.text.contains("the one before it (Tue 3 Nov)")));
        d.span = timed("2026-11-11 09:00", 30);
        assert!(s.read(|c| draft::check_draft(c, &d, Brussels)).unwrap().iter().all(|p| !p.text.contains("can't move")), "a day later is fine");
        let mut all = opened(&s, cal, "2026-11-17 09:00");
        all.span = timed("2026-11-17 09:15", 30);
        assert_eq!(scopes(&s, &all).last().unwrap().1.as_deref(), Some("Removes the changes made to 1 single event."));
        let mut title = opened(&s, cal, "2026-11-17 09:00");
        title.title = "Renamed".into();
        assert_eq!(scopes(&s, &title).last().unwrap().1, None, "a new title keeps them on Microsoft 365");
    }
}
