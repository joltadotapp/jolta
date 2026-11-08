//! Online meetings: find the join link, meeting id, passcode and dial-in
//! numbers in an event.

use super::caps::{self, Capabilities};
use super::{Calendar, Meeting, MeetingProvider};
use Host::*;
use Shape::*;
use MeetingProvider as P;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Video,
    Phone,
}

/// A structured conference entry: Graph `onlineMeeting`, Google
/// `conferenceData.entryPoints`, iCalendar `CONFERENCE`,
/// `X-MICROSOFT-SKYPETEAMSMEETINGURL` or `X-GOOGLE-CONFERENCE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub uri: String,
    pub meeting_id: Option<String>,
    pub passcode: Option<String>,
}

/// The known meeting links. Adding a provider is one row here plus one case
/// in `every_row_is_recognised`.
static ROWS: &[Row] = &[
    Row::new(P::Zoom, Domain("zoom.us"), Prefix("/j/")).id().passcode("pwd").app(App::Zoom),
    Row::new(P::Zoom, Domain("zoom.us"), Prefix("/w/")).id().passcode("pwd").app(App::Zoom),
    Row::new(P::Zoom, Domain("zoom.us"), Prefix("/my/")).app(App::Zoom),
    Row::new(P::Zoom, Domain("zoomgov.com"), Prefix("/j/")).id().passcode("pwd").app(App::Zoom),
    Row::new(P::Teams, Exact("teams.microsoft.com"), Prefix("/l/meetup-join/")).app(App::Teams),
    Row::new(P::Teams, Exact("teams.microsoft.com"), Prefix("/meet/")).id().passcode("p").app(App::Teams),
    Row::new(P::Teams, Exact("teams.live.com"), Prefix("/meet/")).id().passcode("p"),
    Row::new(P::Meet, Exact("meet.google.com"), MeetCode),
    Row::new(P::Meet, Exact("meet.google.com"), Prefix("/lookup/")),
    Row::new(P::Webex, Domain("webex.com"), Prefix("/meet/")),
    Row::new(P::Webex, Domain("webex.com"), Query("/j.php", "MTID")),
    Row::new(P::GoTo, Exact("meet.goto.com"), Room).id(),
    Row::new(P::GoTo, Exact("global.gotomeeting.com"), Prefix("/join/")).id(),
    Row::new(P::Whereby, Exact("whereby.com"), Room),
    Row::new(P::Jitsi, Exact("meet.jit.si"), Room),
    Row::new(P::Chime, Exact("chime.aws"), Room).id(),
    Row::new(P::Slack, Exact("app.slack.com"), Prefix("/huddle/")),
    Row::new(P::FaceTime, Exact("facetime.apple.com"), Fragment("/join")),
    Row::new(P::RingCentral, Exact("meetings.ringcentral.com"), Prefix("/j/")).id(),
    Row::new(P::RingCentral, Exact("v.ringcentral.com"), Prefix("/join/")).id(),
    Row::new(P::Zoho, AnyTld("meeting.zoho."), Prefix("/")),
];

/// One entry of the editor's Add video call menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoChoice {
    pub provider: MeetingProvider,
    pub label: &'static str,
    /// Why the calendar can't have this call, shown greyed in the menu;
    /// None when it can.
    pub refusal: Option<String>,
}

/// A video call the editor offers and when a calendar can have it. Adding
/// one, such as a personal Zoom link, is one row here.
struct VideoRow {
    provider: MeetingProvider,
    label: &'static str,
    refusal: fn(&Calendar, &Capabilities) -> Option<String>,
}

static VIDEO_ROWS: &[VideoRow] = &[
    VideoRow { provider: P::Teams, label: "Teams meeting", refusal: |c, caps| made_by_server(P::Teams, c, caps) },
    VideoRow { provider: P::Meet, label: "Google Meet", refusal: |c, caps| made_by_server(P::Meet, c, caps) },
];

/// Why the server of `calendar` won't make `provider`'s meeting on save,
/// which only a calendar of the provider's own account does.
fn made_by_server(provider: MeetingProvider, calendar: &Calendar, caps: &Capabilities) -> Option<String> {
    if caps.video.contains(&provider) {
        return None;
    }
    let name = super::draft::provider(calendar);
    Some(match caps::own_video(calendar.source) == Some(provider) {
        true => format!("{name} doesn't allow it on this calendar"),
        false => format!("Not on {name} calendars"),
    })
}

