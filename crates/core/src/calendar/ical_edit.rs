//! Edits an iCalendar object without damaging it. CalDAV has no partial
//! update, so Jolta writes back the server's object with only the edited
//! lines changed. Every line it didn't touch is written back byte for byte,
//! including folds, parameter quoting and the line ending style; a general
//! iCalendar writer would re-quote, refold and re-escape them, which breaks
//! values such as Apple's structured location (`geo:50.85,4.35`).

use std::fmt;

use chrono::{NaiveDate, NaiveDateTime, Utc};
use chrono_tz::Tz;

use super::ical::strip_mailto;
use super::*;

/// A parsed iCalendar text: content lines grouped into components.
#[derive(Clone, Debug)]
pub struct Object {
    nodes: Vec<Node>,
    eol: &'static str,
}

#[derive(Clone, Debug)]
pub struct Component {
    name: String,
    begin: Line,
    nodes: Vec<Node>,
    /// None when the text ended before the component did.
    end: Option<Line>,
}

#[derive(Clone, Debug)]
enum Node {
    Line(Line),
    Component(Component),
}

#[derive(Clone, Debug)]
struct Line {
    /// None for a line that isn't a content line, such as a blank one.
    prop: Option<Prop>,
    /// The bytes as read, folds included, and the line ending after them.
    /// None once the line is changed or for a new line.
    original: Option<(String, String)>,
}

/// One content line, unfolded: `NAME;PARAM=VALUE:value`. The value is kept
/// as written, escapes included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prop {
    name: String,
    params: Vec<Param>,
    value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Param {
    name: String,
    values: Vec<String>,
    /// As written (`CN="Jan Peeters"`), so an untouched parameter keeps its
    /// quoting when another one on the same line changes.
    raw: Option<String>,
}

impl Object {
    pub fn parse(text: &str) -> Object {
        let mut eol = None;
        let mut stack: Vec<Component> = Vec::new();
        let mut root = Vec::new();
        fn push(stack: &mut [Component], root: &mut Vec<Node>, node: Node) {
            match stack.last_mut() {
                Some(top) => top.nodes.push(node),
                None => root.push(node),
            }
        }
        for (raw, end, unfolded) in logical_lines(text) {
            if eol.is_none() && !end.is_empty() {
                eol = Some(if end == "\r\n" { "\r\n" } else { "\n" });
            }
            let prop = parse_prop(&unfolded);
            let line = Line { prop, original: Some((raw, end)) };
            let marker = line.prop.as_ref().map(|p| (p.name.as_str(), p.value.trim().to_ascii_uppercase()));
            match marker {
                Some(("BEGIN", value)) if !value.is_empty() => {
                    stack.push(Component { name: value, begin: line, nodes: Vec::new(), end: None });
                }
                Some(("END", value)) if stack.iter().any(|c| c.name == value) => {
                    while stack.last().is_some_and(|c| c.name != value) {
                        let open = stack.pop().expect("checked above");
                        push(&mut stack, &mut root, Node::Component(open));
                    }
                    let mut closed = stack.pop().expect("checked above");
                    closed.end = Some(line);
                    push(&mut stack, &mut root, Node::Component(closed));
                }
                _ => push(&mut stack, &mut root, Node::Line(line)),
            }
        }
        while let Some(open) = stack.pop() {
            push(&mut stack, &mut root, Node::Component(open));
        }
        Object { nodes: root, eol: eol.unwrap_or("\r\n") }
    }

    /// The text with an END line added for every component that lacks one,
    /// or None when nothing was missing.
    pub(super) fn closed(&self) -> Option<String> {
        fn close(nodes: &mut [Node]) -> bool {
            let mut changed = false;
            for node in nodes {
                if let Node::Component(c) = node {
                    changed |= close(&mut c.nodes);
                    if c.end.is_none() {
                        c.end = Some(Line::new(Prop::new("END", &c.name)));
                        changed = true;
                    }
                }
            }
            changed
        }
        let mut copy = self.clone();
        close(&mut copy.nodes).then(|| copy.to_string())
    }

    /// A text holding one component, normally a VCALENDAR, with CRLF line endings.
    pub fn new(calendar: Component) -> Object {
        Object { nodes: vec![Node::Component(calendar)], eol: "\r\n" }
    }

    pub fn calendar(&self) -> Option<&Component> {
        self.nodes.iter().find_map(|n| match n {
            Node::Component(c) if c.name == "VCALENDAR" => Some(c),
            _ => None,
        })
    }

    /// The first VCALENDAR, to set METHOD on a reply, for example.
    pub fn calendar_mut(&mut self) -> Option<&mut Component> {
        self.nodes.iter_mut().find_map(|n| match n {
            Node::Component(c) if c.name == "VCALENDAR" => Some(c),
            _ => None,
        })
    }

    /// The VEVENT with this UID and RECURRENCE-ID value as written (None for
    /// the master or a single event).
    pub fn event(&self, uid: &str, recurrence_id: Option<&str>) -> Option<&Component> {
        self.event_path(uid, recurrence_id).map(|(list, i)| match &self.list(&list)[i] {
            Node::Component(c) => c,
            Node::Line(_) => unreachable!("event_path points at a component"),
        })
    }

    pub fn event_mut(&mut self, uid: &str, recurrence_id: Option<&str>) -> Option<&mut Component> {
        let (list, i) = self.event_path(uid, recurrence_id)?;
        match &mut self.list_mut(&list)[i] {
            Node::Component(c) => Some(c),
            Node::Line(_) => None,
        }
    }

