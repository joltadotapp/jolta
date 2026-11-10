//! Formatted message bodies. A body is stored and passed around as text in a
//! small Markdown subset with a little inline HTML, one line per paragraph:
//!
//! - Inline: `**bold**`, `*italic*`, `~~struck~~`, `<u>underlined</u>`,
//!   `[text](url)`, `![name](file:///path)` for an image shown in the text,
//!   and `<span style="…">` for a font, a size, a text color and a highlight.
//!   A span's style takes only `font-family`, `font-size`, `color` and
//!   `background-color`, each with a value from `Font`, `Size`, `Color` or
//!   `Highlight`, written exactly as `css` gives it.
//! - Per line: `- `, `* ` or `• ` for a bullet, `1. ` for a numbered item and
//!   `> ` for a quote. Two spaces before a list marker nest the item one
//!   level, up to `MAX_INDENT`.
//! - A line may be wrapped in `<div style="…">…</div>` with `margin-left` (a
//!   multiple of 40px, up to `MAX_INDENT`) to indent a paragraph or a quote,
//!   and `text-align: center` or `right`.
//!
//! A tag or style outside these lists is text, so nothing else reaches the
//! sent HTML. Bodies written before styles existed read as they did: they
//! hold none of the new tags, and only an even run of spaces before a list
//! marker now nests it. The editor shows a body as `lines`; sending turns it
//! into an HTML part and a plain-text part.

/// A run of text with one set of styles, as the editor shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub font: Option<Font>,
    pub size: Option<Size>,
    pub color: Option<Color>,
    pub highlight: Option<Highlight>,
    pub link: Option<String>,
    /// The image file shown here, by absolute path; `text` is its name.
    pub image: Option<String>,
}

impl Span {
    fn same_style(&self, other: &Span) -> bool {
        self.image.is_none() && other.image.is_none() && Span { text: String::new(), ..self.clone() } == Span { text: String::new(), ..other.clone() }
    }

    fn look(&self) -> Look {
        Look { font: self.font, size: self.size, color: self.color, highlight: self.highlight }
    }

    fn set_look(&mut self, look: Look) {
        (self.font, self.size, self.color, self.highlight) = (look.font, look.size, look.color, look.highlight);
    }
}

/// One line of a body: its spans, with list and quote markers as text, and
/// how far it is indented and how it is aligned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    /// 0 to `MAX_INDENT`: a list item's nesting, or a paragraph's indent.
    pub indent: u8,
    pub align: Align,
    pub spans: Vec<Span>,
}

pub const MAX_INDENT: u8 = 4;
const INDENT_PX: u8 = 40;

/// The fonts a body can name, as stacks every mail app has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Font {
    Arial,
    Verdana,
    Tahoma,
    Trebuchet,
    Georgia,
    Times,
    Courier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Small,
    Large,
    Huge,
}

/// Text colors, readable on a white message and on Jolta's dark and light
/// themes (at least 3.9:1 against each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    Red,
    Orange,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Gray,
}

/// Highlights: light enough that `INK` reads on them in any theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Highlight {
    Yellow,
    Green,
    Blue,
    Pink,
    Orange,
    Purple,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// The text color on a highlight with no color of its own, in the editor
/// and the sent message, so highlighted text reads in dark mode too.
pub const INK: &str = "#1f1f1f";

/// A fixed list of values, each written one way in a body and in HTML.
pub trait Choice: Copy + PartialEq + 'static {
    const ALL: &'static [Self];
    fn css(self) -> &'static str;

    fn from_css(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.css().eq_ignore_ascii_case(value))
    }
}

impl Choice for Font {
    const ALL: &'static [Self] = &[Font::Arial, Font::Verdana, Font::Tahoma, Font::Trebuchet, Font::Georgia, Font::Times, Font::Courier];
    fn css(self) -> &'static str {
        match self {
            Font::Arial => "Arial, Helvetica, sans-serif",
            Font::Verdana => "Verdana, Geneva, sans-serif",
            Font::Tahoma => "Tahoma, Verdana, sans-serif",
            Font::Trebuchet => "'Trebuchet MS', Helvetica, sans-serif",
            Font::Georgia => "Georgia, serif",
            Font::Times => "'Times New Roman', Times, serif",
            Font::Courier => "'Courier New', Courier, monospace",
        }
    }
}

impl Font {
    /// The family the editor shows it in: the first in the stack.
    pub fn family(self) -> &'static str {
        self.css().split(',').next().unwrap_or_default().trim_matches('\'')
    }
}

impl Choice for Size {
    const ALL: &'static [Self] = &[Size::Small, Size::Large, Size::Huge];
    fn css(self) -> &'static str {
        match self {
            Size::Small => "11px",
            Size::Large => "18px",
            Size::Huge => "28px",
        }
    }
}

impl Size {
    /// Normal text, which names no size, is 14px.
    pub const NORMAL_PX: u32 = 14;

    pub fn px(self) -> u32 {
        self.css().trim_end_matches("px").parse().unwrap_or(Self::NORMAL_PX)
    }
}

impl Choice for Color {
    const ALL: &'static [Self] = &[Color::Red, Color::Orange, Color::Green, Color::Teal, Color::Blue, Color::Purple, Color::Pink, Color::Gray];
    fn css(self) -> &'static str {
        match self {
            Color::Red => "#d93b3b",
            Color::Orange => "#bd630f",
            Color::Green => "#2b8b4f",
            Color::Teal => "#13879a",
            Color::Blue => "#3b72e0",
            Color::Purple => "#8a5cd6",
            Color::Pink => "#c64d97",
            Color::Gray => "#7b7b81",
        }
    }
}

impl Choice for Highlight {
    const ALL: &'static [Self] =
        &[Highlight::Yellow, Highlight::Green, Highlight::Blue, Highlight::Pink, Highlight::Orange, Highlight::Purple];
    fn css(self) -> &'static str {
        match self {
            Highlight::Yellow => "#fff1a8",
            Highlight::Green => "#cdf0d4",
            Highlight::Blue => "#d3e5ff",
            Highlight::Pink => "#fbd5e6",
            Highlight::Orange => "#ffdfbd",
            Highlight::Purple => "#e5dafb",
        }
    }
}

