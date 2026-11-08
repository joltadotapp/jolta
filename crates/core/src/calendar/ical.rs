//! iCalendar (RFC 5545): read events from CalDAV resources, webcal feeds and
//! mail invitations. `ical_edit` writes them back.

use std::collections::HashMap;
use std::str::FromStr;

use calcard::common::timezone::Tz as CalTz;
use calcard::common::PartialDateTime;
use calcard::icalendar::timezone::TzResolver;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType as Kind, ICalendarEntry,
    ICalendarParameterName as Param, ICalendarProperty as Prop, ICalendarValue as Value,
    ICalendarWeekday,
};
use calcard::{Entry, Parser};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Weekday};
use chrono_tz::Tz;

use super::meeting::{self, Entry as Conference, EntryKind};
use super::*;
use crate::{Error, Result};

/// Every VEVENT in `text`. `me` lists the user's own addresses, which mark
/// the attendee whose answer is the user's. VTODO and other components are
/// skipped, and so are events without a UID or a readable DTSTART.
pub fn read(text: &str, me: &[String]) -> Result<Vec<EventData>> {
    read_with(text, me, meeting::detect)
}

/// One VEVENT of an invitation in mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub event: EventData,
    /// False when the VEVENT has no start. An iTIP message may name its
    /// event by UID alone: RFC 5546 leaves DTSTART out of REFRESH and lets
    /// CANCEL, REPLY and DECLINECOUNTER omit it. The event's span is then a
    /// stand-in that says nothing about its time.
    pub dated: bool,
    /// DTSTAMP: when the organizer sent this version. It orders two
    /// versions with the same SEQUENCE.
    pub stamp: Option<DateTime<chrono::Utc>>,
    /// False when the organizer takes no proposals for a new time:
    /// Outlook's `X-MICROSOFT-DISALLOW-COUNTER:TRUE`.
    pub proposals: bool,
}

/// Every VEVENT in an invitation in mail.
pub fn read_invitation(text: &str, me: &[String]) -> Result<Vec<Sent>> {
    read_all(text, me, meeting::detect)
}

/// The VCALENDAR's METHOD (`REQUEST`, `REPLY`, `CANCEL`, `COUNTER`, …), as
/// an invitation in mail carries it.
pub fn method(text: &str) -> Option<String> {
    calendars(text).iter().find_map(calendar_method)
}

/// The VCALENDAR's PRODID: who wrote it, such as
/// `-//Google Inc//Google Calendar 70.9054//EN`.
pub fn producer(text: &str) -> Option<String> {
    calendars(text).iter().find_map(|cal| prop_text(cal.components.first()?, &Prop::Prodid))
}

type Detect = fn(&[Conference], Option<&str>, &str, &str) -> Option<Meeting>;

pub(super) fn read_with(text: &str, me: &[String], detect: Detect) -> Result<Vec<EventData>> {
    Ok(read_all(text, me, detect)?.into_iter().filter_map(|s| s.dated.then_some(s.event)).collect())
}

fn read_all(text: &str, me: &[String], detect: Detect) -> Result<Vec<Sent>> {
    let calendars = calendars(text);
    if calendars.is_empty() {
        return Err(Error::Other("no iCalendar data found".into()));
    }
    let mut events = Vec::new();
    for cal in &calendars {
        let method = calendar_method(cal);
        let mut zones = Zones::new(cal);
        for comp in cal.components.iter().filter(|c| c.component_type == Kind::VEvent) {
            if let Some(event) = event(cal, comp, me, method.as_deref(), &mut zones, detect) {
                events.push(event);
            }
        }
    }
    Ok(events)
}

fn calendars(text: &str) -> Vec<ICalendar> {
    let (calendars, complete) = parse(text);
    if complete {
        return calendars;
    }
    // calcard drops a component that never ends, and with it the whole
    // calendar; closing the open components first keeps what was there.
    match super::ical_edit::Object::parse(text).closed() {
        Some(fixed) => {
            let (repaired, _) = parse(&fixed);
            if repaired.is_empty() { calendars } else { repaired }
        }
        None => calendars,
    }
}

fn parse(text: &str) -> (Vec<ICalendar>, bool) {
    let mut parser = Parser::new(text);
    let mut calendars = Vec::new();
    let mut complete = true;
    // Every entry consumes input; the bound only guards against a parser bug
    // turning malformed input into a hang.
    for _ in 0..=text.len() {
        match parser.entry() {
            Entry::ICalendar(cal) => calendars.push(cal),
            Entry::Eof | Entry::TooManyComponents => break,
            Entry::UnterminatedComponent(_) | Entry::UnexpectedComponentEnd { .. } => {
                complete = false
            }
            _ => {}
        }
    }
    (calendars, complete)
}

fn calendar_method(cal: &ICalendar) -> Option<String> {
    let root = cal.components.first()?;
    let entry = root.property(&Prop::Method)?;
    Some(text(entry)?.trim().to_ascii_uppercase()).filter(|m| !m.is_empty())
}

