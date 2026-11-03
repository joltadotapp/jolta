//! WebDAV XML: a small namespace-aware tree and the 207 Multi-Status reply
//! (RFC 4918 section 13) that PROPFIND and REPORT answer with.

use quick_xml::NsReader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

use crate::{Error, Result};

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const CALENDARSERVER: &str = "http://calendarserver.org/ns/";
pub const APPLE: &str = "http://apple.com/ns/ical/";

/// An element with its namespace, local name, attributes, text and child
/// elements.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub ns: String,
    pub name: String,
    /// By local name, such as `name` in `<C:comp name="VEVENT"/>`.
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    fn open(ns: String, e: &quick_xml::events::BytesStart) -> Node {
        let attrs = e
            .attributes()
            .flatten()
            .filter_map(|a| Some((a.key.local_name().as_ref().to_string(), a.normalized_value(quick_xml::XmlVersion::default()).ok()?.into_owned())))
            .collect();
        Node { ns, name: e.local_name().as_ref().to_string(), attrs, ..Node::default() }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    pub fn child(&self, ns: &str, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.is(ns, name))
    }

    pub fn all<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.is(ns, name))
    }

    /// The trimmed text of a child, such as the href inside a property.
    pub fn child_text(&self, ns: &str, name: &str) -> Option<&str> {
        self.child(ns, name).map(|c| c.text.trim()).filter(|t| !t.is_empty())
    }
}

pub fn parse(xml: &str) -> Result<Node> {
    let mut reader = NsReader::from_str(xml);
    let mut stack: Vec<Node> = vec![Node::default()];
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(xml_error)?;
        let ns = match ns {
            ResolveResult::Bound(n) => n.0.to_string(),
            _ => String::new(),
        };
        match event {
            Event::Start(e) => stack.push(Node::open(ns, &e)),
            Event::Empty(e) => {
                let node = Node::open(ns, &e);
                stack.last_mut().expect("the root stays").children.push(node);
            }
            Event::End(_) => {
                let node = stack.pop().expect("the reader checks that tags match");
                let parent = stack.last_mut().ok_or_else(|| Error::Other("WebDAV reply: unbalanced XML".into()))?;
                parent.children.push(node);
            }
            Event::Text(t) => stack.last_mut().expect("the root stays").text.push_str(&t.xml10_content()),
            Event::CData(t) => stack.last_mut().expect("the root stays").text.push_str(&t.into_inner()),
            Event::GeneralRef(r) => {
                let name = r.xml10_content();
                let resolved = match r.resolve_char_ref().map_err(xml_error)? {
                    Some(c) => c.to_string(),
                    None => resolve_predefined_entity(&name).unwrap_or_default().to_string(),
                };
                stack.last_mut().expect("the root stays").text.push_str(&resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let root = stack.pop().filter(|_| stack.is_empty()).ok_or_else(|| Error::Other("WebDAV reply: unclosed XML".into()))?;
    root.children.into_iter().next().ok_or_else(|| Error::Other("WebDAV reply: no XML".into()))
}

fn xml_error(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("WebDAV reply: {e}"))
}

/// One `response` of a Multi-Status: a resource with its status, or with
/// properties grouped by the status the server gave each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub href: String,
    /// Set for a response about the whole resource, such as 404 for one
    /// that sync-collection reports as gone.
    pub status: Option<u16>,
    /// The properties the server returned with 200.
    pub props: Vec<Node>,
}

impl Response {
    pub fn prop(&self, ns: &str, name: &str) -> Option<&Node> {
        self.props.iter().find(|p| p.is(ns, name))
    }

    pub fn prop_text(&self, ns: &str, name: &str) -> Option<&str> {
        self.prop(ns, name).map(|p| p.text.trim()).filter(|t| !t.is_empty())
    }

    /// The href inside a property such as current-user-principal.
    pub fn prop_href(&self, ns: &str, name: &str) -> Option<&str> {
        self.prop(ns, name)?.child_text(DAV, "href")
    }
}

/// The responses of a Multi-Status reply, plus its top-level sync-token.
pub struct MultiStatus {
    pub responses: Vec<Response>,
    pub sync_token: Option<String>,
}

pub fn multistatus(xml: &str) -> Result<MultiStatus> {
    let root = parse(xml)?;
    if !root.is(DAV, "multistatus") {
        return Err(Error::Other(format!("WebDAV reply: expected multistatus, got {}", root.name)));
    }
    let responses = root
        .all(DAV, "response")
        .filter_map(|r| {
            let href = r.child_text(DAV, "href")?.to_string();
            let status = r.child_text(DAV, "status").and_then(status_code);
            let props = r
                .all(DAV, "propstat")
                .filter(|ps| ps.child_text(DAV, "status").and_then(status_code).is_some_and(|s| (200..300).contains(&s)))
                .filter_map(|ps| ps.child(DAV, "prop"))
                .flat_map(|p| p.children.iter().cloned())
                .collect();
            Some(Response { href, status, props })
        })
        .collect();
    Ok(MultiStatus { responses, sync_token: root.child_text(DAV, "sync-token").map(str::to_string) })
}

/// 200 from `HTTP/1.1 200 OK`.
fn status_code(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

/// The path of an href with percent-escapes decoded, so `/a%40b/` and
/// `/a@b/`, or a full URL and its path, compare equal.
pub fn href_key(href: &str) -> String {
    let path = match href.split_once("://") {
        Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
        None => href,
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (bytes.get(i + 1).and_then(|b| hex(*b)), bytes.get(i + 2).and_then(|b| hex(*b)))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