/// The styles a `<span style>` carries.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Look {
    font: Option<Font>,
    size: Option<Size>,
    color: Option<Color>,
    highlight: Option<Highlight>,
}

impl Look {
    /// `font-family: …; color: …`, in a fixed order; `ink` adds `INK` as the
    /// color of a highlight that has none.
    fn css(self, ink: bool) -> String {
        let color = self.color.map(Color::css).or((ink && self.highlight.is_some()).then_some(INK));
        [
            self.font.map(|f| format!("font-family: {}", f.css())),
            self.size.map(|s| format!("font-size: {}", s.css())),
            color.map(|c| format!("color: {c}")),
            self.highlight.map(|h| format!("background-color: {}", h.css())),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ")
    }

    /// A style attribute's value, when every declaration in it is one the
    /// format knows, each at most once.
    fn parse(css: &str) -> Option<Look> {
        let mut look = Look::default();
        for (property, value) in declarations(css)? {
            let fresh = match property {
                "font-family" => look.font.replace(Font::from_css(value)?).is_none(),
                "font-size" => look.size.replace(Size::from_css(value)?).is_none(),
                "color" => look.color.replace(Color::from_css(value)?).is_none(),
                "background-color" => look.highlight.replace(Highlight::from_css(value)?).is_none(),
                _ => return None,
            };
            if !fresh {
                return None;
            }
        }
        (look != Look::default()).then_some(look)
    }
}

fn declarations(css: &str) -> Option<Vec<(&str, &str)>> {
    if css.contains(['"', '<', '>']) {
        return None;
    }
    css.split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| d.split_once(':').map(|(p, v)| (p.trim(), v.trim())))
        .collect()
}

/// A line's indent and alignment as the CSS of the `<div>` around it.
fn block_css(indent: u8, align: Align) -> String {
    let margin = (indent > 0).then(|| format!("margin-left: {}px", u32::from(indent) * u32::from(INDENT_PX)));
    let align = match align {
        Align::Left => None,
        Align::Center => Some("text-align: center".to_string()),
        Align::Right => Some("text-align: right".to_string()),
    };
    [margin, align].into_iter().flatten().collect::<Vec<_>>().join("; ")
}

fn parse_block_css(css: &str) -> Option<(u8, Align)> {
    let (mut indent, mut align) = (None, None);
    for (property, value) in declarations(css)? {
        let fresh = match property {
            "margin-left" => {
                let px: u8 = value.strip_suffix("px")?.parse().ok()?;
                let level = px / INDENT_PX;
                if px % INDENT_PX != 0 || !(1..=MAX_INDENT).contains(&level) {
                    return None;
                }
                indent.replace(level).is_none()
            }
            "text-align" => align
                .replace(match value {
                    "center" => Align::Center,
                    "right" => Align::Right,
                    _ => return None,
                })
                .is_none(),
            _ => return None,
        };
        if !fresh {
            return None;
        }
    }
    (indent.is_some() || align.is_some()).then(|| (indent.unwrap_or(0), align.unwrap_or_default()))
}

const DIV_OPEN: &str = "<div style=\"";
const DIV_CLOSE: &str = "</div>";

/// The line wrapped in the `<div>` that carries its indent and alignment.
fn wrap(indent: u8, align: Align, line: &str) -> String {
    match block_css(indent, align) {
        css if css.is_empty() => line.to_string(),
        css => format!("{DIV_OPEN}{css}\">{line}{DIV_CLOSE}"),
    }
}

/// The indent, alignment and inside of a line wrapped in a `<div>`.
fn unwrap(line: &str) -> Option<(u8, Align, &str)> {
    let (css, inner) = line.strip_prefix(DIV_OPEN)?.split_once("\">")?;
    let (indent, align) = parse_block_css(css)?;
    Some((indent, align, inner.strip_suffix(DIV_CLOSE)?))
}

/// What a line is, by its marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Paragraph,
    Bullet,
    Number,
    Quote,
}

/// The line's kind and its marker's length in bytes.
fn marker(line: &str) -> (Kind, usize) {
    if line.starts_with("> ") {
        return (Kind::Quote, 2);
    }
    if line.starts_with('>') {
        return (Kind::Quote, 1);
    }
    if let Some(m) = ["- ", "* ", "• "].iter().find(|m| line.starts_with(**m)) {
        return (Kind::Bullet, m.len());
    }
    match line.split_once(". ") {
        Some((n, _)) if !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit()) => (Kind::Number, n.len() + 2),
        _ => (Kind::Paragraph, 0),
    }
}

/// What a line the editor shows is, and its marker's length in bytes. A
/// marker that the body would escape (`* `) marks nothing.
pub fn line_kind(text: &str) -> (Kind, usize) {
    let (kind, len) = marker(&escape(text));
    (kind, if kind == Kind::Paragraph { 0 } else { len })
}

/// One line of a body, taken apart.
struct Parts<'a> {
    indent: u8,
    align: Align,
    kind: Kind,
    /// The line without its `<div>` and nesting spaces; the marker is kept.
    rest: &'a str,
    marker: usize,
}

fn parts(line: &str) -> Parts<'_> {
    let (mut indent, align, inner) = unwrap(line).unwrap_or((0, Align::Left, line));
    let spaces = inner.len() - inner.trim_start_matches(' ').len();
    let after = &inner[spaces..];
    let nests = spaces >= 2 && spaces % 2 == 0 && spaces <= 2 * usize::from(MAX_INDENT);
    let rest = match marker(after).0 {
        Kind::Bullet | Kind::Number if nests => {
            indent = (indent + (spaces / 2) as u8).min(MAX_INDENT);
            after
        }
        _ => inner,
    };
    let (kind, marker) = marker(rest);
    Parts { indent, align, kind, rest, marker }
}

const SPECIAL: &[char] = &['\\', '*', '_', '~', '[', ']'];