fn event(
    cal: &ICalendar,
    comp: &ICalendarComponent,
    me: &[String],
    method: Option<&str>,
    zones: &mut Zones<'_>,
    detect: Detect,
) -> Option<Sent> {
    let uid = comp.property(&Prop::Uid).and_then(text).filter(|u| !u.is_empty())?;
    let start = comp.property(&Prop::Dtstart).and_then(|e| first_moment(e, zones));
    let span = match start {
        Some(start) => span(comp, start, zones),
        None => Span::AllDay { start: NaiveDate::default(), end: NaiveDate::default() + Duration::days(1) },
    };

    let rid = comp.property(&Prop::RecurrenceId);
    let recurrence_id = rid.and_then(|e| first_moment(e, zones));
    // calcard keeps RANGE only when it is THISANDFUTURE.
    let this_and_future = rid.is_some_and(|e| e.has_parameter(&Param::Range));

    let recurrence = recurrence(comp, zones);
    let title = prop_text(comp, &Prop::Summary).unwrap_or_default();
    let location = prop_text(comp, &Prop::Location).unwrap_or_default();
    let plain = prop_text(comp, &Prop::Description).unwrap_or_default();
    let html = x_props(comp, "X-ALT-DESC")
        .filter(|e| param(e, &Param::Fmttype).is_some_and(|f| f.eq_ignore_ascii_case("text/html")))
        .find_map(|e| text(e).filter(|t| !t.trim().is_empty()));
    let url = comp.property(&Prop::Url).and_then(text).filter(|u| !u.is_empty());

    let status = match comp.property(&Prop::Status).and_then(upper).as_deref() {
        Some("TENTATIVE") => EventStatus::Tentative,
        Some("CANCELLED") => EventStatus::Cancelled,
        _ => EventStatus::Confirmed,
    };
    let transparent = comp.property(&Prop::Transp).and_then(upper).as_deref() == Some("TRANSPARENT");
    let microsoft = x_props(comp, "X-MICROSOFT-CDO-BUSYSTATUS").find_map(upper);
    let show_as = match microsoft.as_deref() {
        Some("FREE") => ShowAs::Free,
        Some("TENTATIVE") => ShowAs::Tentative,
        Some("BUSY") => ShowAs::Busy,
        Some("OOF") => ShowAs::OutOfOffice,
        Some("WORKINGELSEWHERE") => ShowAs::Elsewhere,
        _ if transparent => ShowAs::Free,
        _ => ShowAs::Busy,
    };
    let visibility = match comp.property(&Prop::Class).and_then(upper).as_deref() {
        Some("PUBLIC") => Visibility::Public,
        Some("PRIVATE") => Visibility::Private,
        Some("CONFIDENTIAL") => Visibility::Confidential,
        _ => Visibility::Default,
    };

    let organizer = comp.property(&Prop::Organizer).and_then(person);
    // A reply or a proposal carries its one guest's note as COMMENT.
    let reply_comment = matches!(method, Some("REPLY" | "COUNTER")).then(|| prop_text(comp, &Prop::Comment)).flatten();
    let mut attendees: Vec<Attendee> =
        comp.properties(&Prop::Attendee).filter_map(|e| attendee(e, me)).collect();
    if let ([only], Some(comment)) = (attendees.as_mut_slice(), reply_comment)
        && only.comment.is_empty()
    {
        only.comment = comment;
    }

    let sequence = comp
        .property(&Prop::Sequence)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_integer().or_else(|| v.as_text()?.trim().parse().ok()))
        .unwrap_or(0);
    let alarms = comp
        .component_ids
        .iter()
        .filter_map(|id| cal.components.get(*id as usize))
        .filter(|c| c.component_type == Kind::VAlarm)
        .filter_map(|c| alarm(c, &span))
        .collect();
    let categories = comp
        .properties(&Prop::Categories)
        .flat_map(|e| e.values.iter().filter_map(Value::as_text))
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let color = prop_text(comp, &Prop::Color).filter(|c| !c.is_empty());

    let entries = conference_entries(comp);
    let meeting = detect(
        &entries,
        url.as_deref(),
        &location,
        if plain.is_empty() { html.as_deref().unwrap_or("") } else { &plain },
    );
    let (description, description_html) = match html {
        Some(html) => (html, true),
        None => (plain, false),
    };

    let proposals = !x_props(comp, "X-MICROSOFT-DISALLOW-COUNTER").filter_map(upper).any(|v| v == "TRUE");
    // RSVP=FALSE is the default, so only a meeting that says it of every
    // guest and asks none counts as not asking for answers.
    let rsvp: Vec<Option<bool>> = comp
        .properties(&Prop::Attendee)
        .filter(|e| !matches!(param_upper(e, &Param::Cutype).as_deref(), Some("ROOM" | "RESOURCE")))
        .map(|e| match e.parameter(&Param::Rsvp) {
            Some(calcard::icalendar::ICalendarParameterValue::Bool(b)) => Some(*b),
            Some(v) => v.as_text().map(|t| t.trim().eq_ignore_ascii_case("TRUE")),
            None => None,
        })
        .collect();
    let request_responses = !(rsvp.contains(&Some(false)) && !rsvp.contains(&Some(true)));
    let options = MeetingOptions { request_responses, allow_new_times: proposals, ..MeetingOptions::default() };
    let organizer_is_me = organizer.as_ref().is_some_and(|o| me.iter().any(|m| m.eq_ignore_ascii_case(&o.address)));

    let event = EventData {
        uid,
        recurrence_id,
        this_and_future,
        span,
        recurrence,
        title,
        location,
        description,
        description_html,
        url,
        status,
        show_as,
        visibility,
        organizer,
        attendees,
        sequence,
        alarms,
        meeting,
        categories,
        color,
        master_id: None,
        organizer_is_me,
        options,
    };
    let stamp = comp.property(&Prop::Dtstamp).and_then(|e| match e.values.first() {
        Some(Value::PartialDateTime(dt)) => utc(dt),
        _ => None,
    });
    Some(Sent { event, dated: start.is_some(), stamp, proposals })
}