    /// Every VEVENT with this UID: the master and its overrides.
    pub fn events_mut(&mut self, uid: &str) -> Vec<&mut Component> {
        fn collect<'a>(nodes: &'a mut [Node], uid: &str, out: &mut Vec<&'a mut Component>) {
            for node in nodes {
                let Node::Component(c) = node else { continue };
                if c.name == "VEVENT" {
                    if c.prop("UID").is_some_and(|p| p.text() == uid) {
                        out.push(c);
                    }
                } else if c.name == "VCALENDAR" {
                    collect(&mut c.nodes, uid, out);
                }
            }
        }
        let mut out = Vec::new();
        collect(&mut self.nodes, uid, &mut out);
        out
    }

    /// Adds `event` (an override of one occurrence) right after the master
    /// with the same UID. False when there's no master.
    pub fn add_after_master(&mut self, event: Component) -> bool {
        let Some(uid) = event.prop("UID").map(Prop::text) else { return false };
        let Some((list, i)) = self.event_path(&uid, None) else { return false };
        self.list_mut(&list).insert(i + 1, Node::Component(event));
        true
    }

    pub fn remove_event(&mut self, uid: &str, recurrence_id: Option<&str>) -> bool {
        let Some((list, i)) = self.event_path(uid, recurrence_id) else { return false };
        self.list_mut(&list).remove(i);
        true
    }

    /// Where the event sits: the indexes of the components down to its list,
    /// and its index in that list.
    fn event_path(&self, uid: &str, recurrence_id: Option<&str>) -> Option<(Vec<usize>, usize)> {
        fn find(nodes: &[Node], uid: &str, rid: Option<&str>, path: &mut Vec<usize>) -> Option<usize> {
            for (i, node) in nodes.iter().enumerate() {
                let Node::Component(c) = node else { continue };
                if c.name == "VEVENT" {
                    let matches = c.prop("UID").is_some_and(|p| p.text() == uid)
                        && c.prop("RECURRENCE-ID").map(|p| p.value.trim()) == rid.map(str::trim);
                    if matches {
                        return Some(i);
                    }
                } else if c.name == "VCALENDAR" {
                    path.push(i);
                    if let Some(found) = find(&c.nodes, uid, rid, path) {
                        return Some(found);
                    }
                    path.pop();
                }
            }
            None
        }
        let mut path = Vec::new();
        let i = find(&self.nodes, uid, recurrence_id, &mut path)?;
        Some((path, i))
    }

    fn list(&self, path: &[usize]) -> &[Node] {
        path.iter().fold(&self.nodes[..], |nodes, i| match &nodes[*i] {
            Node::Component(c) => &c.nodes,
            Node::Line(_) => unreachable!("paths only go through components"),
        })
    }

    fn list_mut(&mut self, path: &[usize]) -> &mut Vec<Node> {
        path.iter().fold(&mut self.nodes, |nodes, i| match &mut nodes[*i] {
            Node::Component(c) => &mut c.nodes,
            Node::Line(_) => unreachable!("paths only go through components"),
        })
    }
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        write_nodes(&self.nodes, self.eol, &mut out);
        f.write_str(&out)
    }
}

fn write_nodes(nodes: &[Node], eol: &str, out: &mut String) {
    for node in nodes {
        match node {
            Node::Line(line) => write_line(line, eol, out),
            Node::Component(c) => {
                write_line(&c.begin, eol, out);
                write_nodes(&c.nodes, eol, out);
                if let Some(end) = &c.end {
                    write_line(end, eol, out);
                }
            }
        }
    }
}

fn write_line(line: &Line, eol: &str, out: &mut String) {
    // Only the last line of a text can lack its line ending; anything
    // written after it starts on a line of its own.
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    match (&line.original, &line.prop) {
        (Some((raw, end)), _) => {
            out.push_str(raw);
            out.push_str(end);
        }
        (None, Some(prop)) => {
            out.push_str(&fold(&prop.to_string(), eol));
            out.push_str(eol);
        }
        (None, None) => out.push_str(eol),
    }
}

impl Line {
    fn new(prop: Prop) -> Line {
        Line { prop: Some(prop), original: None }
    }

    fn change(&mut self, change: impl FnOnce(&mut Prop)) {
        let Some(prop) = self.prop.as_mut() else { return };
        let before = prop.clone();
        change(prop);
        if *prop != before {
            self.original = None;
        }
    }
}

impl Component {
    pub fn new(name: &str) -> Component {
        let name = name.to_ascii_uppercase();
        Component {
            begin: Line::new(Prop::new("BEGIN", &name)),
            end: Some(Line::new(Prop::new("END", &name))),
            name,
            nodes: Vec::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn props(&self) -> impl Iterator<Item = &Prop> {
        self.nodes.iter().filter_map(|n| match n {
            Node::Line(Line { prop: Some(p), .. }) => Some(p),
            _ => None,
        })
    }

    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props().find(|p| p.is(name))
    }

    /// Sets the first `name` line's raw value, keeping its parameters, or
    /// adds the line.
    pub fn set(&mut self, name: &str, value: &str) {
        let first = self.nodes.iter_mut().find_map(|n| match n {
            Node::Line(line) if line.prop.as_ref().is_some_and(|p| p.is(name)) => Some(line),
            _ => None,
        });
        match first {
            Some(line) => line.change(|p| p.value = value.to_string()),
            None => self.add(Prop::new(name, value)),
        }
    }

    /// `set` for a TEXT value, which gets escaped.
    pub fn set_text(&mut self, name: &str, text: &str) {
        self.set(name, &escape(text));
    }

    /// Adds a line after the last property line, so before any VALARM.
    pub fn add(&mut self, prop: Prop) {
        let at = self.nodes.iter().rposition(|n| matches!(n, Node::Line(_))).map_or(0, |i| i + 1);
        self.nodes.insert(at, Node::Line(Line::new(prop)));
    }

    /// Removes every `name` line and returns how many there were.
    pub fn remove(&mut self, name: &str) -> usize {
        let before = self.nodes.len();
        self.nodes.retain(|n| !matches!(n, Node::Line(Line { prop: Some(p), .. }) if p.is(name)));
        before - self.nodes.len()
    }

    /// Keeps only the `name` lines `keep` picks.
    pub fn retain(&mut self, name: &str, mut keep: impl FnMut(&Prop) -> bool) {
        self.nodes.retain(|n| !matches!(n, Node::Line(Line { prop: Some(p), .. }) if p.is(name) && !keep(p)));
    }

    /// Changes the `name` lines that `select` picks with `change`, such as
    /// setting PARTSTAT on one ATTENDEE. A line `change` leaves equal stays
    /// byte for byte. Returns how many lines matched.
    pub fn update(&mut self, name: &str, mut select: impl FnMut(&Prop) -> bool, mut change: impl FnMut(&mut Prop)) -> usize {
        let mut matched = 0;
        for node in &mut self.nodes {
            let Node::Line(line) = node else { continue };
            if line.prop.as_ref().is_some_and(|p| p.is(name) && select(p)) {
                matched += 1;
                line.change(&mut change);
            }
        }
        matched
    }

    /// Adds an EXDATE in DTSTART's form: `value` as written in DTSTART or
    /// RECURRENCE-ID (`20261124T100000`), with DTSTART's TZID and VALUE.
    pub fn add_exdate(&mut self, value: &str) {
        let mut exdate = Prop::new("EXDATE", value);
        if let Some(start) = self.prop("DTSTART") {
            exdate.params = start.params.iter().filter(|p| p.is("TZID") || p.is("VALUE")).cloned().collect();
        }
        self.add(exdate);
    }

    pub fn push(&mut self, component: Component) {
        self.nodes.push(Node::Component(component));
    }

    /// Removes every component named `name` directly inside this one, such
    /// as a VEVENT's VALARMs.
    pub fn remove_components(&mut self, name: &str) {
        self.nodes.retain(|n| !matches!(n, Node::Component(c) if c.name.eq_ignore_ascii_case(name)));
    }

    /// The components directly inside this one, such as a VCALENDAR's
    /// VTIMEZONEs and VEVENTs.
    pub fn components(&self) -> impl Iterator<Item = &Component> {
        self.nodes.iter().filter_map(|n| match n {
            Node::Component(c) => Some(c),
            Node::Line(_) => None,
        })
    }
}

impl Prop {
    /// One unfolded content line such as `X-JOLTA-HOLD;X=1:yes`.
    pub fn parse(line: &str) -> Option<Prop> {
        parse_prop(line.trim_end_matches(['\r', '\n']))
    }

