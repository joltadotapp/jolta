//! Writing mail: new messages, replies and forwards, built with
//! mail-builder into the bytes the outbox sends.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;
use mail_parser::MessageParser;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::model::*;
use crate::{Error, Result, mime, reader, rich, store};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentFile {
    pub name: String,
    pub mime_type: String,
    pub path: String,
}

/// A message being written. Recipient fields are comma-separated lists as
/// typed ("Ann <ann@example.com>, bob@example.com").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Compose {
    pub account: Option<AccountId>,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// The message this answers; it gets the \Answered flag once sent.
    pub replying_to: Option<MessageId>,
    pub attachments: Vec<AttachmentFile>,
    /// The local draft this compose was saved as, if any.
    pub draft: Option<i64>,
}

/// One address as (name, address).
pub fn parse_addresses(list: &str) -> Vec<(String, String)> {
    if list.trim().is_empty() {
        return Vec::new();
    }
    let header = format!("To: {}\r\n\r\n", list.replace(['\r', '\n'], " "));
    let Some(msg) = MessageParser::new().parse_headers(header.as_bytes()) else { return Vec::new() };
    msg.root()
        .to()
        .map(|l| {
            l.mailboxes()
                .filter_map(|m| Some((m.name().unwrap_or_default().to_string(), m.address()?.to_string())))
                .filter(|(_, a)| a.contains('@'))
                .collect()
        })
        .unwrap_or_default()
}