fn span(comp: &ICalendarComponent, start: Moment, zones: &mut Zones<'_>) -> Span {
    let end = comp.property(&Prop::Dtend).and_then(|e| first_moment(e, zones));
    let duration = comp.property(&Prop::Duration).and_then(|e| match e.values.first() {
        Some(Value::Duration(d)) if !d.neg => Some(Duration::seconds(d.as_seconds())),
        _ => None,
    });
    match start {
        Moment::Date(start) => {
            let end = match (end, duration) {
                (Some(Moment::Date(end)), _) => end,
                (Some(Moment::Time(end)), _) => end.wall.date(),
                (None, Some(d)) => start + Duration::days((d.num_seconds() + 86_399) / 86_400),
                (None, None) => start,
            };
            Span::AllDay { start, end: end.max(start + Duration::days(1)) }
        }
        Moment::Time(start) => {
            let end = match (end, duration) {
                (Some(Moment::Time(end)), _) => end,
                (Some(Moment::Date(end)), _) => Local { wall: end.into(), zone: start.zone },
                // Nominal: the duration is added to the wall clock, so a
                // PT1H event that crosses a clock change keeps its wall length.
                (None, Some(d)) => Local { wall: start.wall + d, zone: start.zone },
                (None, None) => start,
            };
            let end = if end.zone == start.zone && end.wall < start.wall { start } else { end };
            Span::Timed { start, end }
        }
    }
}

fn recurrence(comp: &ICalendarComponent, zones: &mut Zones<'_>) -> Option<Recurrence> {
    let rules: Vec<String> = comp
        .properties(&Prop::Rrule)
        .flat_map(|e| &e.values)
        .filter_map(|v| match v {
            Value::RecurrenceRule(rule) => Some(rule.to_string()),
            Value::Text(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
            _ => None,
        })
        .collect();
    let mut moments = |prop: Prop| -> Vec<Moment> {
        comp.properties(&prop)
            .flat_map(|e| e.values.iter().map(move |v| (e, v)))
            .filter_map(|(e, v)| moment(e, v, zones))
            .collect()
    };
    let rdates = moments(Prop::Rdate);
    let exdates = moments(Prop::Exdate);
    let r = Recurrence { rules, rdates, exdates };
    (r != Recurrence::default()).then_some(r)
}

fn attendee(e: &ICalendarEntry, me: &[String]) -> Option<Attendee> {
    let person = person(e)?;
    let role = match (param_upper(e, &Param::Cutype).as_deref(), param_upper(e, &Param::Role).as_deref()) {
        (Some("ROOM"), _) => Role::Room,
        (Some("RESOURCE"), _) => Role::Resource,
        (_, Some("CHAIR")) => Role::Chair,
        (_, Some("OPT-PARTICIPANT")) => Role::Optional,
        (_, Some("NON-PARTICIPANT")) => Role::Fyi,
        _ => Role::Required,
    };
    let response = match param_upper(e, &Param::Partstat).as_deref() {
        Some("ACCEPTED") => Response::Accepted,
        Some("TENTATIVE") => Response::Tentative,
        Some("DECLINED") => Response::Declined,
        Some("DELEGATED") => Response::Delegated,
        _ => Response::NeedsAction,
    };
    let comment = e
        .params
        .iter()
        .find(|p| matches!(&p.name, Param::Other(n) if n.eq_ignore_ascii_case("X-RESPONSE-COMMENT")))
        .and_then(|p| p.value.as_text())
        .unwrap_or_default()
        .to_string();
    let is_me = me.iter().any(|m| m.eq_ignore_ascii_case(&person.address));
    Some(Attendee { person, role, response, comment, is_me })
}

/// An ORGANIZER or ATTENDEE. Apple sometimes gives a `urn:uuid:` address
/// with the mail address in the EMAIL parameter.
fn person(e: &ICalendarEntry) -> Option<Person> {
    let value = text(e)?;
    let address = match strip_mailto(value.trim()) {
        Some(a) => a.to_string(),
        None => param(e, &Param::Email).map(str::to_string).unwrap_or(value),
    };
    if address.is_empty() {
        return None;
    }
    let name = param(e, &Param::Cn).filter(|n| !n.eq_ignore_ascii_case(&address)).unwrap_or("");
    Some(Person { address, name: name.to_string() })
}

pub(super) fn strip_mailto(value: &str) -> Option<&str> {
    let head = value.get(..7)?;
    head.eq_ignore_ascii_case("mailto:").then(|| value[7..].trim())
}

fn alarm(comp: &ICalendarComponent, span: &Span) -> Option<Alarm> {
    let action = comp.property(&Prop::Action).and_then(upper);
    // Apple marks "no alert" with an ACTION:NONE alarm in 1976.
    if action.as_deref() == Some("NONE") {
        return None;
    }
    let entry = comp.property(&Prop::Trigger)?;
    let trigger = match entry.values.first()? {
        Value::Duration(d) => {
            let mut seconds = d.as_seconds();
            if param_upper(entry, &Param::Related).as_deref() == Some("END") {
                seconds += span_seconds(span);
            }
            AlarmTrigger::MinutesBefore(-seconds.div_euclid(60))
        }
        Value::PartialDateTime(dt) => AlarmTrigger::At(utc(dt)?),
        _ => return None,
    };
    Some(Alarm { trigger, email: action.as_deref() == Some("EMAIL") })
}

fn span_seconds(span: &Span) -> i64 {
    match span {
        Span::AllDay { start, end } => (*end - *start).num_seconds(),
        Span::Timed { start, end } if start.zone == end.zone => (end.wall - start.wall).num_seconds(),
        Span::Timed { .. } => 0,
    }
}

fn utc(dt: &PartialDateTime) -> Option<DateTime<chrono::Utc>> {
    let wall = wall(dt)?;
    Some((wall - Duration::seconds(offset_seconds(dt))).and_utc())
}

/// The structured join links that `meeting::detect` looks at first.
fn conference_entries(comp: &ICalendarComponent) -> Vec<Conference> {
    let mut entries = Vec::new();
    for e in &comp.entries {
        let Some(uri) = text(e).map(|u| u.trim().to_string()).filter(|u| !u.is_empty()) else {
            continue;
        };
        let kind = match &e.name {
            Prop::Conference => {
                let features: Vec<String> =
                    e.parameters(&Param::Feature).filter_map(|v| v.as_text()).map(str::to_ascii_uppercase).collect();
                let phone = uri.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("tel:"))
                    || (features.iter().any(|f| f == "PHONE") && !features.iter().any(|f| f == "VIDEO"));
                if phone { EntryKind::Phone } else { EntryKind::Video }
            }
            Prop::Other(n)
                if n.eq_ignore_ascii_case("X-MICROSOFT-SKYPETEAMSMEETINGURL") || n.eq_ignore_ascii_case("X-GOOGLE-CONFERENCE") =>
            {
                EntryKind::Video
            }
            _ => continue,
        };
        entries.push(Conference { kind, uri, meeting_id: None, passcode: None });
    }
    entries
}

