//! Writing mail: new messages, replies and forwards, built with
//! mail-builder into the bytes the outbox sends.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use mail_parser::MessageParser;
use rusqlite::{Connection, OptionalExtension, params};
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
    /// The size its pasted images go out at.
    pub image_size: ImageSize,
}

/// The size pasted images go out at, as Apple Mail offers it: the longest
/// side at most 320, 640 or 1280 pixels, or as pasted. An image is never
/// made larger, and a GIF keeps its size so it keeps its animation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageSize {
    Small,
    Medium,
    Large,
    #[default]
    Actual,
}

impl ImageSize {
    pub const ALL: [ImageSize; 4] = [ImageSize::Small, ImageSize::Medium, ImageSize::Large, ImageSize::Actual];

    /// How a draft stores it.
    pub fn name(self) -> &'static str {
        match self {
            ImageSize::Small => "small",
            ImageSize::Medium => "medium",
            ImageSize::Large => "large",
            ImageSize::Actual => "actual",
        }
    }

    pub fn from_name(name: &str) -> ImageSize {
        ImageSize::ALL.into_iter().find(|s| s.name() == name).unwrap_or_default()
    }

    /// The longest side in pixels, or None for the size as pasted.
    pub fn longest_side(self) -> Option<u32> {
        match self {
            ImageSize::Small => Some(320),
            ImageSize::Medium => Some(640),
            ImageSize::Large => Some(1280),
            ImageSize::Actual => None,
        }
    }
}

/// What the message comes to at one image size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeOption {
    pub size: ImageSize,
    /// The built message, in bytes.
    pub bytes: u64,
    /// False when the account's provider refuses a message this large.
    pub fits: bool,
}

/// The largest message Microsoft Graph's sendMail takes, as Jolta builds it:
/// the request carries the message in base64, a third larger, and Graph
/// refuses requests over about 4 MB.
pub const GRAPH_SEND_LIMIT: u64 = 3_000_000;