/// Text made safe to put in a body as is: no character in it formats. A `<`
/// needs escaping only where it starts a tag the format knows.
pub fn escape(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if escapable(&chars, i) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether a backslash before `chars[i]` escapes it.
fn escapable(chars: &[char], i: usize) -> bool {
    SPECIAL.contains(&chars[i]) || chars[i] == '<' && tag_len(chars, i).is_some()
}

/// The length of a tag the format knows at `i`, opening or closing.
fn tag_len(chars: &[char], i: usize) -> Option<usize> {
    if let Some((_, len)) = tag(chars, i) {
        return Some(len);
    }
    if starts(chars, i, DIV_CLOSE) {
        return Some(DIV_CLOSE.len());
    }
    let rest: String = chars[i..].iter().collect();
    let (css, _) = rest.strip_prefix(DIV_OPEN)?.split_once("\">")?;
    parse_block_css(css).map(|_| DIV_OPEN.len() + css.chars().count() + 2)
}

fn starts(chars: &[char], i: usize, s: &str) -> bool {
    s.chars().enumerate().all(|(j, c)| chars.get(i + j) == Some(&c))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Bold,
    Italic,
    Strike,
    Underline,
    Look,
}

enum Tag {
    Open(Mark, Look),
    Close(Mark),
}

const SPAN_OPEN: &str = "<span style=\"";

/// An inline tag at `i` and its length.
fn tag(chars: &[char], i: usize) -> Option<(Tag, usize)> {
    for (s, t) in [("<u>", Tag::Open(Mark::Underline, Look::default())), ("</u>", Tag::Close(Mark::Underline)), ("</span>", Tag::Close(Mark::Look))] {
        if starts(chars, i, s) {
            return Some((t, s.len()));
        }
    }
    if !starts(chars, i, SPAN_OPEN) {
        return None;
    }
    let from = i + SPAN_OPEN.len();
    let end = (from..chars.len()).find(|&j| chars[j] == '"')?;
    if chars.get(end + 1) != Some(&'>') {
        return None;
    }
    let look = Look::parse(&chars[from..end].iter().collect::<String>())?;
    Some((Tag::Open(Mark::Look, look), end + 2 - i))
}

/// Whether the closing tag for `mark` follows `from` on the line.
fn has_tag_closer(chars: &[char], from: usize, mark: Mark) -> bool {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if matches!(tag(chars, i), Some((Tag::Close(m), _)) if m == mark) {
            return true;
        }
        i += 1;
    }
    false
}

/// The lines of a body, for the editor. List and quote markers stay text.
pub fn lines(body: &str) -> Vec<Line> {
    body.split('\n')
        .map(|line| {
            let p = parts(line);
            let mut spans = Vec::new();
            for s in inline(p.rest, &Span::default()) {
                push(&mut spans, s);
            }
            Line { indent: p.indent, align: p.align, spans }
        })
        .collect()
}

/// The body for these lines. A newline in a span starts a new line with the
/// same indent and alignment.
pub fn body(lines: &[Line]) -> String {
    let mut out = Vec::new();
    for line in lines {
        let mut current: Vec<Span> = Vec::new();
        for s in &line.spans {
            if s.image.is_some() {
                current.push(s.clone());
                continue;
            }
            for (i, text) in s.text.split('\n').enumerate() {
                if i > 0 {
                    out.push(write_line(line, &current));
                    current.clear();
                }
                current.push(Span { text: text.into(), ..s.clone() });
            }
        }
        out.push(write_line(line, &current));
    }
    out.join("\n")
}

fn write_line(line: &Line, spans: &[Span]) -> String {
    let text = markdown(spans);
    let indent = line.indent.min(MAX_INDENT);
    match marker(&text).0 {
        Kind::Bullet | Kind::Number => wrap(0, line.align, &format!("{}{text}", "  ".repeat(indent.into()))),
        _ => wrap(indent, line.align, &text),
    }
}

/// One line's spans in the body format. Markers and tags leave surrounding
/// spaces outside.
fn markdown(spans: &[Span]) -> String {
    let mut merged: Vec<Span> = Vec::new();
    for s in spans.iter().filter(|s| !s.text.is_empty()) {
        match merged.last_mut() {
            Some(last) if last.same_style(s) => last.text.push_str(&s.text),
            _ => merged.push(s.clone()),
        }
    }
    let mut out = String::new();
    for span in &merged {
        if let Some(path) = &span.image {
            out.push_str(&format!("![{}]({})", escape(&span.text.replace('\n', " ")), file_url(path)));
            continue;
        }
        let core = span.text.trim_matches(' ');
        if core.is_empty() {
            out.push_str(&span.text);
            continue;
        }
        let lead = &span.text[..span.text.len() - span.text.trim_start_matches(' ').len()];
        let trail = &span.text[span.text.trim_end_matches(' ').len()..];
        let mut s = escape(core);
        // `***a***` reads back wrong; inside `**`, `_` can't touch a letter.
        if span.italic {
            s = if span.bold { format!("_{s}_") } else { format!("*{s}*") };
        }
        if span.bold {
            s = format!("**{s}**");
        }
        if span.strike {
            s = format!("~~{s}~~");
        }
        if span.underline {
            s = format!("<u>{s}</u>");
        }
        let look = span.look();
        if look != Look::default() {
            s = format!("{SPAN_OPEN}{}\">{s}</span>", look.css(false));
        }
        if let Some(url) = &span.link {
            s = format!("[{s}]({})", url.replace(')', "%29").replace(' ', "%20"));
        }
        out.push_str(lead);
        out.push_str(&s);
        out.push_str(trail);
    }
    out
}

/// The span with every style gone but its link or image.
pub fn clear(span: &Span) -> Span {
    Span { text: span.text.clone(), link: span.link.clone(), image: span.image.clone(), ..Default::default() }
}

/// An image's path as the URL a body names it by.
fn file_url(path: &str) -> String {
    reqwest::Url::from_file_path(path).map(|u| u.as_str().replace(')', "%29")).unwrap_or_default()
}

/// The path a `file:` URL names.
fn file_path(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok().filter(|u| u.scheme() == "file")?;
    Some(url.to_file_path().ok()?.to_str()?.to_string())
}