fn first_moment(e: &ICalendarEntry, zones: &mut Zones<'_>) -> Option<Moment> {
    moment(e, e.values.first()?, zones)
}

fn moment(e: &ICalendarEntry, value: &Value, zones: &mut Zones<'_>) -> Option<Moment> {
    let dt = match value {
        Value::PartialDateTime(dt) => dt.as_ref(),
        Value::Period(calcard::icalendar::ICalendarPeriod::Range { start, .. })
        | Value::Period(calcard::icalendar::ICalendarPeriod::Duration { start, .. }) => start,
        _ => return None,
    };
    if !dt.has_time() {
        return date(dt).map(Moment::Date);
    }
    if dt.has_zone() {
        let wall = wall(dt)? - Duration::seconds(offset_seconds(dt));
        return Some(Moment::Time(Local { wall, zone: Zone::Tz(Tz::UTC) }));
    }
    let wall = wall(dt)?;
    let zone = match e.tz_id() {
        Some(id) => zones.zone(id, wall.year()),
        None => Zone::Floating,
    };
    Some(Moment::Time(Local { wall, zone }))
}

fn date(dt: &PartialDateTime) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(dt.year?.into(), dt.month?.into(), dt.day?.into())
}

fn wall(dt: &PartialDateTime) -> Option<NaiveDateTime> {
    let second = dt.second.unwrap_or(0).min(59);
    date(dt)?.and_hms_opt(dt.hour?.into(), dt.minute?.into(), second.into())
}

fn offset_seconds(dt: &PartialDateTime) -> i64 {
    let s = i64::from(dt.tz_hour.unwrap_or(0)) * 3600 + i64::from(dt.tz_minute.unwrap_or(0)) * 60;
    if dt.tz_minus { -s } else { s }
}

fn text(e: &ICalendarEntry) -> Option<String> {
    let parts: Vec<&str> = e.values.iter().filter_map(Value::as_text).collect();
    (!parts.is_empty()).then(|| parts.join(","))
}

fn upper(e: &ICalendarEntry) -> Option<String> {
    Some(text(e)?.trim().to_ascii_uppercase())
}

fn prop_text(comp: &ICalendarComponent, prop: &Prop) -> Option<String> {
    comp.property(prop).and_then(text)
}

fn x_props<'a>(comp: &'a ICalendarComponent, name: &'a str) -> impl Iterator<Item = &'a ICalendarEntry> {
    comp.entries.iter().filter(move |e| matches!(&e.name, Prop::Other(n) if n.eq_ignore_ascii_case(name)))
}

fn param<'a>(e: &'a ICalendarEntry, name: &Param) -> Option<&'a str> {
    e.parameter(name).and_then(|v| v.as_text())
}

fn param_upper(e: &ICalendarEntry, name: &Param) -> Option<String> {
    param(e, name).map(|v| v.trim().to_ascii_uppercase())
}

/// TZIDs to zones for one calendar. calcard resolves IANA and Windows names,
/// `X-LIC-LOCATION` and Exchange's `X-MICROSOFT-CDO-TZID`. What it can't
/// name, such as Outlook's "Customized Time Zone", is matched by its rules.
struct Zones<'a> {
    cal: &'a ICalendar,
    resolver: TzResolver<&'a str>,
    cache: HashMap<(String, i32), Zone>,
}

impl<'a> Zones<'a> {
    fn new(cal: &'a ICalendar) -> Self {
        Zones { cal, resolver: cal.build_tz_resolver(), cache: HashMap::new() }
    }

    fn zone(&mut self, id: &str, year: i32) -> Zone {
        if let Some(zone) = self.cache.get(&(id.to_string(), year)) {
            return *zone;
        }
        let zone = match self.resolver.resolve(id) {
            Some(CalTz::Tz(tz)) => Zone::Tz(tz),
            Some(CalTz::Fixed(offset)) => Zone::Tz(fixed_zone(offset.local_minus_utc(), year)),
            Some(CalTz::Floating) => Zone::Floating,
            None => match observances(self.cal, id) {
                Some(obs) => Zone::Tz(matching_zone(&obs, year)),
                // An unknown name without a definition: show it at its wall
                // time rather than guess a zone.
                None => Zone::Floating,
            },
        };
        self.cache.insert((id.to_string(), year), zone);
        zone
    }
}

/// One STANDARD or DAYLIGHT block: from `start`, and every year after when
/// `rule` (month, nth weekday) is set, the offset becomes `to`.
struct Observance {
    start: NaiveDateTime,
    from: i32,
    to: i32,
    rule: Option<(u32, i16, Weekday)>,
}