    /// A line with a raw value (a date, a URI, a number).
    pub fn new(name: &str, value: &str) -> Prop {
        Prop { name: name.to_ascii_uppercase(), params: Vec::new(), value: value.to_string() }
    }

    /// A line with a TEXT value, escaped per RFC 5545.
    pub fn text_value(name: &str, text: &str) -> Prop {
        Prop::new(name, &escape(text))
    }

    /// DTSTART, DTEND, RECURRENCE-ID, RDATE or EXDATE for a moment: a date
    /// with VALUE=DATE, a UTC time with `Z`, a zoned time with its IANA TZID
    /// (no VTIMEZONE needed by iCloud, Fastmail or Google), or a floating time.
    pub fn moment(name: &str, moment: &Moment) -> Prop {
        match moment {
            Moment::Date(d) => Prop::new(name, &format_date(*d)).with_param("VALUE", "DATE"),
            Moment::Time(Local { wall, zone: Zone::Tz(Tz::UTC) }) => {
                Prop::new(name, &format!("{}Z", format_time(*wall)))
            }
            Moment::Time(Local { wall, zone: Zone::Tz(tz) }) => {
                Prop::new(name, &format_time(*wall)).with_param("TZID", tz.name())
            }
            Moment::Time(Local { wall, zone: Zone::Floating }) => Prop::new(name, &format_time(*wall)),
        }
    }

    pub fn with_param(mut self, name: &str, value: &str) -> Prop {
        self.set_param(name, value);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }

    /// The value as written, escapes included.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The value read as TEXT, escapes undone.
    pub fn text(&self) -> String {
        unescape(&self.value)
    }

    /// The address of a `mailto:` value, as on ATTENDEE and ORGANIZER.
    pub fn address(&self) -> Option<&str> {
        strip_mailto(self.value.trim())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|p| p.is(name)).and_then(|p| p.values.first()).map(String::as_str)
    }

    /// Sets a parameter to one value, replacing it where it was or adding it
    /// at the end.
    pub fn set_param(&mut self, name: &str, value: &str) {
        let param = Param { name: name.to_ascii_uppercase(), values: vec![value.to_string()], raw: None };
        match self.params.iter_mut().find(|p| p.is(name)) {
            Some(p) if p.values == param.values => {}
            Some(p) => *p = param,
            None => self.params.push(param),
        }
    }

    pub fn remove_param(&mut self, name: &str) {
        self.params.retain(|p| !p.is(name));
    }
}

impl fmt::Display for Prop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        for p in &self.params {
            match &p.raw {
                Some(raw) => write!(f, ";{raw}")?,
                None => {
                    write!(f, ";{}=", p.name)?;
                    for (i, v) in p.values.iter().enumerate() {
                        let v = v.replace('"', "'");
                        let sep = if i == 0 { "" } else { "," };
                        if v.contains([':', ';', ',']) {
                            write!(f, "{sep}\"{v}\"")?;
                        } else {
                            write!(f, "{sep}{v}")?;
                        }
                    }
                }
            }
        }
        write!(f, ":{}", self.value)
    }
}

impl Param {
    fn is(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name)
    }
}

/// Splits text into content lines: (bytes as written with folds, line
/// ending, unfolded text). A line starting with a space or tab continues the
/// one before (RFC 5545 section 3.1).
fn logical_lines(text: &str) -> Vec<(String, String, String)> {
    let mut lines: Vec<(String, String, String)> = Vec::new();
    for piece in text.split_inclusive('\n') {
        let (body, end) = match piece.strip_suffix("\r\n") {
            Some(body) => (body, "\r\n"),
            None => match piece.strip_suffix('\n') {
                Some(body) => (body, "\n"),
                None => (piece, ""),
            },
        };
        match lines.last_mut() {
            Some((raw, last_end, unfolded)) if body.starts_with([' ', '\t']) && !last_end.is_empty() => {
                raw.push_str(last_end);
                raw.push_str(body);
                *last_end = end.to_string();
                unfolded.push_str(&body[1..]);
            }
            _ => lines.push((body.to_string(), end.to_string(), body.to_string())),
        }
    }
    lines
}

fn parse_prop(line: &str) -> Option<Prop> {
    let name_end = line.find([';', ':'])?;
    let name = line[..name_end].trim_start_matches('\u{feff}');
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut params = Vec::new();
    let mut rest = &line[name_end..];
    while let Some(after) = rest.strip_prefix(';') {
        let (param, tail) = split_param(after)?;
        params.push(param);
        rest = tail;
    }
    let value = rest.strip_prefix(':')?;
    Some(Prop { name: name.to_ascii_uppercase(), params, value: value.to_string() })
}