/// The Add video call menu for `calendar`: the calls it can have, its
/// account's own first (Teams on Microsoft 365, Meet on Google), then the
/// others greyed with the reason. Empty when it can have none, as on a
/// read-only calendar, iCloud or a calendar on this Mac, so the editor
/// leaves the button out.
pub fn video_choices(calendar: &Calendar) -> Vec<VideoChoice> {
    let caps = caps::of(calendar);
    if !caps.writable {
        return Vec::new();
    }
    let mut choices: Vec<VideoChoice> =
        VIDEO_ROWS.iter().map(|r| VideoChoice { provider: r.provider, label: r.label, refusal: (r.refusal)(calendar, &caps) }).collect();
    if choices.iter().all(|c| c.refusal.is_some()) {
        return Vec::new();
    }
    let own = caps::own_video(calendar.source);
    choices.sort_by_key(|c| (c.refusal.is_some(), Some(c.provider) != own));
    choices
}

const ID_LABELS: &[&str] = &["meeting id", "meeting number", "webinar id"];
const PASSCODE_LABELS: &[&str] = &["passcode", "password"];
/// The code dialled after the number. For Teams it differs from the meeting id.
const DIAL_LABELS: &[&str] = &["conference id", "access code", "participant code"];

struct Row {
    provider: MeetingProvider,
    host: Host,
    shape: Shape,
    /// The path segment the shape matched is the meeting id.
    id: bool,
    /// The query parameter that holds the passcode.
    passcode: Option<&'static str>,
    app: App,
}

impl Row {
    const fn new(provider: MeetingProvider, host: Host, shape: Shape) -> Row {
        Row { provider, host, shape, id: false, passcode: None, app: App::Browser }
    }

    const fn id(mut self) -> Row {
        self.id = true;
        self
    }

    const fn passcode(mut self, key: &'static str) -> Row {
        self.passcode = Some(key);
        self
    }

    const fn app(mut self, app: App) -> Row {
        self.app = app;
        self
    }
}

#[derive(Clone, Copy)]
enum Host {
    Exact(&'static str),
    /// The domain and every host under it.
    Domain(&'static str),
    /// The prefix followed by a top-level domain such as `com` or `com.au`.
    AnyTld(&'static str),
}

impl Host {
    fn matches(self, host: &str) -> bool {
        match self {
            Exact(h) => host == h,
            Domain(d) => host == d || host.strip_suffix(d).is_some_and(|sub| sub.ends_with('.')),
            AnyTld(prefix) => host.strip_prefix(prefix).is_some_and(|tld| {
                tld.split('.').all(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_lowercase()))
            }),
        }
    }
}

#[derive(Clone, Copy)]
enum Shape {
    /// The prefix and a non-empty segment after it.
    Prefix(&'static str),
    /// A single path segment: a room name or meeting number.
    Room,
    /// Google Meet's `abc-defg-hij`.
    MeetCode,
    /// A path ending in the suffix, with the key in the query.
    Query(&'static str, &'static str),
    /// Exactly this path, with the meeting in the fragment.
    Fragment(&'static str),
}

impl Shape {
    /// The segment that identifies the meeting, when the link has this shape.
    fn matches<'a>(self, link: &Link<'a>) -> Option<&'a str> {
        let segment = || {
            let rest = &link.path[1..];
            let seg = rest.strip_suffix('/').unwrap_or(rest);
            (!seg.is_empty() && !seg.contains('/')).then_some(seg)
        };
        match self {
            Prefix(prefix) => link.path.strip_prefix(prefix)?.split('/').next().filter(|s| !s.is_empty()),
            Room => segment(),
            MeetCode => segment().filter(|seg| {
                let parts: Vec<&str> = seg.split('-').collect();
                parts.iter().map(|p| p.len()).eq([3, 4, 3])
                    && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_lowercase()))
            }),
            Query(suffix, key) => {
                (link.path.ends_with(suffix) && param(link.query, key).is_some_and(|v| !v.is_empty())).then_some(link.path)
            }
            Fragment(path) => (link.path == path && !link.fragment.is_empty()).then_some(link.fragment),
        }
    }
}