fn observances(cal: &ICalendar, id: &str) -> Option<Vec<Observance>> {
    let tz = cal.timezones().find(|c| c.property(&Prop::Tzid).and_then(text).as_deref() == Some(id))?;
    let obs: Vec<Observance> = tz
        .component_ids
        .iter()
        .filter_map(|i| cal.components.get(*i as usize))
        .filter(|c| matches!(c.component_type, Kind::Standard | Kind::Daylight))
        .filter_map(|c| {
            let offset = |p: Prop| match c.property(&p)?.values.first()? {
                Value::PartialDateTime(dt) if dt.has_zone() => Some(offset_seconds(dt) as i32),
                _ => None,
            };
            let start = match c.property(&Prop::Dtstart)?.values.first()? {
                Value::PartialDateTime(dt) => wall(dt)?,
                _ => return None,
            };
            let to = offset(Prop::Tzoffsetto)?;
            let rule = c.property(&Prop::Rrule).and_then(|e| match e.values.first()? {
                Value::RecurrenceRule(r) => {
                    let day = r.byday.first()?;
                    Some((u32::from(r.bymonth.first()?.month()), day.ordwk.unwrap_or(1), weekday(day.weekday)))
                }
                _ => None,
            });
            Some(Observance { start, from: offset(Prop::Tzoffsetfrom).unwrap_or(to), to, rule })
        })
        .collect();
    (!obs.is_empty()).then_some(obs)
}

fn weekday(day: ICalendarWeekday) -> Weekday {
    match day {
        ICalendarWeekday::Monday => Weekday::Mon,
        ICalendarWeekday::Tuesday => Weekday::Tue,
        ICalendarWeekday::Wednesday => Weekday::Wed,
        ICalendarWeekday::Thursday => Weekday::Thu,
        ICalendarWeekday::Friday => Weekday::Fri,
        ICalendarWeekday::Saturday => Weekday::Sat,
        ICalendarWeekday::Sunday => Weekday::Sun,
    }
}

fn nth_weekday(year: i32, month: u32, n: i16, day: Weekday) -> Option<NaiveDate> {
    if n > 0 {
        return NaiveDate::from_weekday_of_month_opt(year, month, day, u8::try_from(n).ok()?);
    }
    let next_month = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)?
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)?
    };
    let last = next_month.pred_opt()?;
    let back = (last.weekday().num_days_from_monday() + 7 - day.num_days_from_monday()) % 7;
    Some(last - Duration::days(i64::from(back) + 7 * (i64::from(-n) - 1)))
}

/// UTC instants (seconds) at which the zone's offset changes, around `year`.
fn onsets(obs: &[Observance], year: i32) -> Vec<(i64, i32)> {
    let mut onsets = Vec::new();
    for o in obs {
        let at = |local: NaiveDateTime| local.and_utc().timestamp() - i64::from(o.from);
        match o.rule {
            Some((month, n, day)) => {
                for y in (year - 1).max(o.start.year())..=year + 1 {
                    if let Some(d) = nth_weekday(y, month, n, day) {
                        onsets.push((at(d.and_time(o.start.time())), o.to));
                    }
                }
            }
            None => onsets.push((at(o.start), o.to)),
        }
    }
    onsets.sort_unstable();
    onsets
}

/// The IANA zone whose offsets agree with a custom VTIMEZONE in `year`:
/// first at an hour either side of each clock change, then, for a definition
/// with sloppy change times, on the 15th of every month. Common zones are
/// tried before the full list, so Brussels' rules give Europe/Brussels and
/// not Africa/Ceuta. With no match, the standard offset as `Etc/GMT±N`.
fn matching_zone(obs: &[Observance], year: i32) -> Tz {
    let onsets = onsets(obs, year);
    let offset_at = |t: i64| {
        let i = onsets.partition_point(|(at, _)| *at <= t);
        i.checked_sub(1).map(|i| onsets[i].1)
    };
    let monthly: Vec<i64> = (1..=12)
        .filter_map(|m| NaiveDate::from_ymd_opt(year, m, 15)?.and_hms_opt(12, 0, 0))
        .map(|d| d.and_utc().timestamp())
        .collect();
    let mut changes = monthly.clone();
    for (at, _) in onsets.iter().filter(|(at, _)| DateTime::from_timestamp(*at, 0).is_some_and(|d| d.year() == year)) {
        changes.extend([at - 3600, at + 3600]);
    }
    let expected = |points: &[i64]| -> Vec<(i64, i32)> {
        points.iter().filter_map(|t| Some((*t, offset_at(*t)?))).collect()
    };
    for points in [expected(&changes), expected(&monthly)] {
        if let Some(tz) = candidates().find(|tz| agrees(*tz, &points)) {
            return tz;
        }
    }
    let standard = obs.iter().min_by_key(|o| o.to).map_or(0, |o| o.to);
    etc_zone(standard)
}

/// A fixed UTC offset: `Etc/GMT±N` for whole hours, which is exactly that
/// offset all year; otherwise the zone that keeps it all of `year`
/// (+05:30 gives Asia/Kolkata).
fn fixed_zone(seconds: i32, year: i32) -> Tz {
    if seconds % 3600 == 0 {
        return etc_zone(seconds);
    }
    let points: Vec<(i64, i32)> = (1..=12)
        .filter_map(|m| NaiveDate::from_ymd_opt(year, m, 15)?.and_hms_opt(12, 0, 0))
        .map(|d| (d.and_utc().timestamp(), seconds))
        .collect();
    candidates().find(|tz| agrees(*tz, &points)).unwrap_or_else(|| etc_zone(seconds))
}

fn etc_zone(seconds: i32) -> Tz {
    let hours = (f64::from(seconds) / 3600.0).round() as i32;
    if hours == 0 {
        return Tz::UTC;
    }
    // POSIX-style names: Etc/GMT-1 is one hour ahead of UTC.
    let name = format!("Etc/GMT{}{}", if hours > 0 { '-' } else { '+' }, hours.abs());
    Tz::from_str(&name).unwrap_or(Tz::UTC)
}