/// One `NAME=v1,"v2"` parameter at the start of `s`, and what follows it.
fn split_param(s: &str) -> Option<(Param, &str)> {
    let mut values = Vec::new();
    let eq = s.find(['=', ';', ':'])?;
    let name = &s[..eq];
    let mut i = eq;
    if s[eq..].starts_with('=') {
        i += 1;
        loop {
            let tail = &s[i..];
            let (value, len) = if let Some(quoted) = tail.strip_prefix('"') {
                let close = quoted.find('"')?;
                (&quoted[..close], close + 2)
            } else {
                let stop = tail.find([',', ';', ':']).unwrap_or(tail.len());
                (&tail[..stop], stop)
            };
            values.push(value.to_string());
            i += len;
            if s[i..].starts_with(',') {
                i += 1;
            } else {
                break;
            }
        }
    }
    let param = Param { name: name.to_ascii_uppercase(), values, raw: Some(s[..i].to_string()) };
    Some((param, &s[i..]))
}

/// Writes `line` as lines of at most 75 octets, never splitting a UTF-8
/// character; each continuation starts with a space.
fn fold(line: &str, eol: &str) -> String {
    let mut out = String::with_capacity(line.len() + line.len() / 70 * 3);
    let mut width = 0;
    for ch in line.chars() {
        let len = ch.len_utf8();
        if width + len > 75 {
            out.push_str(eol);
            out.push(' ');
            width = 1;
        }
        out.push(ch);
        width += len;
    }
    out
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let text = text.replace("\r\n", "\n");
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\;"),
            ',' => out.push_str("\\,"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn format_date(d: NaiveDate) -> String {
    d.format("%Y%m%d").to_string()
}

fn format_time(t: NaiveDateTime) -> String {
    t.format("%Y%m%dT%H%M%S").to_string()
}

/// A new VCALENDAR with one VEVENT, for a new event or a reply. Zoned times
/// carry their IANA TZID without a VTIMEZONE.
pub fn write_event(e: &EventData, prodid: &str) -> String {
    let mut cal = Component::new("VCALENDAR");
    cal.add(Prop::new("VERSION", "2.0"));
    cal.add(Prop::text_value("PRODID", prodid));
    cal.add(Prop::new("CALSCALE", "GREGORIAN"));
    cal.push(vevent(e));
    Object::new(cal).to_string()
}

/// The VEVENT for `e`, every field Jolta models written out.
pub fn vevent(e: &EventData) -> Component {
    use super::change::Field;
    let mut ev = Component::new("VEVENT");
    ev.add(Prop::text_value("UID", &e.uid));
    ev.add(Prop::new("DTSTAMP", &format!("{}Z", format_time(Utc::now().naive_utc()))));
    if let Some(rid) = &e.recurrence_id {
        ev.add(Prop::moment("RECURRENCE-ID", rid));
    }
    for field in [Field::Time, Field::Repeat, Field::Title, Field::Location, Field::Description] {
        write_field(&mut ev, field, e);
    }
    if let Some(url) = &e.url {
        ev.add(Prop::new("URL", url).with_param("VALUE", "URI"));
    }
    for field in [Field::Status, Field::ShowAs, Field::Visibility] {
        write_field(&mut ev, field, e);
    }
    if let Some(o) = &e.organizer {
        ev.add(person_prop("ORGANIZER", o));
    }
    write_field(&mut ev, Field::People, e);
    if !e.options.allow_new_times {
        write_field(&mut ev, Field::Options, e);
    }
    ev.add(Prop::new("SEQUENCE", &e.sequence.to_string()));
    for field in [Field::Categories, Field::Color, Field::MeetingLink, Field::Alarms] {
        write_field(&mut ev, field, e);
    }
    ev
}

/// Replaces the lines a field is written as in `ev` with `e`'s value; a
/// field without a value leaves none. Every other line stays as it was.
pub fn write_field(ev: &mut Component, field: super::change::Field, e: &EventData) {
    use super::change::Field;
    match field {
        Field::Time => {
            for name in ["DTSTART", "DTEND", "DURATION"] {
                ev.remove(name);
            }
            let (start, end) = match e.span {
                Span::AllDay { start, end } => (Moment::Date(start), Moment::Date(end)),
                Span::Timed { start, end } => (Moment::Time(start), Moment::Time(end)),
            };
            ev.add(Prop::moment("DTSTART", &start));
            ev.add(Prop::moment("DTEND", &end));
        }
        Field::Repeat => {
            for name in ["RRULE", "RDATE", "EXDATE"] {
                ev.remove(name);
            }
            if let Some(r) = &e.recurrence {
                for rule in &r.rules {
                    ev.add(Prop::new("RRULE", rule));
                }
                for m in &r.rdates {
                    ev.add(Prop::moment("RDATE", m));
                }
                for m in &r.exdates {
                    ev.add(Prop::moment("EXDATE", m));
                }
            }
        }
        Field::Title | Field::Location => {
            let (name, text) = if field == Field::Title { ("SUMMARY", &e.title) } else { ("LOCATION", &e.location) };
            ev.remove(name);
            if !text.is_empty() {
                ev.add(Prop::text_value(name, text));
            }
        }
        Field::Description => {
            ev.remove("DESCRIPTION");
            ev.remove("X-ALT-DESC");
            if e.description_html {
                ev.add(Prop::text_value("DESCRIPTION", &html_to_text(&e.description)));
                ev.add(Prop::text_value("X-ALT-DESC", &e.description).with_param("FMTTYPE", "text/html"));
            } else if !e.description.is_empty() {
                ev.add(Prop::text_value("DESCRIPTION", &e.description));
            }
        }
        Field::Status => {
            let status = match e.status {
                EventStatus::Confirmed => "CONFIRMED",
                EventStatus::Tentative => "TENTATIVE",
                EventStatus::Cancelled => "CANCELLED",
            };
            ev.remove("STATUS");
            ev.add(Prop::new("STATUS", status));
        }
        Field::ShowAs => {
            ev.remove("TRANSP");
            ev.remove("X-MICROSOFT-CDO-BUSYSTATUS");
            ev.add(Prop::new("TRANSP", if e.show_as == ShowAs::Free { "TRANSPARENT" } else { "OPAQUE" }));
            let microsoft = match e.show_as {
                ShowAs::Free | ShowAs::Busy => None,
                ShowAs::Tentative => Some("TENTATIVE"),
                ShowAs::OutOfOffice => Some("OOF"),
                ShowAs::Elsewhere => Some("WORKINGELSEWHERE"),
            };
            if let Some(status) = microsoft {
                ev.add(Prop::new("X-MICROSOFT-CDO-BUSYSTATUS", status));
            }
        }
        Field::Visibility => {
            ev.remove("CLASS");
            let class = match e.visibility {
                Visibility::Default => None,
                Visibility::Public => Some("PUBLIC"),
                Visibility::Private => Some("PRIVATE"),
                Visibility::Confidential => Some("CONFIDENTIAL"),
            };
            if let Some(class) = class {
                ev.add(Prop::new("CLASS", class));
            }
        }
        // A person who stays keeps their line, with what the server added
        // to it (SCHEDULE-STATUS, their answer); only a new role changes it.
        Field::People => {
            let find = |p: &Prop| p.address().and_then(|address| e.attendees.iter().find(|a| a.person.address.eq_ignore_ascii_case(address)));
            ev.retain("ATTENDEE", |p| find(p).is_some());
            let mut kept: Vec<String> = Vec::new();
            ev.update("ATTENDEE", |_| true, |p| {
                let Some(a) = find(p) else { return };
                kept.push(a.person.address.to_lowercase());
                if line_role(p) != a.role {
                    let (cutype, role) = kinds(a.role);
                    p.set_param("CUTYPE", cutype);
                    p.set_param("ROLE", role);
                }
            });
            for a in e.attendees.iter().filter(|a| !kept.contains(&a.person.address.to_lowercase())) {
                ev.add(attendee_prop(a, e.options.request_responses));
            }
        }
        Field::Options => {
            let rsvp = if e.options.request_responses { "TRUE" } else { "FALSE" };
            ev.update("ATTENDEE", |p| !matches!(p.param("CUTYPE").map(str::to_ascii_uppercase).as_deref(), Some("ROOM" | "RESOURCE")), |p| p.set_param("RSVP", rsvp));
            ev.remove("X-MICROSOFT-DISALLOW-COUNTER");
            if !e.options.allow_new_times {
                ev.add(Prop::new("X-MICROSOFT-DISALLOW-COUNTER", "TRUE"));
            }
        }
        Field::Categories => {
            ev.remove("CATEGORIES");
            if !e.categories.is_empty() {
                let list: Vec<String> = e.categories.iter().map(|c| escape(c)).collect();
                ev.add(Prop::new("CATEGORIES", &list.join(",")));
            }
        }
        Field::Color => {
            ev.remove("COLOR");
            if let Some(color) = &e.color {
                ev.add(Prop::text_value("COLOR", color));
            }
        }
        Field::MeetingLink => {
            ev.remove("CONFERENCE");
            if let Some(m) = &e.meeting {
                ev.add(Prop::new("CONFERENCE", &m.join_url).with_param("VALUE", "URI").with_param("FEATURE", "VIDEO"));
                for phone in &m.phones {
                    ev.add(Prop::new("CONFERENCE", phone).with_param("VALUE", "URI").with_param("FEATURE", "PHONE"));
                }
            }
        }
        Field::Alarms => {
            ev.remove_components("VALARM");
            for alarm in &e.alarms {
                ev.push(alarm_component(alarm, &e.title));
            }
        }
    }
}

/// The current time as DTSTAMP and LAST-MODIFIED take it.
pub fn utc_stamp() -> String {
    format!("{}Z", format_time(Utc::now().naive_utc()))
}

fn person_prop(name: &str, p: &Person) -> Prop {
    let prop = Prop::new(name, &format!("mailto:{}", p.address));
    if p.name.is_empty() { prop } else { prop.with_param("CN", &p.name) }
}

/// CUTYPE and ROLE for a role.
fn kinds(role: Role) -> (&'static str, &'static str) {
    match role {
        Role::Required => ("INDIVIDUAL", "REQ-PARTICIPANT"),
        Role::Optional => ("INDIVIDUAL", "OPT-PARTICIPANT"),
        Role::Chair => ("INDIVIDUAL", "CHAIR"),
        Role::Fyi => ("INDIVIDUAL", "NON-PARTICIPANT"),
        Role::Room => ("ROOM", "NON-PARTICIPANT"),
        Role::Resource => ("RESOURCE", "NON-PARTICIPANT"),
    }
}

/// The role an ATTENDEE line gives, as the reader takes it.
fn line_role(p: &Prop) -> Role {
    let upper = |name| p.param(name).map(str::to_ascii_uppercase);
    match (upper("CUTYPE").as_deref(), upper("ROLE").as_deref()) {
        (Some("ROOM"), _) => Role::Room,
        (Some("RESOURCE"), _) => Role::Resource,
        (_, Some("CHAIR")) => Role::Chair,
        (_, Some("OPT-PARTICIPANT")) => Role::Optional,
        (_, Some("NON-PARTICIPANT")) => Role::Fyi,
        _ => Role::Required,
    }
}

fn attendee_prop(a: &Attendee, request_responses: bool) -> Prop {
    let (cutype, role) = kinds(a.role);
    let partstat = match a.response {
        Response::NeedsAction => "NEEDS-ACTION",
        Response::Accepted => "ACCEPTED",
        Response::Tentative => "TENTATIVE",
        Response::Declined => "DECLINED",
        Response::Delegated => "DELEGATED",
    };
    let mut prop = person_prop("ATTENDEE", &a.person)
        .with_param("CUTYPE", cutype)
        .with_param("ROLE", role)
        .with_param("PARTSTAT", partstat);
    if a.response == Response::NeedsAction && cutype == "INDIVIDUAL" {
        prop.set_param("RSVP", if request_responses { "TRUE" } else { "FALSE" });
    }
    if !a.comment.is_empty() {
        prop.set_param("X-RESPONSE-COMMENT", &a.comment.replace(['\r', '\n'], " "));
    }
    prop
}

fn alarm_component(alarm: &Alarm, title: &str) -> Component {
    let mut c = Component::new("VALARM");
    c.add(Prop::new("ACTION", if alarm.email { "EMAIL" } else { "DISPLAY" }));
    let trigger = match alarm.trigger {
        AlarmTrigger::MinutesBefore(m) if m >= 0 => Prop::new("TRIGGER", &format!("-PT{m}M")),
        AlarmTrigger::MinutesBefore(m) => Prop::new("TRIGGER", &format!("PT{}M", -m)),
        AlarmTrigger::At(at) => {
            Prop::new("TRIGGER", &format!("{}Z", format_time(at.naive_utc()))).with_param("VALUE", "DATE-TIME")
        }
    };
    c.add(trigger);
    c.add(Prop::text_value("DESCRIPTION", if title.is_empty() { "Reminder" } else { title }));
    if alarm.email {
        c.add(Prop::text_value("SUMMARY", if title.is_empty() { "Reminder" } else { title }));
    }
    c
}

/// A plain-text DESCRIPTION next to X-ALT-DESC, for clients that only read
/// the plain one. Tags dropped, the common entities decoded.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, NaiveDate};

    use super::*;
    use crate::calendar::ical::tests::{at, fixture, read_events, ME};

    const FIXTURES: &[(&str, &str)] = &[
        ("outlook-series", fixture!("outlook-series.ics")),
        ("outlook-custom-tz", fixture!("outlook-custom-tz.ics")),
        ("google", fixture!("google.ics")),
        ("icloud", fixture!("icloud.ics")),
        ("zoom", fixture!("zoom.ics")),
        ("calendly", fixture!("calendly.ics")),
        ("holidays", fixture!("holidays.ics")),
        ("todo", fixture!("todo.ics")),
        ("malformed", fixture!("malformed.ics")),
    ];

    const ICLOUD_UID: &str = "7A1C5E2B-93D4-4F6A-8B0C-1D2E3F4A5B6C";
    const OUTLOOK_UID: &str = "040000008200E00074C5B7101A82E00800000000A1B2C3D4E5F6D901000000000000000010000000F00DFACEB00C4B6A9E2F5B3D8C7A6E51";

    /// Content lines as written, folds and line endings included.
    fn lines(text: &str) -> Vec<String> {
        logical_lines(text).into_iter().map(|(raw, end, _)| raw + &end).collect()
    }

    /// (lines only in `before`, lines only in `after`), by longest common
    /// subsequence, so a moved line would show as removed and added.
    fn diff(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
        let (a, b) = (lines(before), lines(after));
        let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
            }
        }
        let (mut i, mut j, mut removed, mut added) = (0, 0, Vec::new(), Vec::new());
        while i < a.len() || j < b.len() {
            if i < a.len() && j < b.len() && a[i] == b[j] {
                i += 1;
                j += 1;
            } else if j < b.len() && (i == a.len() || lcs[i][j + 1] >= lcs[i + 1][j]) {
                added.push(b[j].clone());
                j += 1;
            } else {
                removed.push(a[i].clone());
                i += 1;
            }
        }
        (removed, added)
    }

    fn unfold(line: &str) -> String {
        line.replace("\r\n ", "").replace("\n ", "").trim_end().to_string()
    }

    fn structured_location(text: &str) -> String {
        lines(text).into_iter().find(|l| l.starts_with("X-APPLE-STRUCTURED-LOCATION")).unwrap()
    }

    #[test]
    fn every_fixture_round_trips_byte_for_byte() {
        for (name, text) in FIXTURES {
            assert_eq!(Object::parse(text).to_string(), *text, "{name}");
        }
        for odd in [
            "",
            "\r\n\r\n",
            "BEGIN:VCALENDAR\nX-A:1\r\n folded\r\nEND:VCALENDAR",
            "END:VEVENT\nBEGIN:VEVENT\n\tX:tab-folded\nBEGIN:VALARM\nEND:VEVENT\n",
            "no colon\n;;;\n:\nX;P=\"unterminated:quote\n",
        ] {
            assert_eq!(Object::parse(odd).to_string(), odd);
        }
        let bom = "\u{feff}BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:b\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(Object::parse(bom).to_string(), bom);
        assert!(Object::parse(bom).event("b", None).is_some());
    }

    #[test]
    fn answering_changes_only_that_attendee_line() {
        let text = fixture!("icloud.ics");
        let mut obj = Object::parse(text);
        let jan = |p: &Prop| p.address().is_some_and(|a| a.eq_ignore_ascii_case("Jan.Peeters@example.com"));
        let matched = obj.event_mut(ICLOUD_UID, None).unwrap().update("ATTENDEE", jan, |p| {
            p.set_param("PARTSTAT", "ACCEPTED");
            p.remove_param("RSVP");
        });
        assert_eq!(matched, 1);
        let out = obj.to_string();

        let (removed, added) = diff(text, &out);
        assert_eq!(removed.len(), 1);
        assert_eq!(added.len(), 1);
        assert!(unfold(&removed[0]).contains("PARTSTAT=NEEDS-ACTION"));
        assert_eq!(
            unfold(&added[0]),
            "ATTENDEE;CN=\"Jan Peeters\";CUTYPE=INDIVIDUAL;EMAIL=\"jan.peeters@example.com\";\
             PARTSTAT=ACCEPTED;ROLE=REQ-PARTICIPANT;SCHEDULE-STATUS=1.2:mailto:jan.peeters@example.com"
        );
        assert!(added[0].ends_with("\r\n") && added[0].split("\r\n").all(|l| l.len() <= 75));
        assert_eq!(structured_location(&out), structured_location(text));
        assert!(structured_location(&out).contains("geo:50.846777,4.352360"));

        let jan = &read_events(&out)[0].attendees[1];
        assert_eq!(jan.response, Response::Accepted);

    }

    #[test]
    fn setting_a_value_already_there_rewrites_nothing() {
        for (text, uid) in [(fixture!("outlook-series.ics"), OUTLOOK_UID), (fixture!("icloud.ics"), ICLOUD_UID)] {
            let mut obj = Object::parse(text);
            let ev = obj.event_mut(uid, None).unwrap();
            let summary = ev.prop("SUMMARY").unwrap().value().to_string();
            ev.set("SUMMARY", &summary);
            ev.update("ATTENDEE", |p| p.param("PARTSTAT").is_some(), |p| {
                let partstat = p.param("PARTSTAT").unwrap().to_string();
                p.set_param("PARTSTAT", &partstat);
            });
            assert_eq!(obj.to_string(), text);
        }
    }

    #[test]
    fn deleting_one_occurrence_adds_an_exdate_in_dtstart_form() {
        let text = fixture!("outlook-series.ics");
        let mut obj = Object::parse(text);
        obj.event_mut(OUTLOOK_UID, None).unwrap().add_exdate("20261201T100000");
        let out = obj.to_string();
        let (removed, added) = diff(text, &out);
        assert_eq!(removed, Vec::<String>::new());
        assert_eq!(added, ["EXDATE;TZID=Romance Standard Time:20261201T100000\r\n"]);
        let paris = Zone::Tz(Tz::Europe__Paris);
        let exdates = read_events(&out)[0].recurrence.clone().unwrap().exdates;
        assert_eq!(exdates, [Moment::Time(at("2026-11-24 10:00", paris)), Moment::Time(at("2026-12-01 10:00", paris))]);

        assert!(obj.remove_event(OUTLOOK_UID, Some("20261117T100000")));
        assert!(!obj.remove_event(OUTLOOK_UID, Some("20261117T100000")));
        let out = obj.to_string();
        let (removed, added) = diff(text, &out);
        assert_eq!(added, ["EXDATE;TZID=Romance Standard Time:20261201T100000\r\n"]);
        assert_eq!(removed.first().map(String::as_str), Some("BEGIN:VEVENT\r\n"));
        assert_eq!(removed.last().map(String::as_str), Some("END:VEVENT\r\n"));
        assert_eq!(read_events(&out).len(), 1);
    }

    #[test]
    fn an_override_goes_after_the_master() {
        let text = fixture!("icloud.ics");
        let mut obj = Object::parse(text);
        let mut moved = obj.event(ICLOUD_UID, None).unwrap().clone();
        moved.remove("RRULE");
        moved.add(Prop::moment("RECURRENCE-ID", &Moment::Time(at("2026-11-13 12:00", Zone::Tz(Tz::Europe__Brussels)))));
        moved.set("DTSTART", "20261113T140000");
        moved.set("DTEND", "20261113T150000");
        moved.set_text("SUMMARY", "Lunch, later; with Jan");
        assert!(obj.add_after_master(moved));
        let out = obj.to_string();

        let (removed, added) = diff(text, &out);
        assert_eq!(removed, Vec::<String>::new(), "the master and everything around it is untouched");
        assert!(added.contains(&"SUMMARY:Lunch\\, later\\; with Jan\r\n".to_string()));
        assert!(added.contains(&"RECURRENCE-ID;TZID=Europe/Brussels:20261113T120000\r\n".to_string()));
        assert!(out.find("RECURRENCE-ID").unwrap() > out.find("RRULE").unwrap());
        assert!(out.ends_with("END:VEVENT\r\nEND:VCALENDAR\r\n"));

        let events = read_events(&out);
        assert_eq!(events.len(), 2);
        let brussels = Zone::Tz(Tz::Europe__Brussels);
        assert_eq!(events[1].recurrence_id, Some(Moment::Time(at("2026-11-13 12:00", brussels))));
        assert_eq!(events[1].span, Span::Timed { start: at("2026-11-13 14:00", brussels), end: at("2026-11-13 15:00", brussels) });
        assert_eq!(events[1].title, "Lunch, later; with Jan");
        assert_eq!(events[1].recurrence, None);
        assert!(obj.event(ICLOUD_UID, Some("20261113T120000")).is_some());
    }

    #[test]
    fn long_new_lines_fold_on_character_boundaries() {
        let text = fixture!("google.ics");
        let long = "Één café ☕ voor 👩‍💻 iedereen; ook in Brussel, België.\n".repeat(6);
        let mut obj = Object::parse(text);
        obj.event_mut("5f2k9q8v1n3m7c0b4x6z2a8d1e@google.com", None).unwrap().set_text("DESCRIPTION", &long);
        let out = obj.to_string();
        for physical in out.split("\r\n") {
            assert!(physical.len() <= 75, "{physical:?} is {} octets", physical.len());
        }
        let (removed, added) = diff(text, &out);
        assert_eq!((removed.len(), added.len()), (1, 1));
        assert!(added[0].lines().count() > 5);
        assert_eq!(Object::parse(&out).event("5f2k9q8v1n3m7c0b4x6z2a8d1e@google.com", None).unwrap().prop("DESCRIPTION").unwrap().text(), long);
        assert_eq!(read_events(&out)[0].description, long);

        assert_eq!(fold(&"a".repeat(75), "\r\n"), "a".repeat(75));
        assert_eq!(fold(&"a".repeat(76), "\r\n"), format!("{}\r\n a", "a".repeat(75)));
        assert_eq!(fold(&format!("{}é", "a".repeat(74)), "\n"), format!("{}\n é", "a".repeat(74)));
    }

    #[test]
    fn new_lines_follow_the_text_line_endings() {
        let text = fixture!("holidays.ics");
        let mut obj = Object::parse(text);
        obj.event_mut("2026-11-01-allerheiligen@holidays.example.org", None).unwrap().set_text("LOCATION", "België");
        let out = obj.to_string();
        assert!(!out.contains('\r'));
        assert_eq!(diff(text, &out).1, ["LOCATION:België\n"]);

        let text = fixture!("malformed.ics");
        let mut obj = Object::parse(text);
        obj.event_mut("unterminated@example.org", None).unwrap().set_text("LOCATION", "Gent");
        assert!(obj.to_string().ends_with("SUMMARY:Never ends\r\nLOCATION:Gent\r\n"));
        assert!(Object::parse(text).closed().unwrap().ends_with("SUMMARY:Never ends\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"));
        assert_eq!(Object::parse(fixture!("zoom.ics")).closed(), None);
    }

    #[test]
    fn text_escaping() {
        let text = "a\\b;c,d\ne:f";
        assert_eq!(escape(text), "a\\\\b\\;c\\,d\\ne:f");
        assert_eq!(unescape(&escape(text)), text);
        assert_eq!(unescape("x\\Ny\\"), "x\ny\\");
    }

    fn person(address: &str, name: &str) -> Person {
        Person { address: address.into(), name: name.into() }
    }

    fn attendee(address: &str, name: &str, role: Role, response: Response) -> Attendee {
        Attendee { person: person(address, name), role, response, comment: String::new(), is_me: address == ME }
    }

    fn event(uid: &str, span: Span) -> EventData {
        EventData {
            uid: uid.into(),
            recurrence_id: None,
            this_and_future: false,
            span,
            recurrence: None,
            title: String::new(),
            location: String::new(),
            description: String::new(),
            description_html: false,
            url: None,
            status: EventStatus::Confirmed,
            show_as: ShowAs::Busy,
            visibility: Visibility::Default,
            organizer: None,
            attendees: Vec::new(),
            sequence: 0,
            alarms: Vec::new(),
            meeting: None,
            categories: Vec::new(),
            color: None,
            master_id: None,
            options: Default::default(),
            organizer_is_me: false,
            video: None,
        }
    }

    fn round_trip(e: &EventData) {
        let text = write_event(e, "-//Jolta//Jolta 0.3//EN");
        assert!(text.split("\r\n").all(|l| l.len() <= 75), "{text}");
        assert_eq!(Object::parse(&text).to_string(), text);
        let read = EventData { organizer_is_me: e.organizer.as_ref().is_some_and(|o| o.address == ME), ..e.clone() };
        assert_eq!(read_events(&text), [read], "{text}");
    }

    #[test]
    fn written_events_read_back_the_same() {
        let brussels = Zone::Tz(Tz::Europe__Brussels);
        let mut timed = event(
            "meeting-1@jolta.example.org",
            Span::Timed { start: at("2026-11-05 10:00", brussels), end: at("2026-11-05 11:00", brussels) },
        );
        timed.title = "Planning, Q4; deel 2".into();
        timed.location = "Vergaderzaal 3\nKantoor Gent".into();
        timed.description = "Agenda:\n1. Cijfers\n2. Vragen, opmerkingen; varia \\ rest".into();
        timed.url = Some("https://example.com/meeting?a=1&b=2".into());
        timed.status = EventStatus::Tentative;
        timed.show_as = ShowAs::Tentative;
        timed.visibility = Visibility::Private;
        timed.organizer = Some(person("sofie@example.com", "Sofie Janssens"));
        timed.attendees = vec![
            attendee(ME, "Koen Test", Role::Required, Response::NeedsAction),
            attendee("sofie@example.com", "Sofie Janssens", Role::Chair, Response::Accepted),
            Attendee {
                comment: "Sorry: op vakantie, volgende keer".into(),
                ..attendee("jan@example.com", "Jan Peeters", Role::Optional, Response::Declined)
            },
            attendee("zaal3@example.com", "Zaal 3", Role::Room, Response::Accepted),
            attendee("beamer@example.com", "Beamer", Role::Resource, Response::Tentative),
            attendee("info@example.com", "", Role::Fyi, Response::Delegated),
        ];
        timed.sequence = 4;
        timed.alarms = vec![
            Alarm { trigger: AlarmTrigger::MinutesBefore(15), email: false },
            Alarm { trigger: AlarmTrigger::MinutesBefore(-5), email: false },
            Alarm { trigger: AlarmTrigger::At(DateTime::from_timestamp(1_793_869_200, 0).unwrap()), email: true },
        ];
        timed.categories = vec!["Werk".into(), "Q4, planning".into()];
        round_trip(&timed);

        let day = |s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let mut all_day = event("holiday-1@jolta.example.org", Span::AllDay { start: day("2026-12-24"), end: day("2026-12-27") });
        all_day.title = "Kerst 🎄 bij de familie Vandenbroucke-Verstraete in Sint-Martens-Latem".into();
        all_day.description = "<p>Breng <b>dessert</b> mee &amp; drankjes</p>".into();
        all_day.description_html = true;
        all_day.show_as = ShowAs::Free;
        all_day.visibility = Visibility::Public;
        all_day.color = Some("darkgreen".into());
        all_day.alarms = vec![Alarm { trigger: AlarmTrigger::MinutesBefore(-9 * 60), email: false }];
        round_trip(&all_day);
        assert!(write_event(&all_day, "-//Jolta//EN").contains("DESCRIPTION:Breng dessert mee & drankjes\r\n"));

        let utc = Zone::Tz(Tz::UTC);
        let mut series = event(
            "standup@jolta.example.org",
            Span::Timed { start: at("2026-11-03 08:30", utc), end: at("2026-11-03 08:45", utc) },
        );
        series.title = "Standup".into();
        series.recurrence = Some(Recurrence {
            rules: vec!["FREQ=WEEKLY;UNTIL=20261231T235959Z;INTERVAL=2;BYDAY=TU,TH;WKST=MO".into()],
            rdates: vec![Moment::Time(at("2026-11-07 09:00", brussels))],
            exdates: vec![Moment::Time(at("2026-11-05 08:30", utc)), Moment::Time(at("2026-11-17 08:30", utc))],
        });
        series.organizer = Some(person(ME, "Koen Test"));
        series.attendees = vec![
            attendee(ME, "Koen Test", Role::Chair, Response::Accepted),
            attendee("team@example.com", "Team", Role::Required, Response::NeedsAction),
        ];
        series.alarms = vec![Alarm { trigger: AlarmTrigger::MinutesBefore(0), email: false }];
        round_trip(&series);

        let floating = Zone::Floating;
        let mut moved = event(
            "standup@jolta.example.org",
            Span::Timed { start: at("2026-11-12 09:00", floating), end: at("2026-11-12 09:30", floating) },
        );
        moved.recurrence_id = Some(Moment::Time(at("2026-11-12 08:30", floating)));
        moved.title = "Standup (later)".into();
        moved.sequence = 1;
        round_trip(&moved);

        let mut on_a_day = moved.clone();
        on_a_day.recurrence_id = Some(Moment::Date(day("2026-11-12")));
        round_trip(&on_a_day);
    }
}