fn unescape(chars: &[char]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && escapable(chars, i + 1) {
            i += 1;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn push(out: &mut Vec<Span>, s: Span) {
    if s.text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(last) if last.same_style(&s) => last.text.push_str(&s.text),
        _ => out.push(s),
    }
}

/// Parses one line. A marker formats only when it has a partner on the same
/// line and hugs the text (`*a*`, not `* a`); otherwise it is literal. So is
/// a tag with no closing tag after it.
fn inline(line: &str, base: &Span) -> Vec<Span> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut style = base.clone();
    let mut text = String::new();
    // Each open marker or tag, with the font, size and colors before it.
    let mut open: Vec<(Mark, Look)> = Vec::new();
    let mut i = 0;
    let flush = |text: &mut String, style: &Span, out: &mut Vec<Span>| {
        if !text.is_empty() {
            out.push(Span { text: std::mem::take(text), ..style.clone() });
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && escapable(&chars, i + 1) {
            text.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '!' && chars.get(i + 1) == Some(&'[') {
            let image = link_at(&chars, i + 1).and_then(|(end, url, next)| Some((end, file_path(&url)?, next)));
            if let Some((label_end, path, next)) = image {
                flush(&mut text, &style, &mut out);
                let name = unescape(&chars[i + 2..label_end]);
                out.push(Span { text: if name.is_empty() { "image".into() } else { name }, image: Some(path), ..Default::default() });
                i = next;
                continue;
            }
        }
        if c == '[' {
            if let Some((label_end, url, next)) = link_at(&chars, i) {
                flush(&mut text, &style, &mut out);
                let label: String = chars[i + 1..label_end].iter().collect();
                let inner = Span { link: Some(url), ..style.clone() };
                out.extend(inline(&label, &inner));
                i = next;
                continue;
            }
        }
        if c == '<' {
            match tag(&chars, i) {
                Some((Tag::Open(mark, look), len)) if has_tag_closer(&chars, i + len, mark) => {
                    flush(&mut text, &style, &mut out);
                    let before = style.look();
                    open.push((mark, before));
                    match mark {
                        Mark::Underline => style.underline = true,
                        _ => style.set_look(Look {
                            font: look.font.or(before.font),
                            size: look.size.or(before.size),
                            color: look.color.or(before.color),
                            highlight: look.highlight.or(before.highlight),
                        }),
                    }
                    i += len;
                    continue;
                }
                Some((Tag::Close(mark), len)) if open.last().is_some_and(|(m, _)| *m == mark) => {
                    flush(&mut text, &style, &mut out);
                    let (_, before) = open.pop().unwrap_or((mark, Look::default()));
                    close(&mut style, mark, base, before);
                    i += len;
                    continue;
                }
                _ => {}
            }
        }
        let (mark, len) = match c {
            '*' if chars.get(i + 1) == Some(&'*') => (Some(Mark::Bold), 2),
            '~' if chars.get(i + 1) == Some(&'~') => (Some(Mark::Strike), 2),
            '*' => (Some(Mark::Italic), 1),
            '_' => (Some(Mark::Italic), 1),
            _ => (None, 1),
        };
        if let Some(mark) = mark {
            let before = i.checked_sub(1).map(|j| chars[j]);
            let after = chars.get(i + len).copied();
            let intraword = c == '_' && before.is_some_and(char::is_alphanumeric) && after.is_some_and(char::is_alphanumeric);
            let closes = open.last().is_some_and(|(m, _)| *m == mark) && before.is_some_and(|b| !b.is_whitespace());
            let opens = !closes
                && !intraword
                && after.is_some_and(|a| !a.is_whitespace())
                && has_closer(&chars, i + len, c, len);
            if closes || opens {
                flush(&mut text, &style, &mut out);
                if closes {
                    open.pop();
                    close(&mut style, mark, base, Look::default());
                } else {
                    open.push((mark, style.look()));
                    match mark {
                        Mark::Bold => style.bold = true,
                        Mark::Italic => style.italic = true,
                        _ => style.strike = true,
                    }
                }
                i += len;
                continue;
            }
        }
        text.push(c);
        i += 1;
    }
    flush(&mut text, &style, &mut out);
    out
}

/// Ends `mark`'s style: a marker's back to what the line started with (so
/// `**a **b** c**` reads as it always has), a span's back to `before`.
fn close(style: &mut Span, mark: Mark, base: &Span, before: Look) {
    match mark {
        Mark::Bold => style.bold = base.bold,
        Mark::Italic => style.italic = base.italic,
        Mark::Strike => style.strike = base.strike,
        Mark::Underline => style.underline = base.underline,
        Mark::Look => style.set_look(before),
    }
}

/// Whether a closing marker like the one at `from - len` follows on the line.
fn has_closer(chars: &[char], from: usize, c: char, len: usize) -> bool {
    let mut i = from;
    while i + len <= chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        let run = chars[i..i + len].iter().all(|&x| x == c);
        let longer = run && len == 1 && c == '*' && chars.get(i + 1) == Some(&'*');
        if run && !longer && i > from && !chars[i - 1].is_whitespace() {
            return true;
        }
        if longer {
            i += 2;
            continue;
        }
        i += 1;
    }
    false
}

/// `[label](url)` starting at `start`: the label's end, the URL, and where
/// parsing continues.
fn link_at(chars: &[char], start: usize) -> Option<(usize, String, usize)> {
    let mut depth = 0;
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '[' => depth += 1,
            ']' if depth == 0 => break,
            ']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    let label_end = i;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let url_start = label_end + 2;
    let close = (url_start..chars.len()).find(|&j| chars[j] == ')')?;
    let url: String = chars[url_start..close].iter().collect();
    if url.is_empty() || url.contains(char::is_whitespace) {
        return None;
    }
    Some((label_end, url, close + 1))
}

/// The plain-text part: styles gone, links as `text (url)`, quotes and list
/// markers kept as typed, and four spaces per level of indent or nesting.
pub fn to_text(body: &str) -> String {
    body.split('\n')
        .map(|line| {
            let p = parts(line);
            let text: String = inline(p.rest, &Span::default())
                .into_iter()
                .map(|s| match s.link {
                    _ if s.image.is_some() => format!("[image: {}]", s.text),
                    Some(url) if url != s.text && url.trim_start_matches("mailto:") != s.text => format!("{} ({url})", s.text),
                    _ => s.text,
                })
                .collect();
            format!("{}{text}", "    ".repeat(p.indent.into()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The images a body shows, as (name, path), in order.
pub fn images(body: &str) -> Vec<(String, String)> {
    lines(body).into_iter().flat_map(|l| l.spans).filter_map(|s| Some((s.text, s.image?))).collect()
}

/// The HTML part: paragraphs, nested lists, quotes and inline styles, with
/// bare web addresses made into links. `image` writes the tag for an image span.
pub fn to_html(body: &str, image: &dyn Fn(&Span) -> String) -> String {
    let lines: Vec<&str> = body.split('\n').collect();
    format!("<div style=\"font-family: -apple-system, Helvetica, Arial, sans-serif; font-size: 14px; line-height: 1.5\">{}</div>", blocks(&lines, image))
}

fn blocks(lines: &[&str], image: &dyn Fn(&Span) -> String) -> String {
    let all: Vec<Parts> = lines.iter().map(|l| parts(l)).collect();
    let mut html = String::new();
    let mut i = 0;
    while i < all.len() {
        let start = i;
        match all[i].kind {
            Kind::Quote => {
                let mut inner = Vec::new();
                while i < all.len() && all[i].kind == Kind::Quote {
                    let p = &all[i];
                    inner.push(wrap(p.indent, p.align, &p.rest[p.marker..]));
                    i += 1;
                }
                let inner: Vec<&str> = inner.iter().map(String::as_str).collect();
                html.push_str(&format!(
                    "<blockquote style=\"margin: 0 0 0 0.8ex; border-left: 1px solid #ccc; padding-left: 1ex; color: #555\">{}</blockquote>",
                    blocks(&inner, image)
                ));
            }
            Kind::Bullet | Kind::Number => {
                while i < all.len() && matches!(all[i].kind, Kind::Bullet | Kind::Number) {
                    i += 1;
                }
                html.push_str(&list(&all[start..i], image));
            }
            Kind::Paragraph => {
                // One div per line, as Gmail writes them; an empty line keeps its height.
                let p = &all[i];
                let h = inline_html(p.rest, image);
                let css = block_css(p.indent, p.align);
                let attr = if css.is_empty() { String::new() } else { format!(" style=\"{css}\"") };
                html.push_str(&format!("<div{attr}>{}</div>", if h.trim().is_empty() { "<br>".into() } else { h }));
                i += 1;
            }
        }
    }
    html
}

/// A run of list items as nested lists. An item nests at most one level
/// deeper than the one before it.
fn list(items: &[Parts], image: &dyn Fn(&Span) -> String) -> String {
    let tag = |k: Kind| if k == Kind::Number { "ol" } else { "ul" };
    let mut html = String::new();
    let mut open: Vec<Kind> = Vec::new();
    for item in items {
        let depth = (usize::from(item.indent) + 1).min(open.len() + 1);
        while open.len() > depth {
            html.push_str(&format!("</li></{}>", tag(open.pop().unwrap_or(Kind::Bullet))));
        }
        if open.len() == depth && open.last() != Some(&item.kind) {
            html.push_str(&format!("</li></{}>", tag(open.pop().unwrap_or(Kind::Bullet))));
        }
        if open.len() == depth {
            html.push_str("</li>");
        } else {
            html.push_str(&format!("<{}>", tag(item.kind)));
            open.push(item.kind);
        }
        let attr = match item.align {
            Align::Left => String::new(),
            align => format!(" style=\"{}\"", block_css(0, align)),
        };
        html.push_str(&format!("<li{attr}>{}", inline_html(&item.rest[item.marker..], image)));
    }
    while let Some(kind) = open.pop() {
        html.push_str(&format!("</li></{}>", tag(kind)));
    }
    html
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn inline_html(line: &str, image: &dyn Fn(&Span) -> String) -> String {
    let mut out = String::new();
    for s in inline(line, &Span::default()) {
        if s.image.is_some() {
            out.push_str(&image(&s));
            continue;
        }
        let mut h = match &s.link {
            Some(_) => html_escape(&s.text),
            None => autolink(&s.text),
        };
        if s.italic {
            h = format!("<i>{h}</i>");
        }
        if s.bold {
            h = format!("<b>{h}</b>");
        }
        if s.strike {
            h = format!("<s>{h}</s>");
        }
        if s.underline {
            h = format!("<u>{h}</u>");
        }
        let look = s.look();
        if look != Look::default() {
            h = format!("<span style=\"{}\">{h}</span>", look.css(true));
        }
        if let Some(url) = &s.link {
            h = format!("<a href=\"{}\">{h}</a>", html_escape(url));
        }
        out.push_str(&h);
    }
    out
}

/// Escapes text and links the `http://` and `https://` addresses in it.
fn autolink(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = ["https://", "http://"].iter().filter_map(|p| rest.find(p)).min() {
        out.push_str(&html_escape(&rest[..start]));
        let tail = &rest[start..];
        let mut end = tail.find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"').unwrap_or(tail.len());
        while end > 0 && tail[..end].ends_with(['.', ',', ')', ';', ':', '!', '?']) {
            end -= 1;
        }
        let url = html_escape(&tail[..end]);
        out.push_str(&format!("<a href=\"{url}\">{url}</a>"));
        rest = &tail[end..];
    }
    out.push_str(&html_escape(rest));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str) -> Span {
        Span { text: text.into(), ..Default::default() }
    }

    fn styled(text: &str, style: impl FnOnce(&mut Span)) -> Span {
        let mut s = span(text);
        style(&mut s);
        s
    }

    fn line(spans: Vec<Span>) -> Line {
        Line { spans, ..Default::default() }
    }

    /// The lines as one run per style, joined by newlines, as the editor
    /// read bodies before lines had styles.
    fn flat(lines: &[Line]) -> Vec<Span> {
        let mut out = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                push(&mut out, span("\n"));
            }
            for s in &l.spans {
                push(&mut out, s.clone());
            }
        }
        out
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.text.as_str()).collect()).collect()
    }

    fn to_html_plain(body: &str) -> String {
        to_html(body, &|_| unreachable!("no images"))
    }

    fn img(s: &Span) -> String {
        format!("<img alt=\"{}\">", html_escape(&s.text))
    }

    /// Today's bodies were read by the code at c0a7107 to make the corpus:
    /// fixtures as drafts and replies, bodies the editor wrote and Markdown as
    /// an agent writes it. None uses a tag or nesting the format gained since.
    /// Each reads as it did; each is written back as it was, except bold
    /// italic text, which is now written so that it reads back.
    #[test]
    fn bodies_written_before_styles_read_and_write_as_they_did() {
        let corpus = include_str!("../tests/fixtures/rich/bodies-before-styles.jsonl");
        let (mut count, mut fixed) = (0, 0);
        for case in corpus.lines() {
            let case: serde_json::Value = serde_json::from_str(case).unwrap();
            let body = case["body"].as_str().unwrap();
            let before: Vec<Span> = case["spans"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| Span {
                    text: s["text"].as_str().unwrap().into(),
                    bold: s["bold"].as_bool().unwrap(),
                    italic: s["italic"].as_bool().unwrap(),
                    strike: s["strike"].as_bool().unwrap(),
                    link: s["link"].as_str().map(Into::into),
                    image: s["image"].as_str().map(Into::into),
                    ..Default::default()
                })
                .collect();
            let read = lines(body);
            assert!(read.iter().all(|l| l.indent == 0 && l.align == Align::Left), "{body:?}");
            assert_eq!(flat(&read), before, "{body:?}");
            let again = super::body(&read);
            if before.iter().any(|s| s.bold && s.italic) {
                // Written as `***a***` before, which read back with stray
                // asterisks; `each_inline_style_…` checks the new way.
                fixed += 1;
            } else {
                assert_eq!(again, case["again"].as_str().unwrap(), "{body:?}");
            }
            assert_eq!(to_text(body), case["text"].as_str().unwrap(), "{body:?}");
            assert_eq!(to_html(body, &img), case["html"].as_str().unwrap(), "{body:?}");
            count += 1;
        }
        assert!(count > 250 && fixed < count / 4, "the corpus is all there: {count}, {fixed} with bold italic");
    }

    #[test]
    fn a_body_the_editor_wrote_comes_back_byte_for_byte() {
        let signature = "<div style=\"text-align: center\"><span style=\"font-family: Georgia, serif\">Ann</span></div>\n<span style=\"color: #7b7b81\">Example Ltd</span>";
        let written = format!(
            "<u>Thanks</u>, see:\n- **one**\n  1. <span style=\"background-color: #fff1a8\">two</span>\n<div style=\"margin-left: 40px\">aside</div>\n\n{signature}\n\nOn Monday, Bob wrote:\n> hi\n> > older"
        );
        assert_eq!(body(&lines(&written)), written);
    }

    #[test]
    fn each_inline_style_is_written_as_inline_html_and_read_back() {
        let cases = [
            (styled("under", |s| s.underline = true), "<u>under</u>"),
            (styled("big", |s| s.size = Some(Size::Large)), "<span style=\"font-size: 18px\">big</span>"),
            (styled("serif", |s| s.font = Some(Font::Georgia)), "<span style=\"font-family: Georgia, serif\">serif</span>"),
            (styled("red", |s| s.color = Some(Color::Red)), "<span style=\"color: #d93b3b\">red</span>"),
            (styled("marked", |s| s.highlight = Some(Highlight::Yellow)), "<span style=\"background-color: #fff1a8\">marked</span>"),
            (
                styled("all", |s| {
                    (s.bold, s.italic, s.strike, s.underline) = (true, true, true, true);
                    s.font = Some(Font::Times);
                    s.size = Some(Size::Huge);
                    s.color = Some(Color::Blue);
                    s.highlight = Some(Highlight::Green);
                    s.link = Some("https://x.test".into());
                }),
                "[<span style=\"font-family: 'Times New Roman', Times, serif; font-size: 28px; color: #3b72e0; background-color: #cdf0d4\"><u>~~**_all_**~~</u></span>](https://x.test)",
            ),
        ];
        for (s, written) in cases {
            assert_eq!(body(&[line(vec![s.clone()])]), written);
            assert_eq!(lines(written), vec![line(vec![s])], "{written}");
        }
        let mut every = Vec::new();
        every.extend(Font::ALL.iter().map(|&f| styled("font", |s| s.font = Some(f))));
        every.extend(Size::ALL.iter().map(|&z| styled("size", |s| s.size = Some(z))));
        every.extend(Color::ALL.iter().map(|&c| styled("color", |s| s.color = Some(c))));
        every.extend(Highlight::ALL.iter().map(|&h| styled("highlight", |s| s.highlight = Some(h))));
        let doc: Vec<Line> = every.into_iter().map(|s| line(vec![span("a "), s, span(" b")])).collect();
        assert_eq!(lines(&body(&doc)), doc);
    }

    #[test]
    fn styles_mix_within_a_line() {
        assert_eq!(
            lines("Plain <u>under **bold**</u> <span style=\"color: #d93b3b\">red <span style=\"font-size: 11px\">small</span></span>."),
            vec![line(vec![
                span("Plain "),
                styled("under ", |s| s.underline = true),
                styled("bold", |s| (s.underline, s.bold) = (true, true)),
                span(" "),
                styled("red ", |s| s.color = Some(Color::Red)),
                styled("small", |s| (s.color, s.size) = (Some(Color::Red), Some(Size::Small))),
                span("."),
            ])]
        );
    }

    #[test]
    fn line_styles_are_written_per_line_and_read_back() {
        let doc = vec![
            Line { indent: 1, spans: vec![span("indented")], ..Default::default() },
            Line { align: Align::Center, spans: vec![span("centred")], ..Default::default() },
            Line { indent: 2, align: Align::Right, spans: vec![span("both")] },
            line(vec![span("- top")]),
            Line { indent: 1, spans: vec![span("- nested")], ..Default::default() },
            Line { indent: 2, spans: vec![span("1. deeper")], ..Default::default() },
            Line { indent: 1, align: Align::Center, spans: vec![span("• centred item")] },
            Line { indent: 1, spans: vec![span("> indented quote")], ..Default::default() },
            Line { align: Align::Center, ..Default::default() },
        ];
        let written = body(&doc);
        assert_eq!(
            written,
            "<div style=\"margin-left: 40px\">indented</div>\n<div style=\"text-align: center\">centred</div>\n\
             <div style=\"margin-left: 80px; text-align: right\">both</div>\n- top\n  - nested\n    1. deeper\n\
             <div style=\"text-align: center\">  • centred item</div>\n<div style=\"margin-left: 40px\">> indented quote</div>\n\
             <div style=\"text-align: center\"></div>"
        );
        assert_eq!(lines(&written), doc);
    }

    #[test]
    fn markdown_lists_nest_by_two_spaces() {
        let read = lines("- fruit\n  - apple\n    - green\n- veg\n  1. leek");
        assert_eq!(read.iter().map(|l| l.indent).collect::<Vec<_>>(), [0, 1, 2, 0, 1]);
        assert_eq!(texts(&read), ["- fruit", "- apple", "- green", "- veg", "1. leek"]);
        for kept in ["   - three spaces", "          - five levels", "  indented text", "  > not a quote"] {
            assert_eq!(lines(kept), vec![line(vec![span(kept)])], "{kept}");
        }
    }

    #[test]
    fn html_part_carries_every_style() {
        let html = to_html_plain(
            "<u>under</u> <span style=\"font-family: Georgia, serif\">serif</span> <span style=\"font-size: 18px\">big</span>\n\
             <span style=\"color: #d93b3b\">red</span> <span style=\"background-color: #fff1a8\">marked</span> \
             <span style=\"color: #3b72e0; background-color: #fff1a8\">both</span> [<span style=\"color: #d93b3b\">red link</span>](https://x.test)\n\
             <div style=\"margin-left: 40px\">indented</div>\n<div style=\"text-align: center\">centred</div>\n\
             - fruit\n  - apple\n    1. one\n    2. two\n<div style=\"text-align: right\">- veg</div>\n\
             > quoted\n<div style=\"margin-left: 40px\">> indented quote</div>",
        );
        for part in [
            "<u>under</u>",
            "<span style=\"font-family: Georgia, serif\">serif</span>",
            "<span style=\"font-size: 18px\">big</span>",
            "<span style=\"color: #d93b3b\">red</span>",
            "<span style=\"color: #1f1f1f; background-color: #fff1a8\">marked</span>",
            "<span style=\"color: #3b72e0; background-color: #fff1a8\">both</span>",
            "<a href=\"https://x.test\"><span style=\"color: #d93b3b\">red link</span></a>",
            "<div style=\"margin-left: 40px\">indented</div>",
            "<div style=\"text-align: center\">centred</div>",
            "<ul><li>fruit<ul><li>apple<ol><li>one</li><li>two</li></ol></li></ul></li><li style=\"text-align: right\">veg</li></ul>",
            "<div>quoted</div><div style=\"margin-left: 40px\">indented quote</div></blockquote>",
        ] {
            assert!(html.contains(part), "{part}\nin {html}");
        }
    }

    #[test]
    fn a_list_nests_at_most_one_level_at_a_time() {
        let html = to_html_plain("- a\n      - three deeper\n- b");
        assert!(html.contains("<ul><li>a<ul><li>three deeper</li></ul></li><li>b</li></ul>"), "{html}");
    }

    #[test]
    fn text_part_keeps_nesting_and_indent_and_drops_styles() {
        let body = "<div style=\"margin-left: 40px\"><u>indented</u></div>\n- fruit\n  - <span style=\"color: #d93b3b\">apple</span>\n    1. one\n<div style=\"text-align: center\">centred</div>";
        assert_eq!(to_text(body), "    indented\n- fruit\n    - apple\n        1. one\ncentred");
    }

    #[test]
    fn styles_outside_the_lists_stay_text() {
        for raw in [
            "<span style=\"color: red\">x</span>",
            "<span style=\"color: #d93b3b; position: fixed\">x</span>",
            "<span style=\"color: #d93b3b; color: #3b72e0\">x</span>",
            "<span style=\"font-family: Comic Sans MS\">x</span>",
            "<span style=\"font-size: 13px\">x</span>",
            "<span style=\"background: url(p.gif)\">x</span>",
            "<span style=\"color: #d93b3b\" onclick=\"alert(1)\">x</span>",
            "<span style=\"\">x</span>",
            "<span class=\"x\">x</span>",
            "<u class=\"x\">x</u>",
            "<b>x</b> <script>y</script>",
            "<span style=\"color: #d93b3b\">never closed",
            "<div style=\"margin-left: 1000px\">x</div>",
            "<div style=\"margin-left: 200px\">x</div>",
            "<div style=\"margin-left: 30px\">x</div>",
            "<div style=\"text-align: justify\">x</div>",
            "<div style=\"margin-left: 40px; color: red\">x</div>",
            "<div style=\"margin-left: 40px\">x",
        ] {
            assert_eq!(lines(raw), vec![line(vec![span(raw)])], "{raw}");
            let html = to_html_plain(raw);
            assert!(html.contains(&html_escape(raw)), "{raw} shows as text: {html}");
        }
    }

    #[test]
    fn typed_tags_stay_text() {
        let raw = "<u>x</u> <span style=\"color: #d93b3b\">y</span> <div style=\"text-align: center\">z</div> a < b";
        assert_eq!(lines(&escape(raw)), vec![line(vec![span(raw)])]);
        let doc = vec![line(vec![span(raw)])];
        assert_eq!(lines(&body(&doc)), doc);
        let centred = vec![Line { align: Align::Center, spans: vec![span("<div style=\"text-align: right\">x</div>")], ..Default::default() }];
        assert_eq!(lines(&body(&centred)), centred);
        assert_eq!(escape("a < b <b> <u"), "a < b <b> <u", "a < that starts no known tag stays as is");
    }

    #[test]
    fn clearing_formatting_keeps_only_links_and_images() {
        let all = styled("x", |s| {
            (s.bold, s.italic, s.strike, s.underline) = (true, true, true, true);
            (s.font, s.size, s.color, s.highlight) = (Some(Font::Courier), Some(Size::Small), Some(Color::Gray), Some(Highlight::Pink));
            s.link = Some("https://x.test".into());
        });
        assert_eq!(clear(&all), Span { text: "x".into(), link: Some("https://x.test".into()), ..Default::default() });
        let image = Span { text: "a.png".into(), image: Some("/tmp/a.png".into()), ..Default::default() };
        assert_eq!(clear(&image), image);
    }

    #[test]
    fn line_kinds_as_the_editor_shows_them() {
        assert_eq!(line_kind("- a"), (Kind::Bullet, 2));
        assert_eq!(line_kind("• a"), (Kind::Bullet, "• ".len()));
        assert_eq!(line_kind("12. a"), (Kind::Number, 4));
        assert_eq!(line_kind("> a"), (Kind::Quote, 2));
        assert_eq!(line_kind(">a"), (Kind::Quote, 1));
        assert_eq!(line_kind("* a"), (Kind::Paragraph, 0), "the body escapes it, so it marks nothing");
        assert_eq!(line_kind("1.5 kg"), (Kind::Paragraph, 0));
    }

    #[test]
    fn images_round_trip_and_stand_in_for_themselves_as_text() {
        let path = "/Users/me/Library/Application Support/Jolta/images/ab (1).png";
        let image = Span { text: "Pasted image.png".into(), image: Some(path.into()), ..Default::default() };
        let original = vec![line(vec![span("Look:")]), line(vec![image.clone(), image.clone()]), line(vec![span("bye!")])];
        let md = body(&original);
        assert_eq!(lines(&md), original, "{md}");
        assert_eq!(to_text(&md), "Look:\n[image: Pasted image.png][image: Pasted image.png]\nbye!");
        assert_eq!(images(&md), vec![("Pasted image.png".to_string(), path.to_string()); 2]);
        let html = to_html(&md, &img);
        assert!(html.contains("<div><img alt=\"Pasted image.png\"><img alt=\"Pasted image.png\"></div>"), "{html}");
    }

    #[test]
    fn only_a_local_file_is_an_image() {
        let link = Span { text: "site".into(), link: Some("https://x.test/a.png".into()), ..Default::default() };
        assert_eq!(lines("Wow![site](https://x.test/a.png)"), vec![line(vec![span("Wow!"), link])]);
        assert!(images("Wow![site](https://x.test/a.png)").is_empty());
        assert_eq!(lines("Hello! [x]"), vec![line(vec![span("Hello! [x]")])]);
    }

    #[test]
    fn spans_round_trip_through_the_body() {
        let original = vec![
            span("Hi Ann,\n\nThe "),
            styled("new price", |s| s.bold = true),
            span(" is "),
            styled("5 * 3 = 15", |s| s.italic = true),
            span(" EUR, see "),
            styled("the offer", |s| s.link = Some("https://shop.test/a_b(1)".into())),
            span(".\n"),
            styled("old line\nsecond", |s| (s.strike, s.bold) = (true, true)),
            span("\nsnake_case and a \\ backslash"),
        ];
        let md = body(&[line(original.clone())]);
        let back = flat(&lines(&md));
        assert_eq!(back.iter().map(|s| s.text.as_str()).collect::<String>(), original.iter().map(|s| s.text.as_str()).collect::<String>());
        assert!(back.iter().any(|s| s.bold && s.text == "new price"), "{md}");
        assert!(back.iter().any(|s| s.italic && s.text == "5 * 3 = 15"), "{md}");
        assert!(back.iter().any(|s| s.link.as_deref() == Some("https://shop.test/a_b(1%29") && s.text == "the offer"), "{md}");
        assert!(back.iter().any(|s| s.strike && s.bold && s.text == "second"), "{md}");
    }

    #[test]
    fn spaces_stay_outside_markers() {
        let md = body(&[line(vec![span("a"), styled(" bold ", |s| s.bold = true), styled(" under ", |s| s.underline = true), span("b")])]);
        assert_eq!(md, "a **bold**  <u>under</u> b");
    }

    #[test]
    fn plain_text_is_not_mistaken_for_formatting() {
        let body = "5 * 3 and 2 * 4, my_var_name, ** alone, a<u, x </u> y";
        assert_eq!(lines(body), vec![line(vec![span(body)])]);
        assert_eq!(to_text(body), body);
    }

    #[test]
    fn text_part_drops_markers_and_spells_out_links() {
        assert_eq!(to_text("**Hi** *there*, see [the page](https://x.test)\n> quoted"), "Hi there, see the page (https://x.test)\n> quoted");
        assert_eq!(to_text("[https://x.test](https://x.test)"), "https://x.test");
    }

    #[test]
    fn html_part_has_lists_quotes_and_links() {
        let html = to_html_plain("Hi <you> & **all**\n\n- one\n- two\n1. first\n> On Monday Ann wrote:\n> > deep\nvisit https://x.test/a?b=1.");
        assert!(html.contains("Hi &lt;you&gt; &amp; <b>all</b>"), "{html}");
        assert!(html.contains("<ul><li>one</li><li>two</li></ul><ol><li>first</li></ol>"), "{html}");
        assert!(html.contains("<blockquote") && html.matches("<blockquote").count() == 2, "{html}");
        assert!(html.contains("<a href=\"https://x.test/a?b=1\">https://x.test/a?b=1</a>."), "{html}");
        assert!(html.contains("<div><br></div>"), "blank lines keep their space: {html}");
    }

    #[test]
    fn escaped_text_stays_literal() {
        let raw = "*not italic* and [not](a link)";
        assert_eq!(lines(&escape(raw)), vec![line(vec![span(raw)])]);
        assert_eq!(to_text(&escape(raw)), raw);
    }
}
