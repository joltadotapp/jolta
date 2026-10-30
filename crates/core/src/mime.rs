//! Turning raw RFC 822 bytes into the fields the store keeps.

use mail_parser::{AddressList, MessageParser, MessageRef};

use crate::model::{BodyInfo, Envelope};

const SNIPPET_LEN: usize = 200;
/// Body text beyond this is left out of the search index.
const INDEX_TEXT_LEN: usize = 64 * 1024;

fn join_addresses(list: Option<AddressList>) -> String {
    let Some(list) = list else { return String::new() };
    list.mailboxes()
        .map(|m| match (m.name(), m.address()) {
            (Some(n), Some(a)) if !n.is_empty() => format!("{n} <{a}>"),
            (_, Some(a)) => a.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn envelope_of(m: &MessageRef) -> Envelope {
    let from = m.from().and_then(|l| l.first());
    let header = |name: &str| {
        m.headers()
            .iter()
            .find(|h| h.raw_name().eq_ignore_ascii_case(name))
            .map(|h| String::from_utf8_lossy(h.raw_value()).trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let list_id = header("List-Id").map(|v| match (v.rfind('<'), v.rfind('>')) {
        (Some(a), Some(b)) if a < b => v[a + 1..b].to_string(),
        _ => v,
    });
    Envelope {
        message_id: m.message_id().map(str::to_string),
        in_reply_to: m.in_reply_to().map(|l| l.iter().map(str::to_string).collect()).unwrap_or_default(),
        references: m.references().map(|l| l.iter().map(str::to_string).collect()).unwrap_or_default(),
        from_name: from.and_then(|f| f.name()).unwrap_or_default().to_string(),
        from_addr: from.and_then(|f| f.address()).unwrap_or_default().to_string(),
        to: join_addresses(m.to()),
        cc: join_addresses(m.cc()),
        subject: m.subject().unwrap_or_default().to_string(),
        list_id,
        list_unsubscribe: header("List-Unsubscribe"),
        has_attachments: m.is_content_type("multipart", "mixed"),
    }
}

/// Envelope from a header block, as fetched with `BODY.PEEK[HEADER]`.
pub fn parse_envelope(header: &[u8]) -> Envelope {
    match MessageParser::new().parse_headers(header) {
        Some(msg) => envelope_of(&msg.root()),
        None => Envelope::default(),
    }
}

/// Header date in unix seconds, if the message has a usable one.
pub fn header_date(raw: &[u8]) -> Option<i64> {
    MessageParser::new().parse_headers(raw)?.root().date().map(|d| d.to_timestamp())
}

/// Envelope plus body facts from a full message.
pub fn parse_full(raw: &[u8], blob: String) -> (Envelope, BodyInfo) {
    let Some(msg) = MessageParser::new().parse(raw) else {
        return (Envelope::default(), BodyInfo { blob, ..Default::default() });
    };
    let root = msg.root();
    let mut text = root.body_text(0).map(|t| t.into_owned()).unwrap_or_default();
    if text.len() > INDEX_TEXT_LEN {
        let mut end = INDEX_TEXT_LEN;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let envelope = envelope_of(&root);
    let body = BodyInfo {
        blob,
        snippet: snippet(&text),
        has_attachments: root.attachments().any(|a| a.attachment_name().is_some() || !a.is_text()),
        text,
        list_id: envelope.list_id.clone(),
        list_unsubscribe: envelope.list_unsubscribe.clone(),
    };
    (envelope, body)
}

/// The first readable words of a body: quoted lines and reply headers
/// dropped, whitespace collapsed.
pub fn snippet(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('>') || line.is_empty() {
            continue;
        }
        if line.starts_with("On ") && line.ends_with("wrote:") {
            break;
        }
        for word in line.split_whitespace() {
            if out.len() + word.len() + 1 > SNIPPET_LEN {
                return out;
            }
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
    }
    out
}