#[derive(Clone, Copy)]
enum App {
    Browser,
    Zoom,
    Teams,
}

impl App {
    fn url(self, link: &Link, m: &Meeting) -> Option<String> {
        match self {
            App::Browser => None,
            App::Zoom => {
                let id = m.meeting_id.as_deref()?;
                let mut url = format!("zoommtg://{}/join?confno={id}", link.host);
                if let Some(pwd) = param(link.query, "pwd").or(m.passcode.as_deref()) {
                    url.push_str("&pwd=");
                    url.push_str(pwd);
                }
                Some(url)
            }
            App::Teams => {
                let mut url = format!("msteams:{}", link.path);
                for (sep, part) in [('?', link.query), ('#', link.fragment)] {
                    if !part.is_empty() {
                        url.push(sep);
                        url.push_str(part);
                    }
                }
                Some(url)
            }
        }
    }
}

/// An https URL split into the parts the table matches on.
struct Link<'a> {
    /// Lowercase, without user info or port.
    host: String,
    /// Starts with `/`.
    path: &'a str,
    query: &'a str,
    fragment: &'a str,
}

fn parse(url: &str) -> Option<Link<'_>> {
    let rest = url.get(..8).filter(|s| s.eq_ignore_ascii_case("https://")).map(|_| &url[8..])?;
    // A backslash ends the host for browsers, so `evil.example\@meet.google.com` is evil.example.
    let end = rest.find(['/', '?', '#', '\\']).unwrap_or(rest.len());
    let host = rest[..end].rsplit('@').next()?.split(':').next()?;
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.') {
        return None;
    }
    let rest = &rest[end..];
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Some(Link { host, path: if path.is_empty() { "/" } else { path }, query, fragment })
}

fn known<'a>(link: &Link<'a>) -> Option<(&'static Row, &'a str)> {
    ROWS.iter()
        .filter(|row| row.host.matches(&link.host))
        .find_map(|row| Some((row, row.shape.matches(link)?)))
}

fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
}