/// The message at each image size, smallest first; empty when the body shows
/// no images.
pub fn size_options(account: &Account, c: &Compose) -> Result<Vec<SizeOption>> {
    if rich::images(&c.body).is_empty() {
        return Ok(Vec::new());
    }
    let limit = (account.kind == AccountKind::Graph).then_some(GRAPH_SEND_LIMIT);
    ImageSize::ALL
        .into_iter()
        .map(|size| {
            let bytes = build(account, &Compose { image_size: size, ..c.clone() }, false)?.raw.len() as u64;
            Ok(SizeOption { size, bytes, fits: limit.is_none_or(|l| bytes <= l) })
        })
        .collect()
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

/// A draft written elsewhere, from its raw source, ready to edit: its
/// recipients, subject, text and the message it answers. Attachments come
/// from `forward_attachments`.
pub fn from_draft(conn: &Connection, message: MessageId, raw: &[u8]) -> Result<Compose> {
    let account: AccountId = conn.query_row("SELECT account_id FROM message WHERE id = ?1", [message], |r| r.get(0))?;
    let msg = MessageParser::new().parse(raw).ok_or_else(|| Error::Other("could not read the draft".into()))?;
    let root = msg.root();
    let (envelope, body) = mime::parse_full(raw, String::new());
    let list = |l: Option<mail_parser::AddressList>| -> String {
        let pairs: Vec<(String, String)> = l
            .map(|l| l.mailboxes().filter_map(|m| Some((m.name().unwrap_or_default().to_string(), m.address()?.to_string()))).collect())
            .unwrap_or_default();
        format_addresses(&pairs)
    };
    let in_reply_to = envelope.in_reply_to.first().cloned();
    let replying_to = match &in_reply_to {
        Some(id) => conn
            .query_row("SELECT id FROM message WHERE account_id = ?1 AND message_id = ?2 ORDER BY id LIMIT 1", params![account, id], |r| r.get(0))
            .optional()?,
        None => None,
    };
    Ok(Compose {
        account: Some(account),
        to: list(root.to()),
        cc: list(root.cc()),
        bcc: list(root.bcc()),
        subject: envelope.subject,
        body: rich::escape(body.text.trim_end()),
        in_reply_to,
        references: envelope.references,
        replying_to,
        ..Default::default()
    })
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

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The MIME type and file extension of a PNG, JPEG or GIF.
fn image_type(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    match bytes {
        b if b.starts_with(PNG) => Some(("image/png", "png")),
        [0xFF, 0xD8, 0xFF, ..] => Some(("image/jpeg", "jpg")),
        b if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") => Some(("image/gif", "gif")),
        _ => None,
    }
}

/// Keeps a pasted image in `dir` for a message to show, and returns its
/// path. Only PNG, JPEG and GIF, which every mail app shows inline.
pub fn save_image(dir: &Path, bytes: &[u8]) -> Result<String> {
    let Some((_, ext)) = image_type(bytes) else { return Err(Error::Other("Only PNG, JPEG and GIF images can be pasted.".into())) };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Error::Other(format!(
            "This image is {:.1} MB; pasted images can be up to {} MB.",
            bytes.len() as f64 / (1 << 20) as f64,
            MAX_IMAGE_BYTES >> 20
        )));
    }
    let path = dir.join(format!("{}.{ext}", &hex::encode(Sha256::digest(bytes))[..32]));
    if !path.exists() {
        std::fs::create_dir_all(dir)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(path.to_string_lossy().into_owned())
}

/// The width to show a PNG at when it was taken on a Retina screen, which
/// records 144 dpi or more: its size on that screen, so it isn't twice as
/// large in the message. None for a 1x image.
fn retina_width(bytes: &[u8]) -> Option<u32> {
    let be = |b: &[u8]| -> Option<u32> { Some(u32::from_be_bytes(b.try_into().ok()?)) };
    if !bytes.starts_with(PNG) {
        return None;
    }
    let width = be(bytes.get(16..20)?)?;
    let mut at = PNG.len();
    while let Some(head) = bytes.get(at..at + 8) {
        let len = be(&head[..4])? as usize;
        let data = bytes.get(at + 8..at + 8 + len)?;
        match &head[4..] {
            b"pHYs" if len == 9 && data[8] == 1 => {
                // 2835 pixels per meter is 72 dpi.
                let per_meter = be(&data[..4])?;
                return (per_meter >= 5600).then(|| ((u64::from(width) * 2835 + u64::from(per_meter) / 2) / u64::from(per_meter)) as u32);
            }
            b"IDAT" | b"IEND" => return None,
            _ => at += len + 12,
        }
    }
    None
}

/// An image the body shows, as one part of the message.
struct InlineImage {
    cid: String,
    name: String,
    mime_type: &'static str,
    bytes: Vec<u8>,
    width: Option<u32>,
}

/// The image as it goes out at `size`, with its new width when it was made
/// smaller. The smaller copy is made from the original and kept next to it,
/// so the next build reuses it and going back to Actual Size loses nothing.
/// The original goes instead when the copy comes out no smaller in bytes, as
/// a screenshot macOS compressed well can.
fn sized(name: &str, path: &str, original: Vec<u8>, size: ImageSize) -> Result<(Vec<u8>, Option<u32>)> {
    let failed = |e: image::ImageError| Error::Other(format!("{name} could not be made smaller: {e}"));
    let reader = |bytes: &[u8]| image::ImageReader::new(std::io::Cursor::new(bytes.to_vec())).with_guessed_format();
    let Some(longest) = size.longest_side().filter(|_| !original.starts_with(b"GIF")) else { return Ok((original, None)) };
    let (width, height) = reader(&original)?.into_dimensions().map_err(failed)?;
    if width.max(height) <= longest {
        return Ok((original, None));
    }
    let jpeg = original.starts_with(&[0xFF, 0xD8, 0xFF]);
    let copy = Path::new(path).with_extension(format!("{longest}.{}", if jpeg { "jpg" } else { "png" }));
    let smaller = |bytes: Vec<u8>, width: u32, original: Vec<u8>| {
        if bytes.len() < original.len() { (bytes, Some(width)) } else { (original, None) }
    };
    if let Ok(bytes) = std::fs::read(&copy) {
        let (w, _) = reader(&bytes)?.into_dimensions().map_err(failed)?;
        return Ok(smaller(bytes, w, original));
    }
    let mut decoder = reader(&original)?.into_decoder().map_err(failed)?;
    let orientation = image::ImageDecoder::orientation(&mut decoder).map_err(failed)?;
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(failed)?;
    img.apply_orientation(orientation);
    let img = img.resize(longest, longest, image::imageops::FilterType::Lanczos3);
    let mut bytes = Vec::new();
    if jpeg {
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85).encode_image(&img.to_rgb8()).map_err(failed)?;
    } else {
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        let encoder = PngEncoder::new_with_quality(&mut bytes, CompressionType::Best, FilterType::Adaptive);
        img.write_with_encoder(encoder).map_err(failed)?;
    }
    let tmp = copy.with_extension("tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &copy)?;
    Ok(smaller(bytes, img.width(), original))
}

/// The images `body` shows at `size`, one part per distinct image, and
/// which part each path is.
fn inline_images(body: &str, domain: &str, size: ImageSize) -> Result<(Vec<InlineImage>, HashMap<String, usize>)> {
    let mut parts: Vec<InlineImage> = Vec::new();
    let mut by_path = HashMap::new();
    for (name, path) in rich::images(body) {
        if by_path.contains_key(&path) {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|_| Error::Other(format!("The image {name} is no longer there; paste it again.")))?;
        let Some((mime_type, _)) = image_type(&bytes) else {
            return Err(Error::Other(format!("{name} is not a PNG, JPEG or GIF image.")));
        };
        let cid = format!("{}@{domain}", &hex::encode(Sha256::digest(&bytes))[..24]);
        let at = match parts.iter().position(|p| p.cid == cid) {
            Some(at) => at,
            None => {
                let on_screen = retina_width(&bytes);
                let (bytes, sent_width) = sized(&name, &path, bytes, size)?;
                let width = on_screen.filter(|&w| sent_width.is_none_or(|sent| w < sent));
                parts.push(InlineImage { cid, name, mime_type, width, bytes });
                parts.len() - 1
            }
        };
        by_path.insert(path, at);
    }
    Ok((parts, by_path))
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
    let (images, image_at) = inline_images(&c.body, domain, c.image_size)?;
    let img = |s: &rich::Span| {
        let image = &images[image_at[s.image.as_deref().unwrap_or_default()]];
        let width = image.width.map(|w| format!(" width=\"{w}\"")).unwrap_or_default();
        format!("<img src=\"cid:{}\" alt=\"{}\"{width} style=\"max-width: 100%\">", image.cid, rich::html_escape(&s.text))
    };
    let mut body = MimePart::new(
        "multipart/alternative",
        vec![MimePart::new("text/plain", rich::to_text(&c.body)), MimePart::new("text/html", rich::to_html(&c.body, &img))],
    );
    if !images.is_empty() {
        let mut parts = vec![body];
        parts.extend(images.into_iter().map(|i| {
            MimePart::new(ContentType::new(i.mime_type).attribute("name", i.name.clone()), i.bytes)
                .header("Content-Disposition", ContentType::new("inline").attribute("filename", i.name))
                .cid(i.cid)
        }));
        body = MimePart::new("multipart/related", parts);
    }
    if !c.attachments.is_empty() {
        let mut parts = vec![body];
        for a in &c.attachments {
            let bytes = std::fs::read(&a.path).map_err(|e| Error::Other(format!("could not read {}: {e}", a.name)))?;
            parts.push(MimePart::new(a.mime_type.clone(), bytes).attachment(a.name.clone()));
        }
        body = MimePart::new("multipart/mixed", parts);
    }

    let from_name = (!account.display_name.is_empty()).then(|| account.display_name.clone());
    let mut builder = MessageBuilder::new()
        .from(BuilderAddress::new_address(from_name, account.address.clone()))
        .subject(c.subject.clone())
        .message_id(message_id.clone())
        .body(body);
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
        let text = msg.body_text(0).unwrap().replace("\r\n", "\n");
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

    fn crc32(bytes: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in bytes {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    /// A real image `width` by `height`, smooth or noise (which barely compresses).
    fn encoded(width: u32, height: u32, noise: bool, format: image::ImageFormat) -> Vec<u8> {
        let mut seed = 0x2545_f491u32;
        let img = image::RgbImage::from_fn(width, height, |x, y| {
            if noise {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let [a, b, c, _] = seed.to_le_bytes();
                image::Rgb([a, b, c])
            } else {
                image::Rgb([(x % 251) as u8, (y % 241) as u8, 120])
            }
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), format).unwrap();
        out
    }

    /// A real PNG as a Retina screenshot saves it, at 144 dpi.
    fn retina_png(width: u32, height: u32) -> Vec<u8> {
        let mut out = encoded(width, height, false, image::ImageFormat::Png);
        let data = [&5669u32.to_be_bytes()[..], &5669u32.to_be_bytes(), &[1]].concat();
        let mut chunk = 9u32.to_be_bytes().to_vec();
        chunk.extend(b"pHYs");
        chunk.extend(&data);
        chunk.extend(crc32(&[&b"pHYs"[..], &data].concat()).to_be_bytes());
        out.splice(33..33, chunk);
        out
    }

    fn dimensions(bytes: &[u8]) -> (u32, u32) {
        image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().unwrap().into_dimensions().unwrap()
    }

    /// The image parts of a built message in order, with its HTML part.
    fn sent_images(c: &Compose) -> (Vec<(String, Vec<u8>)>, String) {
        let raw = build(&account(), c, true).unwrap().raw;
        let msg = MessageParser::new().parse(&raw).unwrap();
        let parts = msg
            .parts()
            .filter(|p| p.content_id().is_some())
            .map(|p| {
                let ct = p.content_type().map(|t| format!("{}/{}", t.ctype(), t.subtype().unwrap_or_default())).unwrap_or_default();
                (ct, p.decoded().into_owned())
            })
            .collect();
        (parts, msg.body_html(0).unwrap().into_owned())
    }

    #[test]
    fn images_go_out_at_the_chosen_size_made_from_the_original() {
        let dir = temp_dir("sizes");
        let screenshot = retina_png(1600, 800);
        let photo = encoded(1500, 1000, false, image::ImageFormat::Jpeg);
        let icon = encoded(300, 200, false, image::ImageFormat::Png);
        let gif = b"GIF89a\x00\x08\x00\x08\x00\x00\x00;".to_vec();
        let saved: Vec<String> = [&screenshot, &photo, &icon, &gif].iter().map(|b| save_image(&dir, b).unwrap()).collect();
        let body = rich::markdown(&saved.iter().map(|p| image("img", p)).collect::<Vec<_>>());
        let at = |image_size| Compose { to: "bob@example.com".into(), body: body.clone(), image_size, ..Default::default() };

        let (parts, html) = sent_images(&at(ImageSize::Medium));
        assert_eq!(parts[0].0, "image/png");
        assert_eq!(dimensions(&parts[0].1), (640, 320), "the longest side becomes 640 pixels");
        assert_eq!(parts[1].0, "image/jpeg");
        let (w, h) = dimensions(&parts[1].1);
        assert!(w == 640 && (426..=427).contains(&h), "{w}x{h}");
        assert_eq!(parts[2].1, icon, "a smaller image is never made larger");
        assert_eq!(parts[3].1, gif, "a GIF keeps its size and its animation");
        assert!(!html.contains("width=\"640\""), "{html}");

        let (parts, html) = sent_images(&at(ImageSize::Large));
        assert_eq!(dimensions(&parts[0].1), (1280, 640));
        assert!(html.contains("width=\"800\""), "never larger than it was on screen: {html}");

        let (parts, html) = sent_images(&at(ImageSize::Small));
        assert_eq!(dimensions(&parts[0].1), (320, 160));
        assert_eq!(dimensions(&parts[2].1), (300, 200));
        assert!(!html.contains("width="), "{html}");

        let (parts, html) = sent_images(&at(ImageSize::Actual));
        assert_eq!((parts[0].1.clone(), parts[1].1.clone()), (screenshot.clone(), photo.clone()), "back to the original bytes");
        assert!(html.contains("width=\"800\""), "{html}");
        assert_eq!(std::fs::read(&saved[0]).unwrap(), screenshot, "the pasted copy stays as it was");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_smaller_size_never_makes_a_bigger_file() {
        let dir = temp_dir("not-bigger");
        let img = image::RgbImage::from_fn(1500, 1000, |x, y| image::Rgb([(x % 251) as u8, (y % 241) as u8, 120]));
        let mut rough = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut rough, 20).encode_image(&img).unwrap();
        let path = save_image(&dir, &rough).unwrap();
        let c = Compose {
            to: "bob@example.com".into(),
            body: rich::markdown(&[image("Rough.jpg", &path)]),
            image_size: ImageSize::Large,
            ..Default::default()
        };
        let (parts, _) = sent_images(&c);
        assert!(parts[0].1 == rough, "the original goes when it is the smaller file: {} vs {} bytes", parts[0].1.len(), rough.len());
        let options = size_options(&account(), &c).unwrap();
        assert!(options.windows(2).all(|w| w[0].bytes <= w[1].bytes + 64), "no size above Actual Size, give or take a header: {options:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_size_menu_shows_what_each_size_comes_to() {
        let dir = temp_dir("estimate");
        let path = save_image(&dir, &retina_png(1600, 800)).unwrap();
        let c = Compose { to: "bob@example.com".into(), body: rich::markdown(&[image("Shot.png", &path)]), ..Default::default() };
        let options = size_options(&account(), &c).unwrap();
        assert_eq!(options.iter().map(|o| o.size).collect::<Vec<_>>(), ImageSize::ALL);
        assert!(options.windows(2).all(|w| w[0].bytes < w[1].bytes), "{options:?}");
        assert!(options.iter().all(|o| o.fits), "no limit for an IMAP account");
        let built = build(&account(), &c, true).unwrap().raw.len() as i64;
        assert!((options[3].bytes as i64 - built).abs() < 64, "the estimate is the built message: {} vs {built}", options[3].bytes);

        let plain = Compose { body: "No pictures".into(), ..c.clone() };
        assert!(size_options(&account(), &plain).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn microsoft_365_says_which_sizes_it_takes() {
        let dir = temp_dir("graph-limit");
        let path = save_image(&dir, &encoded(900, 900, true, image::ImageFormat::Png)).unwrap();
        let video = dir.join("talk.mov");
        std::fs::write(&video, vec![0u8; 25_000_000]).unwrap();
        let c = Compose {
            to: "bob@example.com".into(),
            body: rich::markdown(&[image("Noise.png", &path)]),
            attachments: vec![AttachmentFile { name: "talk.mov".into(), mime_type: "video/quicktime".into(), path: video.to_string_lossy().into() }],
            ..Default::default()
        };
        let options = size_options(&Account { kind: AccountKind::Graph, ..account() }, &c).unwrap();
        assert_eq!(options.iter().map(|o| o.fits).collect::<Vec<_>>(), [true, true, false, false], "{options:?}");
        assert!(size_options(&account(), &c).unwrap().iter().all(|o| o.fits), "only Microsoft 365 has the limit");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prefixes_subjects_once() {
        assert_eq!(prefixed("Lunch", "Re"), "Re: Lunch");
        assert_eq!(prefixed("Re: Lunch", "Re"), "Re: Lunch");
        assert_eq!(prefixed("RE: Lunch", "Re"), "RE: Lunch");
        assert_eq!(prefixed("Lunch", "Fwd"), "Fwd: Lunch");
    }
}