/// An address list for a To or Cc line, quoting a name that has characters
/// a header would otherwise read as part of the address or the list.
pub(crate) fn format_addresses(list: &[(String, String)]) -> String {
    let plain = |n: &str| !n.contains(['<', '>', ',', ';', ':', '"', '\\', '@', '(', ')', '[', ']']);
    list.iter()
        .map(|(n, a)| match n.as_str() {
            "" => a.clone(),
            n if plain(n) => format!("{n} <{a}>"),
            n => format!("\"{}\" <{a}>", n.replace('\\', "\\\\").replace('"', "\\\"")),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn prefixed(subject: &str, prefix: &str) -> String {
    let (base, _) = store::base_subject(subject);
    let already = subject.trim().to_lowercase().starts_with(&format!("{}:", prefix.to_lowercase()));
    if already || base.is_empty() && subject.is_empty() {
        subject.trim().to_string()
    } else {
        format!("{prefix}: {}", subject.trim())
    }
}

fn quote(text: &str) -> String {
    text.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {}", rich::escape(l)) }).collect::<Vec<_>>().join("\n")
}

/// Where a reply's quote or a forward's copy starts in a body Jolta wrote.
fn quote_start(body: &str) -> Option<usize> {
    let forward = body.find("\n\n---------- Forwarded message ----------");
    let reply = body.match_indices("\n\nOn ").map(|(i, _)| i).find(|&i| body[i..].lines().nth(2).is_some_and(|l| l.ends_with(" wrote:")));
    forward.into_iter().chain(reply).min()
}

/// What a message starts with: room to write, the signature, then the
/// quote or forwarded copy, if any.
pub fn opening(signature: &str, quoted: &str) -> String {
    let signature = signature.trim_end();
    match (signature.is_empty(), quoted.is_empty()) {
        (true, true) => String::new(),
        (true, false) => format!("\n\n{quoted}"),
        (false, true) => format!("\n\n{signature}"),
        (false, false) => format!("\n\n{signature}\n\n{quoted}"),
    }
}

/// Where the part the writer types ends: at the signature or the quote.
fn written_end(body: &str, signature: &str) -> usize {
    let signature = signature.trim_end();
    let at_signature = if signature.is_empty() { None } else { body.find(&format!("\n\n{signature}")) };
    at_signature.into_iter().chain(quote_start(body)).min().unwrap_or(body.len())
}

/// What the writer typed, without the signature and the quote below it.
pub fn written(body: &str, signature: &str) -> String {
    body[..written_end(body, signature)].trim().to_string()
}

/// The body with what the writer typed replaced by `text` (body format),
/// keeping the signature and the quote.
pub fn with_written(body: &str, signature: &str, text: &str) -> String {
    format!("{}{}", text.trim_end(), &body[written_end(body, signature)..])
}

/// Swaps one account's signature for another's when the sender changes. A
/// signature that was edited in the message is left alone.
pub fn swap_signature(body: &str, old: &str, new: &str) -> String {
    let (old, new) = (old.trim_end(), new.trim_end());
    if old == new {
        return body.to_string();
    }
    if !old.is_empty() {
        let Some(at) = body.find(&format!("\n\n{old}")) else { return body.to_string() };
        let replacement = if new.is_empty() { String::new() } else { format!("\n\n{new}") };
        return format!("{}{replacement}{}", &body[..at], &body[at + 2 + old.len()..]);
    }
    if new.is_empty() {
        return body.to_string();
    }
    match quote_start(body) {
        Some(at) => format!("{}\n\n{new}{}", &body[..at], &body[at..]),
        None => format!("{}\n\n{new}", body.trim_end()),
    }
}

/// Prefills a reply, reply-all or forward of `message` from its raw source.
pub fn reply(conn: &Connection, message: MessageId, raw: &[u8], kind: ReplyKind, utc_offset: i32) -> Result<Compose> {
    let (account, date): (AccountId, i64) =
        conn.query_row("SELECT account_id, date FROM message WHERE id = ?1", [message], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let me = store::account(conn, account)?.address.to_lowercase();
    let msg = MessageParser::new().parse(raw).ok_or_else(|| Error::Other("could not read the message".into()))?;
    let root = msg.root();
    let (envelope, body) = mime::parse_full(raw, String::new());
    let pair = |m: mail_parser::Mailbox| Some((m.name().unwrap_or_default().to_string(), m.address()?.to_string()));
    let from: Vec<(String, String)> = root.from().map(|l| l.mailboxes().filter_map(pair).collect()).unwrap_or_default();
    let reply_to: Vec<(String, String)> = root.reply_to().map(|l| l.mailboxes().filter_map(pair).collect()).unwrap_or_default();
    let others = |l: Option<mail_parser::AddressList>| -> Vec<(String, String)> {
        l.map(|l| l.mailboxes().filter_map(pair).collect()).unwrap_or_default()
    };
    let not_me = |list: Vec<(String, String)>| -> Vec<(String, String)> {
        list.into_iter().filter(|(_, a)| a.to_lowercase() != me).collect()
    };
    let sender_name = from.first().map(|(n, a)| if n.is_empty() { a.clone() } else { n.clone() }).unwrap_or_default();
    let when = reader::format_date(date, utc_offset);

    let mut c = Compose { account: Some(account), ..Default::default() };
    match kind {
        ReplyKind::Reply | ReplyKind::ReplyAll => {
            let primary = if reply_to.is_empty() { from.clone() } else { reply_to };
            let from_me = from.iter().any(|(_, a)| a.to_lowercase() == me);
            // Replying to your own message goes to its original recipients.
            let to = match from_me {
                true => Some(not_me(others(root.to()))).filter(|t| !t.is_empty()).unwrap_or(primary),
                false => primary,
            };
            c.to = format_addresses(&to);
            if kind == ReplyKind::ReplyAll {
                let mut cc = not_me(others(root.to()));
                cc.extend(not_me(others(root.cc())));
                let mut seen: Vec<String> = to.iter().map(|(_, a)| a.to_lowercase()).collect();
                cc.retain(|(_, a)| {
                    let a = a.to_lowercase();
                    let new = !seen.contains(&a);
                    seen.push(a);
                    new
                });
                c.cc = format_addresses(&cc);
            }
            c.subject = prefixed(&envelope.subject, "Re");
            c.in_reply_to = envelope.message_id.clone();
            c.references = envelope.references.clone();
            c.references.extend(envelope.message_id.clone());
            c.replying_to = Some(message);
            c.body = format!("On {when}, {} wrote:\n{}", rich::escape(&sender_name), quote(body.text.trim_end()));
        }
        ReplyKind::Forward => {
            c.subject = prefixed(&envelope.subject, "Fwd");
            c.body = rich::escape(&format!(
                "---------- Forwarded message ----------\nFrom: {}\nDate: {when}\nSubject: {}\nTo: {}\n\n{}",
                format_addresses(&from),
                envelope.subject,
                envelope.to,
                body.text.trim_end()
            ));
            c.references = envelope.message_id.iter().cloned().collect();
        }
    }
    let signature = store::account(conn, account)?.signature;
    c.body = opening(&signature, &c.body);
    Ok(c)
}

/// Saves the attachments of a message being forwarded into `dir`, so the
/// forward can carry them. Images its HTML shows inline stay behind: the
/// forward is plain text, and they are mostly logos.
pub fn forward_attachments(raw: &[u8], dir: &Path) -> Result<Vec<AttachmentFile>> {
    let raw = crate::smime::readable(raw);
    let Some(msg) = MessageParser::new().parse(&raw[..]) else { return Ok(Vec::new()) };
    let html = msg.body_html(0).unwrap_or_default().to_lowercase();
    let _ = std::fs::remove_dir_all(dir);
    let mut used = HashSet::new();
    let mut files = Vec::new();
    for part in msg.root().attachments() {
        let inline = part.content_id().is_some_and(|id| html.contains(&format!("cid:{}", id.trim_matches(['<', '>']).to_lowercase())));
        if inline {
            continue;
        }
        let name = file_name(part.attachment_name().unwrap_or("attachment"));
        let mut saved = name.clone();
        let mut n = 2;
        while !used.insert(saved.to_lowercase()) {
            saved = match name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem} {n}.{ext}"),
                _ => format!("{name} {n}"),
            };
            n += 1;
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(&saved);
        std::fs::write(&path, part.decoded())?;
        let mime_type = part
            .content_type()
            .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
            .unwrap_or_else(|| "application/octet-stream".into());
        files.push(AttachmentFile { name, mime_type, path: path.to_string_lossy().into_owned() });
    }
    Ok(files)
}

/// An attachment name made safe to use as a file name.
fn file_name(name: &str) -> String {
    let clean: String = name.chars().map(|c| if c == '/' || c == '\\' || c == ':' || c.is_control() { '_' } else { c }).collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() { "attachment".into() } else { clean }
}

/// A snippet's body with `{first name}`, `{name}` and `{email}` filled in
/// from the first recipient in `to` (as typed). Unknown values become empty.
pub fn expand_snippet(body: &str, to: &str) -> String {
    let (name, address) = parse_addresses(to).into_iter().next().unwrap_or_default();
    let first = name.split_whitespace().next().unwrap_or_default().to_string();
    let mut out = body.to_string();
    for (key, value) in [("{first name}", &first), ("{first_name}", &first), ("{name}", &name), ("{email}", &address)] {
        out = out.replace(key, &rich::escape(value));
    }
    out
}

/// The largest image paste takes.
pub const MAX_IMAGE_BYTES: usize = 10 << 20;

/// Keeps a pasted image in `dir` for a message to show, and returns its
/// path. Only PNG, JPEG and GIF, which every mail app shows inline.
pub fn save_image(_dir: &Path, _bytes: &[u8]) -> Result<String> {
    Err(Error::Other("not yet".into()))
}

/// A built message, ready for the outbox.
pub struct Built {
    pub raw: Vec<u8>,
    pub message_id: String,
    pub from: String,
    pub recipients: Vec<String>,
}

/// Builds the message. Drafts may have no recipients yet; sending needs one.
pub fn build(account: &Account, c: &Compose, for_sending: bool) -> Result<Built> {
    let to = parse_addresses(&c.to);
    let cc = parse_addresses(&c.cc);
    let bcc = parse_addresses(&c.bcc);
    if for_sending && to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err(Error::Other("Add at least one recipient.".into()));
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let domain = account.address.rsplit('@').next().unwrap_or("jolta.invalid");
    let message_id = format!("{}@{domain}", &hex::encode(Sha256::digest(format!("{now}{}", c.subject)))[..24]);

    let list = |l: &[(String, String)]| -> BuilderAddress<'static> {
        BuilderAddress::new_list(l.iter().map(|(n, a)| BuilderAddress::new_address(Some(n.clone()).filter(|n| !n.is_empty()), a.clone())).collect())
    };
    let from_name = (!account.display_name.is_empty()).then(|| account.display_name.clone());
    let mut builder = MessageBuilder::new()
        .from(BuilderAddress::new_address(from_name, account.address.clone()))
        .subject(c.subject.clone())
        .message_id(message_id.clone())
        .text_body(rich::to_text(&c.body))
        .html_body(rich::to_html(&c.body, &|_| String::new()));
    if !to.is_empty() {
        builder = builder.to(list(&to));
    }
    if !cc.is_empty() {
        builder = builder.cc(list(&cc));
    }
    if let Some(irt) = &c.in_reply_to {
        builder = builder.in_reply_to(irt.clone());
    }
    if !c.references.is_empty() {
        builder = builder.references(c.references.clone());
    }
    for a in &c.attachments {
        let bytes = std::fs::read(&a.path).map_err(|e| Error::Other(format!("could not read {}: {e}", a.name)))?;
        builder = builder.attachment(a.mime_type.clone(), a.name.clone(), bytes);
    }
    let raw = builder.write_to_vec()?;
    let recipients = to.iter().chain(&cc).chain(&bcc).map(|(_, a)| a.clone()).collect();
    Ok(Built { raw, message_id, from: account.address.clone(), recipients })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_name_with_special_characters_survives_formatting_and_parsing() {
        let list = vec![
            ("Jane <Support> Doe".to_string(), "jane@example.com".to_string()),
            ("Berg, Ann \"AB\"".to_string(), "ann@example.com".to_string()),
            ("Bo".to_string(), "bo@example.com".to_string()),
        ];
        let line = super::format_addresses(&list);
        assert_eq!(super::parse_addresses(&line), list, "{line}");
        assert!(line.ends_with(", Bo <bo@example.com>"), "a plain name stays unquoted: {line}");
    }

    use super::*;

    #[test]
    fn parses_typed_recipients() {
        assert_eq!(
            parse_addresses("Ann Smith <ann@example.com>, bob@example.com; ,  not-an-address"),
            vec![("Ann Smith".into(), "ann@example.com".into()), ("".into(), "bob@example.com".into())]
        );
        assert!(parse_addresses("  ").is_empty());
    }

    #[test]
    fn forwards_carry_attachments_but_not_inline_images() {
        let raw = "From: a@example.com\r\nSubject: Invoice\r\nMIME-Version: 1.0\r\n\
            Content-Type: multipart/mixed; boundary=\"x\"\r\n\r\n\
            --x\r\nContent-Type: text/html\r\n\r\n<p>Hi</p><img src=\"cid:logo@a\">\r\n\
            --x\r\nContent-Type: image/png\r\nContent-ID: <logo@a>\r\nContent-Transfer-Encoding: base64\r\n\r\naGk=\r\n\
            --x\r\nContent-Type: application/pdf; name=\"../inv.pdf\"\r\nContent-Disposition: attachment; filename=\"../inv.pdf\"\r\n\
            Content-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n\
            --x\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"_inv.pdf\"\r\n\r\nsecond\r\n\
            --x--\r\n";
        let dir = std::env::temp_dir().join(format!("jolta-fwd-{}", std::process::id()));
        let files = forward_attachments(raw.as_bytes(), &dir).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["_inv.pdf", "_inv.pdf"], "the inline logo stays behind and the path is defused");
        assert_eq!(files[0].mime_type, "application/pdf");
        assert_eq!(std::fs::read(&files[0].path).unwrap(), b"%PDF-");
        assert!(files[1].path.ends_with("_inv 2.pdf"), "same names get their own files: {}", files[1].path);
        assert!(files.iter().all(|f| Path::new(&f.path).starts_with(&dir)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snippets_fill_in_the_first_recipient() {
        let body = "Hi {first name},\n\nThanks, **{name}** ({email}).";
        assert_eq!(expand_snippet(body, "Ann Smith <ann@x.test>, bob@x.test"), "Hi Ann,\n\nThanks, **Ann Smith** (ann@x.test).");
        assert_eq!(expand_snippet(body, "bob@x.test"), "Hi ,\n\nThanks, **** (bob@x.test).");
        assert_eq!(expand_snippet("Hi {first_name}", "\"Star*\" <s@x.test>"), "Hi Star\\*", "names are escaped");
    }

    #[test]
    fn signatures_follow_the_sender() {
        let quoted = "On 1 Nov 2026, 10:00, Ann wrote:\n> hi";
        let reply = opening("Koen\n**Example Ltd**", quoted);
        assert_eq!(reply, "\n\nKoen\n**Example Ltd**\n\nOn 1 Nov 2026, 10:00, Ann wrote:\n> hi");
        let swapped = swap_signature(&format!("Thanks!{reply}"), "Koen\n**Example Ltd**", "K.");
        assert_eq!(swapped, "Thanks!\n\nK.\n\nOn 1 Nov 2026, 10:00, Ann wrote:\n> hi");
        assert_eq!(swap_signature(&swapped, "K.", ""), "Thanks!\n\nOn 1 Nov 2026, 10:00, Ann wrote:\n> hi");
        let no_signature = format!("Thanks!{}", opening("", quoted));
        assert_eq!(swap_signature(&no_signature, "", "K."), "Thanks!\n\nK.\n\nOn 1 Nov 2026, 10:00, Ann wrote:\n> hi");
        assert_eq!(swap_signature("Hello", "", "K."), "Hello\n\nK.");
        assert_eq!(swap_signature("Hello\n\nK. (edited)", "K. (gone)", "X"), "Hello\n\nK. (edited)", "an edited signature stays");
    }

    fn account() -> Account {
        Account {
            id: AccountId(1),
            kind: AccountKind::Imap,
            address: "me@example.com".into(),
            display_name: "Me".into(),
            username: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            secret_ref: String::new(),
            imap_security: Security::Tls,
            smtp_security: Security::Tls,
            smtp_auth: SmtpAuth::SameAsIncoming,
            smtp_username: String::new(),
            verify_certs: true,
            signature: String::new(),
            calendar: false,
        }
    }

    /// A PNG header `width` pixels wide, at `per_meter` pixels per meter.
    fn png(width: u32, per_meter: Option<u32>) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8], data: &[u8]| {
            out.extend((data.len() as u32).to_be_bytes());
            out.extend(kind);
            out.extend(data);
            out.extend([0; 4]);
        };
        chunk(b"IHDR", &[&width.to_be_bytes()[..], &10u32.to_be_bytes(), &[8, 6, 0, 0, 0]].concat());
        if let Some(p) = per_meter {
            chunk(b"pHYs", &[&p.to_be_bytes()[..], &p.to_be_bytes(), &[1]].concat());
        }
        chunk(b"IEND", &[]);
        out
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("jolta-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The MIME tree as `type[child,child]`.
    fn tree(part: mail_parser::MessagePart) -> String {
        let ct = part.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or_default())).unwrap_or_default();
        match part.is_multipart() {
            true => format!("{ct}[{}]", part.children().map(tree).collect::<Vec<_>>().join(",")),
            false => ct,
        }
    }

    fn image(name: &str, path: &str) -> rich::Span {
        rich::Span { text: name.into(), image: Some(path.into()), ..Default::default() }
    }

    #[test]
    fn a_pasted_image_shows_inline_where_it_was_put() {
        let dir = temp_dir("inline");
        let retina = png(800, Some(5669));
        let path = save_image(&dir.join("images"), &retina).unwrap();
        assert!(Path::new(&path).starts_with(dir.join("images")), "{path}");
        assert_eq!(save_image(&dir.join("images"), &retina).unwrap(), path, "the same image is kept once");
        let pdf = dir.join("offer.pdf");
        std::fs::write(&pdf, b"%PDF-").unwrap();
        let body = rich::markdown(&[
            rich::Span { text: "See below:\n".into(), ..Default::default() },
            image("Screenshot.png", &path),
            rich::Span { text: "\nand once more ".into(), ..Default::default() },
            image("Screenshot.png", &path),
        ]);
        let c = Compose {
            to: "bob@example.com".into(),
            subject: "Look".into(),
            body,
            attachments: vec![AttachmentFile { name: "offer.pdf".into(), mime_type: "application/pdf".into(), path: pdf.to_string_lossy().into() }],
            ..Default::default()
        };
        let built = build(&account(), &c, true).unwrap();
        let msg = MessageParser::new().parse(&built.raw).unwrap();
        assert_eq!(
            tree(msg.root_part()),
            "multipart/mixed[multipart/related[multipart/alternative[text/plain,text/html],image/png],application/pdf]"
        );
        let inline: Vec<_> = msg.parts().filter(|p| p.content_id().is_some()).collect();
        assert_eq!(inline.len(), 1, "an image shown twice is sent once");
        let cid = inline[0].content_id().unwrap();
        assert_eq!(&inline[0].decoded()[..], &retina[..]);
        assert_eq!(inline[0].content_disposition().map(|d| d.ctype()), Some("inline"));
        assert_eq!(inline[0].attachment_name(), Some("Screenshot.png"));
        let html = msg.body_html(0).unwrap();
        let tag = format!("<img src=\"cid:{cid}\" alt=\"Screenshot.png\" width=\"400\" style=\"max-width: 100%\">");
        assert_eq!(html.matches(&tag).count(), 2, "a Retina screenshot shows at its size on screen: {html}");
        let text = msg.body_text(0).unwrap();
        assert!(text.contains("See below:\n[image: Screenshot.png]\nand once more [image: Screenshot.png]"), "{text}");
        assert!(!text.contains("file:") && !html.contains("file:"), "the local path stays home");
        let pdf_part = msg.attachments().find(|p| p.attachment_name() == Some("offer.pdf")).unwrap();
        assert_eq!(pdf_part.content_disposition().map(|d| d.ctype()), Some("attachment"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_message_without_images_keeps_its_shape() {
        let c = Compose { to: "bob@example.com".into(), body: "Hi **Bob**".into(), ..Default::default() };
        let built = build(&account(), &c, true).unwrap();
        let msg = MessageParser::new().parse(&built.raw).unwrap();
        assert_eq!(tree(msg.root_part()), "multipart/alternative[text/plain,text/html]");
        assert!(msg.body_html(0).unwrap().contains("<b>Bob</b>"));
    }

    #[test]
    fn an_image_at_normal_density_gets_no_width() {
        let dir = temp_dir("plain-density");
        let path = save_image(&dir, &png(300, None)).unwrap();
        let c = Compose { to: "bob@example.com".into(), body: rich::markdown(&[image("chart.png", &path)]), ..Default::default() };
        let msg_raw = build(&account(), &c, true).unwrap().raw;
        let html = MessageParser::new().parse(&msg_raw).unwrap().body_html(0).unwrap().into_owned();
        assert!(html.contains("alt=\"chart.png\" style=\"max-width: 100%\">"), "{html}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_images_that_mail_apps_show_can_be_pasted() {
        let dir = temp_dir("paste-kinds");
        assert!(save_image(&dir, b"GIF89a\x01\x00\x01\x00").unwrap().ends_with(".gif"));
        assert!(save_image(&dir, b"\xff\xd8\xff\xe0rest").unwrap().ends_with(".jpg"));
        assert!(save_image(&dir, b"II*\x00 a TIFF").is_err(), "the app converts other formats first");
        let mut huge = png(10, None);
        huge.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(save_image(&dir, &huge).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_image_that_is_gone_stops_the_send_with_its_name() {
        let c = Compose {
            to: "bob@example.com".into(),
            body: rich::markdown(&[image("Plan.png", "/nonexistent/jolta/Plan.png")]),
            ..Default::default()
        };
        let Err(e) = build(&account(), &c, true) else { panic!("built without its image") };
        assert!(e.to_string().contains("Plan.png"), "{e}");
    }

    #[test]
    fn prefixes_subjects_once() {
        assert_eq!(prefixed("Lunch", "Re"), "Re: Lunch");
        assert_eq!(prefixed("Re: Lunch", "Re"), "Re: Lunch");
        assert_eq!(prefixed("RE: Lunch", "Re"), "RE: Lunch");
        assert_eq!(prefixed("Lunch", "Fwd"), "Fwd: Lunch");
    }
}