/// Decodes `&amp;` and unwraps Outlook Safe Links; None unless the result is
/// an https URL.
fn clean(raw: &str) -> Option<String> {
    let url = raw.trim().replace("&amp;", "&");
    let link = parse(&url)?;
    if Domain("safelinks.protection.outlook.com").matches(&link.host)
        && let Some(inner) = param(link.query, "url")
    {
        return clean(&percent_decode(inner));
    }
    Some(url)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let hex = |at: usize| bytes.get(at).and_then(|b| (*b as char).to_digit(16));
    while i < bytes.len() {
        match (bytes[i], hex(i + 1), hex(i + 2)) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every `https://` run in free text, without wrapping brackets or trailing
/// punctuation.
fn https_runs(text: &str) -> Vec<&str> {
    let lower = text.to_ascii_lowercase();
    let mut runs = Vec::new();
    let mut from = 0;
    while let Some(i) = lower[from..].find("https://") {
        let start = from + i;
        let len = text[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`'))
            .unwrap_or(text.len() - start);
        runs.push(text[start..start + len].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '*']));
        from = start + len;
    }
    runs
}

/// A cleaned https link and what the provider table knows about it.
#[derive(Clone)]
struct Hit {
    url: String,
    /// None for a host no row knows.
    provider: Option<MeetingProvider>,
    meeting_id: Option<String>,
    passcode: Option<String>,
}

fn hit(raw: &str) -> Option<Hit> {
    let url = clean(raw)?;
    let (provider, meeting_id, passcode) = {
        let link = parse(&url)?;
        match known(&link) {
            Some((row, segment)) => (
                Some(row.provider),
                row.id.then(|| segment.to_string()),
                row.passcode.and_then(|key| param(link.query, key)).filter(|p| !p.is_empty()).map(str::to_string),
            ),
            None => (None, None, None),
        }
    };
    Some(Hit { url, provider, meeting_id, passcode })
}

/// Known meeting links in free text, deduplicated, in order.
fn meeting_links(text: &str) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    for found in https_runs(text).into_iter().filter_map(hit).filter(|h| h.provider.is_some()) {
        if !hits.iter().any(|h| h.url == found.url) {
            hits.push(found);
        }
    }
    hits
}

/// Looks at structured entries first, then the event's URL and location,
/// then known meeting links in the description (https only).
pub fn detect(entries: &[Entry], url: Option<&str>, location: &str, description: &str) -> Option<Meeting> {
    let described = meeting_links(description);
    let (chosen, entry) = from_entries(entries)
        .or_else(|| url.and_then(hit).filter(|h| h.provider.is_some()).map(|h| (h, None)))
        .or_else(|| from_location(location).map(|h| (h, None)))
        .or_else(|| described.first().map(|h| (h.clone(), None)))?;

    let provider = chosen.provider.unwrap_or(P::Other);
    let text = if provider == P::Other { String::new() } else { plain_text(description) };
    let labelled = |labels, value: fn(&str) -> Option<String>| after_label(&text, labels).and_then(value);

    let meeting_id = entry
        .and_then(|e| e.meeting_id.clone())
        .or_else(|| labelled(ID_LABELS, digits))
        .or(chosen.meeting_id);
    // Zoom's `pwd=` is an encoded form; the labelled passcode is what a person types.
    let passcode = entry
        .and_then(|e| e.passcode.clone())
        .or_else(|| labelled(PASSCODE_LABELS, token))
        .or(chosen.passcode);
    let dial_code = labelled(DIAL_LABELS, digits).or_else(|| meeting_id.clone());

    let mut phones: Vec<String> = Vec::new();
    let structured = entries.iter().filter(|e| e.kind == EntryKind::Phone).filter_map(|e| {
        let pin = e.passcode.as_deref().or(e.meeting_id.as_deref()).or(dial_code.as_deref());
        tel(&e.uri, pin)
    });
    for phone in structured.chain(one_tap_numbers(description)) {
        if !phones.contains(&phone) {
            phones.push(phone);
        }
    }

    let other_links = described.into_iter().map(|h| h.url).filter(|u| *u != chosen.url).collect();
    Some(Meeting { provider, join_url: chosen.url, meeting_id, passcode, phones, other_links })
}

/// The first video entry with a known provider, else the first video entry.
/// The link alone names the provider: a property that calls any link Google
/// Meet would otherwise lend it a trusted Join button.
fn from_entries(entries: &[Entry]) -> Option<(Hit, Option<&Entry>)> {
    let videos: Vec<(Hit, &Entry)> = entries
        .iter()
        .filter(|e| e.kind == EntryKind::Video)
        .filter_map(|e| Some((hit(&e.uri)?, e)))
        .collect();
    let best = videos.iter().position(|(h, _)| h.provider.is_some()).unwrap_or(0);
    videos.into_iter().nth(best).map(|(h, e)| (h, Some(e)))
}

/// A location that is only a URL is the meeting even for an unknown host;
/// otherwise only known meeting links in it count.
fn from_location(location: &str) -> Option<Hit> {
    let location = location.trim();
    if !location.contains(char::is_whitespace)
        && let Some(h) = hit(location)
    {
        return Some(h);
    }
    meeting_links(location).into_iter().next()
}

/// Turns HTML into lines of text, enough to read labelled values.
pub(crate) fn plain_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else { break };
        let tag = rest[open + 1..open + close].trim_start_matches('/');
        let name = tag.split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or("");
        if ["br", "p", "div", "tr", "li", "table"].iter().any(|t| name.eq_ignore_ascii_case(t)) {
            out.push('\n');
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out.replace("&nbsp;", " ").replace("&amp;", "&")
}

/// The rest of the line after `label:` or `label (note):`.
fn after_label<'a>(text: &'a str, labels: &[&str]) -> Option<&'a str> {
    let lower = text.to_ascii_lowercase();
    labels.iter().find_map(|label| {
        lower.match_indices(label).find_map(|(i, _)| {
            let rest = text[i + label.len()..].trim_start();
            let rest = match rest.strip_prefix('(') {
                Some(note) => note.split_once(')')?.1.trim_start(),
                None => rest,
            };
            rest.strip_prefix(':')?.trim_start().lines().next()
        })
    })
}

/// The digits of a leading `123 456 789` group.
fn digits(value: &str) -> Option<String> {
    let group = value.split(|c: char| !(c.is_ascii_digit() || c.is_whitespace())).next()?;
    let digits: String = group.chars().filter(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

fn token(value: &str) -> Option<String> {
    value.split_whitespace().next().map(str::to_string)
}

/// Dial strings such as `+32 2 588 63 12,,123456789#` in free text. Plain
/// numbers without a code are skipped: they could be anyone's phone.
fn one_tap_numbers(text: &str) -> Vec<String> {
    text.match_indices('+')
        .filter_map(|(i, _)| {
            let rest = &text[i + 1..];
            let number = rest.find(|c: char| !(c.is_ascii_digit() || c == ' ' || c == '-')).unwrap_or(rest.len());
            let after = rest[number..].strip_prefix(",,")?;
            let code = after.find(|c: char| !(c.is_ascii_digit() || matches!(c, ',' | '#' | '*'))).unwrap_or(after.len());
            tel(&text[i..i + 1 + number + 2 + code], None)
        })
        .collect()
}

/// A `tel:` URI with pauses: `tel:+3225886312,,123456789#`. A number that
/// carries no code gets `code`.
fn tel(raw: &str, code: Option<&str>) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("tel:").unwrap_or(raw);
    let raw = raw.split(';').next().unwrap_or(raw);
    let (number, tail) = raw.split_at(raw.find(',').unwrap_or(raw.len()));
    let digits: String = number.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 7 {
        return None;
    }
    let plus = if number.trim_start().starts_with('+') { "+" } else { "" };
    let tail: String = tail.chars().filter(|c| c.is_ascii_digit() || matches!(c, ',' | '#' | '*')).collect();
    let tail = tail.trim_end_matches(',');
    let tail = match (tail, code.and_then(digits_only)) {
        ("", Some(code)) => format!(",,{code}#"),
        (tail, _) => tail.to_string(),
    };
    Some(format!("tel:{plus}{digits}{tail}"))
}

fn digits_only(s: &str) -> Option<String> {
    let d: String = s.chars().filter(char::is_ascii_digit).collect();
    (!d.is_empty()).then_some(d)
}

/// A link that opens the provider's own app, for providers that document a
/// scheme. The app checks whether that app is installed.
pub fn app_url(m: &Meeting) -> Option<String> {
    let link = parse(&m.join_url)?;
    let (row, _) = known(&link)?;
    row.app.url(&link, m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{Access, CalendarId, CalendarSource};

    fn in_description(description: &str) -> Option<Meeting> {
        detect(&[], None, "", description)
    }

    fn video(uri: &str) -> Entry {
        Entry { kind: EntryKind::Video, uri: uri.into(), meeting_id: None, passcode: None }
    }

    fn calendar(source: CalendarSource, access: Access, video: Option<Vec<MeetingProvider>>) -> Calendar {
        Calendar {
            id: CalendarId(1),
            account_id: None,
            source,
            remote_id: "x".into(),
            name: "Work".into(),
            color: None,
            zone: None,
            access,
            shown: true,
            counts_as_busy: true,
            notify: true,
            default_reminder: None,
            is_default: false,
            cursor: None,
            refresh: None,
            synced: None,
            video,
        }
    }

    fn menu(c: &Calendar) -> Vec<(MeetingProvider, Option<String>)> {
        video_choices(c).into_iter().map(|v| (v.provider, v.refusal)).collect()
    }

    /// The mockup's menu on Microsoft 365, and Google's the other way round.
    #[test]
    fn the_accounts_own_video_call_comes_first() {
        use Access::*;
        let m365 = calendar(CalendarSource::Graph, Owner, Some(vec![P::Teams]));
        assert_eq!(menu(&m365), [(P::Teams, None), (P::Meet, Some("Not on Microsoft 365 calendars".into()))]);
        assert_eq!(video_choices(&m365)[0].label, "Teams meeting");
        let google = calendar(CalendarSource::Google, Write, None);
        assert_eq!(menu(&google), [(P::Meet, None), (P::Teams, Some("Not on Google calendars".into()))], "a list read before U8 allows Meet");
    }

    #[test]
    fn a_calendar_that_can_make_no_call_offers_none() {
        use Access::*;
        for c in [
            calendar(CalendarSource::Graph, Owner, Some(Vec::new())),
            calendar(CalendarSource::Google, Owner, Some(Vec::new())),
            calendar(CalendarSource::Graph, Read, None),
            calendar(CalendarSource::Google, FreeBusy, None),
            calendar(CalendarSource::CalDav, Owner, None),
            calendar(CalendarSource::Local, Owner, None),
            calendar(CalendarSource::Webcal, Owner, None),
        ] {
            assert!(video_choices(&c).is_empty(), "{:?} {:?} {:?}", c.source, c.access, c.video);
        }
    }

    #[test]
    fn every_row_is_recognised() {
        let cases = [
            ("https://us02web.zoom.us/j/85612345678?pwd=aBcDeFgHiJkLmNoPqRsTuVwXyZ.1", P::Zoom),
            ("https://acme.zoom.us/w/91234567890?tk=abc.DEF&pwd=xYz123", P::Zoom),
            ("https://zoom.us/my/jane.doe", P::Zoom),
            ("https://agency.zoomgov.com/j/1601234567", P::Zoom),
            (
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_NjA2ZTk4%40thread.v2/0?context=%7b%22Tid%22%3a%2272f988bf%22%2c%22Oid%22%3a%22d3b2%22%7d",
                P::Teams,
            ),
            ("https://teams.microsoft.com/meet/23612345678901?p=Ab3dEf9GhIj", P::Teams),
            ("https://teams.live.com/meet/9381234567890?p=XyZ12345", P::Teams),
            ("https://meet.google.com/abc-defg-hij", P::Meet),
            ("https://meet.google.com/lookup/ahgsbkg3ux", P::Meet),
            ("https://acme.webex.com/meet/jane.doe", P::Webex),
            ("https://acme.webex.com/acme/j.php?MTID=m0123456789abcdef0123456789abcdef", P::Webex),
            ("https://meet.goto.com/123456789", P::GoTo),
            ("https://global.gotomeeting.com/join/123456789", P::GoTo),
            ("https://whereby.com/acme-standup", P::Whereby),
            ("https://meet.jit.si/AcmeWeeklySync", P::Jitsi),
            ("https://chime.aws/1234567890", P::Chime),
            ("https://app.slack.com/huddle/T01234567/C0123456789", P::Slack),
            ("https://facetime.apple.com/join#v=1&p=AbCdEf123&k=XyZ", P::FaceTime),
            ("https://meetings.ringcentral.com/j/1491234567", P::RingCentral),
            ("https://v.ringcentral.com/join/123456789", P::RingCentral),
            ("https://meeting.zoho.eu/meeting/register?sessionId=1069853218", P::Zoho),
        ];
        let mut covered = vec![false; ROWS.len()];
        for (url, provider) in cases {
            let m = in_description(&format!("Join here: {url}\nSee you then.")).unwrap_or_else(|| panic!("{url}"));
            assert_eq!((m.provider, m.join_url.as_str()), (provider, url));
            let link = parse(url).unwrap();
            let (row, _) = known(&link).unwrap();
            covered[ROWS.iter().position(|r| std::ptr::eq(r, row)).unwrap()] = true;
        }
        assert!(covered.iter().all(|c| *c), "rows without a test case: {covered:?}");
    }

    #[test]
    fn zoom_link_gives_id_passcode_and_app_url() {
        let m = in_description("Join Zoom Meeting\nhttps://us02web.zoom.us/j/85612345678?pwd=aBcDeF.1\n").unwrap();
        assert_eq!(m.meeting_id.as_deref(), Some("85612345678"));
        assert_eq!(m.passcode.as_deref(), Some("aBcDeF.1"));
        assert_eq!(app_url(&m).as_deref(), Some("zoommtg://us02web.zoom.us/join?confno=85612345678&pwd=aBcDeF.1"));
    }

    #[test]
    fn zoom_labelled_passcode_is_shown_and_link_pwd_opens_the_app() {
        let m = in_description(
            "Join Zoom Meeting\nhttps://zoom.us/j/85612345678?pwd=aBcDeF.1\n\nMeeting ID: 856 1234 5678\nPasscode: 482913\n\n\
             One tap mobile\n+3225884188,,85612345678#,,,,*482913# Belgium\n",
        )
        .unwrap();
        assert_eq!(m.meeting_id.as_deref(), Some("85612345678"));
        assert_eq!(m.passcode.as_deref(), Some("482913"));
        assert_eq!(m.phones, ["tel:+3225884188,,85612345678#,,,,*482913#"]);
        assert_eq!(app_url(&m).as_deref(), Some("zoommtg://zoom.us/join?confno=85612345678&pwd=aBcDeF.1"));
    }

    #[test]
    fn teams_meet_link_with_passcode() {
        let m = in_description("https://teams.microsoft.com/meet/23612345678901?p=Ab3dEf9GhIj").unwrap();
        assert_eq!(m.provider, P::Teams);
        assert_eq!(m.meeting_id.as_deref(), Some("23612345678901"));
        assert_eq!(m.passcode.as_deref(), Some("Ab3dEf9GhIj"));
        assert_eq!(app_url(&m).as_deref(), Some("msteams:/meet/23612345678901?p=Ab3dEf9GhIj"));
    }

    #[test]
    fn teams_html_body_with_amp_and_dial_in() {
        let body = r#"<div><span>Microsoft Teams meeting</span><br>
<a href="https://teams.microsoft.com/l/meetup-join/19%3ameeting_NjA2%40thread.v2/0?context=%7b%22Tid%22%3a%2272f9%22%7d&amp;anon=true" target="_blank">Join the meeting now</a></div>
<div><span>Meeting ID: </span><span>236 123 456 789 01</span></div>
<div><span>Passcode: </span><span>Ab3dEf</span></div>
<div><a href="tel:+3225886312,,123456789#">+32 2 588 63 12,,123456789#</a> Belgium, Brussels</div>
<div><span>Phone conference ID: </span><span>123 456 789#</span></div>"#;
        let m = in_description(body).unwrap();
        assert_eq!(m.provider, P::Teams);
        assert_eq!(
            m.join_url,
            "https://teams.microsoft.com/l/meetup-join/19%3ameeting_NjA2%40thread.v2/0?context=%7b%22Tid%22%3a%2272f9%22%7d&anon=true"
        );
        assert_eq!(m.meeting_id.as_deref(), Some("23612345678901"));
        assert_eq!(m.passcode.as_deref(), Some("Ab3dEf"));
        assert_eq!(m.phones, ["tel:+3225886312,,123456789#"]);
        assert!(m.other_links.is_empty());
        assert_eq!(
            app_url(&m).as_deref(),
            Some("msteams:/l/meetup-join/19%3ameeting_NjA2%40thread.v2/0?context=%7b%22Tid%22%3a%2272f9%22%7d&anon=true")
        );
    }

    #[test]
    fn teams_plain_text_with_angle_brackets() {
        let m = in_description(
            "Join the meeting now<https://teams.microsoft.com/l/meetup-join/19%3ameeting_NjA2%40thread.v2/0?context=%7b%22Tid%22%7d>\n",
        )
        .unwrap();
        assert_eq!(m.join_url, "https://teams.microsoft.com/l/meetup-join/19%3ameeting_NjA2%40thread.v2/0?context=%7b%22Tid%22%7d");
    }

    #[test]
    fn safe_links_are_unwrapped() {
        let wrapped = "https://eur01.safelinks.protection.outlook.com/?url=https%3A%2F%2Fteams.microsoft.com%2Fl%2Fmeetup-join%2F19%253ameeting_NjA2%2540thread.v2%2F0%3Fcontext%3D%257b%2522Tid%2522%257d&amp;data=05%7C01%7Cjane%40acme.example%7C&amp;sdata=abc%3D&amp;reserved=0";
        let m = in_description(&format!("Join: <{wrapped}>")).unwrap();
        assert_eq!(m.provider, P::Teams);
        assert_eq!(m.join_url, "https://teams.microsoft.com/l/meetup-join/19%3ameeting_NjA2%40thread.v2/0?context=%7b%22Tid%22%7d");
    }

    #[test]
    fn google_meet_entry_with_phone_and_pin() {
        let entries = [
            video("https://meet.google.com/abc-defg-hij"),
            Entry {
                kind: EntryKind::Phone,
                uri: "tel:+1-650-555-0100".into(),
                meeting_id: None,
                passcode: Some("123 456 789".into()),
            },
        ];
        let m = detect(&entries, None, "", "").unwrap();
        assert_eq!((m.provider, m.join_url.as_str()), (P::Meet, "https://meet.google.com/abc-defg-hij"));
        assert_eq!(m.phones, ["tel:+16505550100,,123456789#"]);
    }

    #[test]
    fn known_entry_beats_unknown_entry() {
        let entries = [video("https://jitsi.acme.example/standup"), video("https://meet.google.com/abc-defg-hij")];
        let m = detect(&entries, None, "", "").unwrap();
        assert_eq!((m.provider, m.join_url.as_str()), (P::Meet, "https://meet.google.com/abc-defg-hij"));
    }

    #[test]
    fn unknown_entry_or_bare_location_url_is_other() {
        let m = detect(&[video("https://jitsi.acme.example/standup")], None, "", "").unwrap();
        assert_eq!((m.provider, m.join_url.as_str()), (P::Other, "https://jitsi.acme.example/standup"));

        let m = detect(&[], None, " https://jitsi.acme.example/standup ", "").unwrap();
        assert_eq!((m.provider, m.join_url.as_str()), (P::Other, "https://jitsi.acme.example/standup"));

        assert_eq!(detect(&[], None, "Room 4, see https://acme.example/rooms", ""), None);
        assert_eq!(detect(&[], Some("https://acme.example/events/42"), "", ""), None);
    }

    #[test]
    fn location_beats_description() {
        let m = detect(
            &[],
            None,
            "Zoom: https://zoom.us/j/85612345678",
            "Backup link: https://meet.google.com/abc-defg-hij",
        )
        .unwrap();
        assert_eq!((m.provider, m.join_url.as_str()), (P::Zoom, "https://zoom.us/j/85612345678"));
        assert_eq!(m.other_links, ["https://meet.google.com/abc-defg-hij"]);
    }

    #[test]
    fn url_field_beats_location() {
        let m = detect(&[], Some("https://meet.google.com/abc-defg-hij"), "https://zoom.us/j/85612345678", "").unwrap();
        assert_eq!(m.provider, P::Meet);
    }

    #[test]
    fn look_alikes_are_not_meetings() {
        for text in [
            "https://zoom.us/pricing",
            "https://teams.microsoft.com/",
            "http://zoom.us/j/85612345678",
            "https://meet.google.com.evil.example/abc-defg-hij",
            "https://evilzoom.us/j/123",
            "https://meet.google.com@evil.example/abc-defg-hij",
            "https://evil.example\\@meet.google.com/abc-defg-hij",
            "https://meet.google.com/abc-defg-hijk",
            "https://acme.example/agenda",
        ] {
            assert_eq!(in_description(&format!("Details: {text}.")), None, "{text}");
        }
    }

    #[test]
    fn other_links_are_deduplicated_meeting_links() {
        let m = in_description(
            "Join (https://zoom.us/j/85612345678).\n\
             Or Meet: https://meet.google.com/abc-defg-hij, again https://meet.google.com/abc-defg-hij\n\
             <a href=\"https://meet.google.com/abc-defg-hij\">Meet</a> https://zoom.us/j/85612345678\n\
             Agenda: https://docs.acme.example/agenda\n\
             Teams fallback: https://teams.live.com/meet/9381234567890?p=XyZ12345",
        )
        .unwrap();
        assert_eq!(m.join_url, "https://zoom.us/j/85612345678");
        assert_eq!(m.other_links, ["https://meet.google.com/abc-defg-hij", "https://teams.live.com/meet/9381234567890?p=XyZ12345"]);
    }

    #[test]
    fn providers_without_an_app_scheme_have_no_app_url() {
        let m = in_description("https://meet.google.com/abc-defg-hij").unwrap();
        assert_eq!(app_url(&m), None);
        let m = in_description("https://zoom.us/my/jane.doe").unwrap();
        assert_eq!(app_url(&m), None);
    }
}