fn agrees(tz: Tz, points: &[(i64, i32)]) -> bool {
    !points.is_empty()
        && points.iter().all(|(t, offset)| {
            DateTime::from_timestamp(*t, 0)
                .is_some_and(|d| tz.offset_from_utc_datetime(&d.naive_utc()).fix().local_minus_utc() == *offset)
        })
}

const COMMON_ZONES: &[Tz] = &[
    Tz::Europe__Brussels,
    Tz::Europe__London,
    Tz::Europe__Lisbon,
    Tz::Europe__Helsinki,
    Tz::Europe__Istanbul,
    Tz::Europe__Moscow,
    Tz::America__New_York,
    Tz::America__Chicago,
    Tz::America__Denver,
    Tz::America__Phoenix,
    Tz::America__Los_Angeles,
    Tz::America__Anchorage,
    Tz::Pacific__Honolulu,
    Tz::America__Halifax,
    Tz::America__St_Johns,
    Tz::America__Mexico_City,
    Tz::America__Bogota,
    Tz::America__Sao_Paulo,
    Tz::America__Santiago,
    Tz::Africa__Lagos,
    Tz::Africa__Johannesburg,
    Tz::Africa__Cairo,
    Tz::Africa__Nairobi,
    Tz::Asia__Jerusalem,
    Tz::Asia__Dubai,
    Tz::Asia__Karachi,
    Tz::Asia__Kolkata,
    Tz::Asia__Dhaka,
    Tz::Asia__Bangkok,
    Tz::Asia__Shanghai,
    Tz::Asia__Singapore,
    Tz::Asia__Tokyo,
    Tz::Asia__Seoul,
    Tz::Australia__Perth,
    Tz::Australia__Adelaide,
    Tz::Australia__Brisbane,
    Tz::Australia__Sydney,
    Tz::Pacific__Auckland,
];

/// Common zones, then every canonical-looking IANA zone. Old aliases such as
/// `US/Eastern` and the `Etc` zones are left out.
fn candidates() -> impl Iterator<Item = Tz> {
    const AREAS: &[&str] =
        &["Africa/", "America/", "Antarctica/", "Asia/", "Atlantic/", "Australia/", "Europe/", "Indian/", "Pacific/"];
    COMMON_ZONES.iter().copied().chain(
        chrono_tz::TZ_VARIANTS.iter().copied().filter(|tz| AREAS.iter().any(|a| tz.name().starts_with(a))),
    )
}


#[cfg(test)]
pub(super) mod tests {
    use chrono::NaiveDate;

    use super::*;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../../tests/fixtures/ical/", $name))
        };
    }
    pub(in crate::calendar) use fixture;

    pub(in crate::calendar) const ME: &str = "koen@example.org";

    // meeting::detect is built separately; these tests check the entries
    // read hands it, not its verdict.
    pub(in crate::calendar) fn read_events(text: &str) -> Vec<EventData> {
        read_with(text, &[ME.to_string()], |_, _, _, _| None).expect("parses")
    }

    pub(in crate::calendar) fn at(s: &str, zone: Zone) -> Local {
        Local { wall: NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap(), zone }
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn entries(text: &str) -> Vec<Conference> {
        let cals = calendars(text);
        let ev = cals[0].components.iter().find(|c| c.component_type == Kind::VEvent).unwrap();
        conference_entries(ev)
    }

    fn timed(e: &EventData) -> (Local, Local) {
        match e.span {
            Span::Timed { start, end } => (start, end),
            Span::AllDay { .. } => panic!("expected a timed event"),
        }
    }

    #[test]
    fn outlook_series_with_a_moved_occurrence() {
        let text = fixture!("outlook-series.ics");
        let events = read_events(text);
        assert_eq!(events.len(), 2);
        let paris = Zone::Tz(Tz::Europe__Paris);

        let master = &events[0];
        assert_eq!(master.recurrence_id, None);
        assert_eq!(timed(master), (at("2026-11-10 10:00", paris), at("2026-11-10 10:30", paris)));
        let r = master.recurrence.as_ref().unwrap();
        assert_eq!(r.rules, ["FREQ=WEEKLY;UNTIL=20261215T090000Z;INTERVAL=1;BYDAY=TU;WKST=MO"]);
        assert_eq!(r.exdates, [Moment::Time(at("2026-11-24 10:00", paris))]);
        assert!(master.description_html);
        assert!(master.description.starts_with("<html>"));
        assert!(master.description.contains("text/html; charset=utf-8"));
        assert_eq!(master.show_as, ShowAs::Tentative, "the Microsoft busy status wins over TRANSP");
        assert_eq!(master.visibility, Visibility::Public);
        assert_eq!(master.title, "Weekplanning");
        assert_eq!(master.organizer.as_ref().unwrap().name, "Sofie Janssens");
        let roles: Vec<_> = master.attendees.iter().map(|a| (a.person.address.as_str(), a.role, a.is_me)).collect();
        assert_eq!(
            roles,
            [
                ("koen@example.org", Role::Required, true),
                ("jan.peeters@example.com", Role::Optional, false),
                ("atomium@example.com", Role::Resource, false),
            ]
        );
        assert_eq!(master.my_response(), Some(Response::NeedsAction));
        assert_eq!(master.alarms, [Alarm { trigger: AlarmTrigger::MinutesBefore(15), email: false }]);

        let moved = &events[1];
        assert_eq!(moved.uid, master.uid);
        assert_eq!(moved.recurrence_id, Some(Moment::Time(at("2026-11-17 10:00", paris))));
        assert!(!moved.this_and_future);
        assert_eq!(timed(moved).0, at("2026-11-18 14:00", paris));
        assert_eq!(moved.sequence, 2);
        assert_eq!(moved.show_as, ShowAs::Busy);
        assert_eq!(moved.recurrence, None);

        let teams = entries(text);
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].kind, EntryKind::Video);
        assert!(teams[0].uri.starts_with("https://teams.microsoft.com/l/meetup-join/"));
    }

    #[test]
    fn outlook_customized_time_zone_is_matched_by_its_rules() {
        let events = read_events(fixture!("outlook-custom-tz.ics"));
        let e = &events[0];
        let ny = Zone::Tz(Tz::America__New_York);
        assert_eq!(timed(e), (at("2026-11-16 09:00", ny), at("2026-11-16 10:00", ny)));
        assert_eq!(e.show_as, ShowAs::OutOfOffice);
        assert_eq!(e.visibility, Visibility::Private);
        assert_eq!(e.sequence, 3);
        assert_eq!(e.alarms[0].trigger, AlarmTrigger::MinutesBefore(26 * 60));
        assert!(e.attendees[0].is_me, "mailto:Koen@Example.org is the user's address");
        assert_eq!(e.attendees[0].person.name, "", "a CN that repeats the address is no name");
        assert_eq!(e.attendees[1].role, Role::Chair);
    }

    #[test]
    fn google_invitation() {
        let text = fixture!("google.ics");
        assert_eq!(method(text).as_deref(), Some("REQUEST"));
        let e = &read_events(text)[0];
        let utc = Zone::Tz(Tz::UTC);
        assert_eq!(timed(e), (at("2026-11-05 09:00", utc), at("2026-11-05 09:45", utc)));
        assert_eq!(e.uid, "5f2k9q8v1n3m7c0b4x6z2a8d1e@google.com");
        assert!(e.description.contains("\nJoin with Google Meet: https://meet.google.com/abc-defg-hij\n"));
        let people: Vec<_> = e.attendees.iter().map(|a| (a.role, a.response)).collect();
        assert_eq!(
            people,
            [
                (Role::Required, Response::Accepted),
                (Role::Required, Response::NeedsAction),
                (Role::Optional, Response::Tentative),
                (Role::Resource, Response::Accepted),
            ]
        );
        assert_eq!(e.attendees[3].person.name, "Gent-2-Belfort (6)");
        assert_eq!(e.my_response(), Some(Response::NeedsAction));
        let meet = entries(text);
        assert_eq!(meet.len(), 1);
        assert_eq!(meet[0].uri, "https://meet.google.com/abc-defg-hij");
    }

    #[test]
    fn icloud_event() {
        let e = &read_events(fixture!("icloud.ics"))[0];
        let brussels = Zone::Tz(Tz::Europe__Brussels);
        assert_eq!(timed(e), (at("2026-11-06 12:00", brussels), at("2026-11-06 13:00", brussels)));
        assert_eq!(e.location, "Grote Markt\nGrote Markt, 1000 Brussel, België");
        assert_eq!(e.recurrence.as_ref().unwrap().rules, ["FREQ=WEEKLY;COUNT=6"]);
        assert_eq!(e.organizer, Some(Person { address: ME.into(), name: "Koen Test".into() }));
        assert_eq!(e.attendees[0].role, Role::Chair);
        assert_eq!(e.attendees[1].person.name, "Jan Peeters");
        assert_eq!(e.url.as_deref(), Some("https://example.com/lunch"));
        assert_eq!(e.color.as_deref(), Some("teal"));
        assert_eq!(e.sequence, 1);
        assert_eq!(
            e.alarms,
            [Alarm { trigger: AlarmTrigger::MinutesBefore(30), email: false }],
            "Apple's ACTION:NONE placeholder alarm is not an alarm"
        );
    }

    #[test]
    fn zoom_and_calendly() {
        let zoom = fixture!("zoom.ics");
        assert_eq!(method(zoom).as_deref(), Some("PUBLISH"));
        let z = &read_events(zoom)[0];
        assert_eq!(timed(z).0, at("2026-11-09 15:00", Zone::Tz(Tz::Europe__Brussels)));
        assert!(z.location.starts_with("https://us02web.zoom.us/j/81234567890"));
        assert!(z.description.contains("Meeting ID: 812 3456 7890\nPasscode: 424242\n"));
        assert!(z.organizer.is_none() && z.attendees.is_empty());
        assert!(entries(zoom).is_empty());

        let c = &read_events(fixture!("calendly.ics"))[0];
        assert_eq!(timed(c).1, at("2026-11-12 13:30", Zone::Tz(Tz::UTC)));
        assert_eq!(c.visibility, Visibility::Confidential);
        assert_eq!(c.alarms, [Alarm { trigger: AlarmTrigger::MinutesBefore(60), email: true }]);
        assert_eq!(c.my_response(), Some(Response::NeedsAction));
        assert_eq!(c.location, "https://meet.google.com/xyz-abcd-efg");
    }

    #[test]
    fn holiday_feed_of_all_day_events() {
        let events = read_events(fixture!("holidays.ics"));
        let spans: Vec<_> = events.iter().map(|e| (e.title.as_str(), e.span)).collect();
        let all_day = |s, e| Span::AllDay { start: day(s), end: day(e) };
        assert_eq!(
            spans,
            [
                ("Wapenstilstand", all_day("2026-11-11", "2026-11-12")),
                ("Allerheiligen", all_day("2026-11-01", "2026-11-02")),
                ("Kerstvakantie", all_day("2026-12-21", "2027-01-04")),
                ("Nieuwjaar", all_day("2027-01-01", "2027-01-02")),
            ]
        );
        assert!(events.iter().all(|e| e.show_as == ShowAs::Free));
        assert_eq!(events[0].categories, ["Holidays", "Public holiday"]);
        assert_eq!(events[0].color.as_deref(), Some("crimson"));
        assert_eq!(events[3].recurrence.as_ref().unwrap().rules, ["FREQ=YEARLY"]);
    }

    #[test]
    fn todos_and_journals_are_skipped() {
        assert_eq!(read_events(fixture!("todo.ics")), []);
    }

    #[test]
    fn malformed_input_keeps_what_can_be_read() {
        let events = read_events(fixture!("malformed.ics"));
        let uids: Vec<_> = events.iter().map(|e| e.uid.as_str()).collect();
        assert_eq!(uids, ["good@example.org", "unterminated@example.org"]);
        let good = &events[0];
        assert_eq!(timed(good).0, at("2026-11-05 10:00", Zone::Floating), "an unknown TZID floats");
        assert_eq!(good.sequence, 0);
        assert_eq!(good.attendees[0].response, Response::NeedsAction);
        assert_eq!(good.alarms, []);

        for junk in ["", "hello", "BEGIN:VCARD\nFN:Jan\nEND:VCARD\n", "\u{0}\u{1}BEGIN"] {
            assert!(read(junk, &[]).is_err(), "{junk:?}");
        }
        assert_eq!(read_events("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"), []);
    }

    #[test]
    fn span_defaults() {
        let wrap = |body: &str| format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\n{body}END:VEVENT\r\nEND:VCALENDAR\r\n");
        let floating = |s| at(s, Zone::Floating);
        let instant = &read_events(&wrap("DTSTART:20261105T100000\r\n"))[0];
        assert_eq!(timed(instant), (floating("2026-11-05 10:00"), floating("2026-11-05 10:00")));
        let lasting = &read_events(&wrap("DTSTART:20261105T100000\r\nDURATION:PT1H30M\r\n"))[0];
        assert_eq!(timed(lasting).1, floating("2026-11-05 11:30"));
        let week = &read_events(&wrap("DTSTART;VALUE=DATE:20261105\r\nDURATION:P1W\r\n"))[0];
        assert_eq!(week.span, Span::AllDay { start: day("2026-11-05"), end: day("2026-11-12") });
    }

    #[test]
    fn reply_this_and_future_and_conference() {
        let text = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:r\r\n\
            RECURRENCE-ID;RANGE=THISANDFUTURE:20261105T100000Z\r\nDTSTART:20261105T100000Z\r\n\
            ATTENDEE;PARTSTAT=ACCEPTED;CN=Jan:mailto:jan@example.com\r\nCOMMENT:See you there\r\n\
            CONFERENCE;VALUE=URI;FEATURE=PHONE;LABEL=Belgium:tel:+3225886312,,123#\r\n\
            CONFERENCE;VALUE=URI;FEATURE=VIDEO,AUDIO:https://example.com/join/42\r\n\
            END:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(method(text).as_deref(), Some("REPLY"));
        let e = &read_events(text)[0];
        assert!(e.this_and_future);
        assert_eq!(e.attendees[0].comment, "See you there");
        let kinds: Vec<_> = entries(text).into_iter().map(|c| (c.kind, c.uri)).collect();
        assert_eq!(
            kinds,
            [
                (EntryKind::Phone, "tel:+3225886312,,123#".to_string()),
                (EntryKind::Video, "https://example.com/join/42".to_string()),
            ]
        );
    }

    fn custom_zone(observances: &str, start: &str) -> Zone {
        let text = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Customized Time Zone\r\n{observances}END:VTIMEZONE\r\n\
             BEGIN:VEVENT\r\nUID:z\r\nDTSTART;TZID=Customized Time Zone:{start}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        timed(&read_events(&text)[0]).0.zone
    }

    #[test]
    fn custom_zones_map_to_iana() {
        let brussels = "BEGIN:STANDARD\r\nDTSTART:16010101T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\n\
            RRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\n\
            TZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=3\r\nEND:DAYLIGHT\r\n";
        assert_eq!(custom_zone(brussels, "20260706T090000"), Zone::Tz(Tz::Europe__Brussels));

        let sloppy = brussels.replace("16010101T020000", "16010101T050000");
        assert_eq!(custom_zone(&sloppy, "20260706T090000"), Zone::Tz(Tz::Europe__Brussels), "change times off by hours");

        let sydney = "BEGIN:STANDARD\r\nDTSTART:16010101T030000\r\nTZOFFSETFROM:+1100\r\nTZOFFSETTO:+1000\r\n\
            RRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=4\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\n\
            TZOFFSETFROM:+1000\r\nTZOFFSETTO:+1100\r\nRRULE:FREQ=YEARLY;BYDAY=1SU;BYMONTH=10\r\nEND:DAYLIGHT\r\n";
        assert_eq!(custom_zone(sydney, "20261116T090000"), Zone::Tz(Tz::Australia__Sydney));

        let india = "BEGIN:STANDARD\r\nDTSTART:16010101T000000\r\nTZOFFSETFROM:+0530\r\nTZOFFSETTO:+0530\r\nEND:STANDARD\r\n";
        assert_eq!(custom_zone(india, "20261116T090000"), Zone::Tz(Tz::Asia__Kolkata));

        let nowhere = "BEGIN:STANDARD\r\nDTSTART:16010101T000000\r\nTZOFFSETFROM:+0317\r\nTZOFFSETTO:+0317\r\nEND:STANDARD\r\n";
        assert_eq!(custom_zone(nowhere, "20261116T090000"), Zone::Tz(Tz::Etc__GMTMinus3));

        assert_eq!(fixed_zone(3600, 2026), Tz::Etc__GMTMinus1);
        assert_eq!(fixed_zone(-5 * 3600, 2026), Tz::Etc__GMTPlus5);
        assert_eq!(fixed_zone(0, 2026), Tz::UTC);
        assert_eq!(fixed_zone(5 * 3600 + 1800, 2026), Tz::Asia__Kolkata);
    }
}
