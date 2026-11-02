//! Formatted message bodies. A body is stored and passed around as a small
//! Markdown subset: `**bold**`, `*italic*`, `~~struck~~`, `[text](url)`,
//! and per line `- ` or `1. ` list items and `> ` quotes. The editor shows it
//! as formatted text through `spans`; sending turns it into an HTML part and
//! a plain-text part.

/// A run of text with one set of styles, as the editor shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub link: Option<String>,
}

impl Span {
    fn same_style(&self, other: &Span) -> bool {
        self.bold == other.bold && self.italic == other.italic && self.strike == other.strike && self.link == other.link
    }
}

const SPECIAL: &[char] = &['\\', '*', '_', '~', '[', ']'];

/// Text made safe to put in a body as is: no character in it formats.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The body for these spans. Markers wrap each line separately and leave
/// surrounding spaces outside, so every line parses on its own.
pub fn markdown(spans: &[Span]) -> String {
    let mut merged: Vec<Span> = Vec::new();
    for s in spans.iter().filter(|s| !s.text.is_empty()) {
        match merged.last_mut() {
            Some(last) if last.same_style(s) => last.text.push_str(&s.text),
            _ => merged.push(s.clone()),
        }
    }
    let mut out = String::new();
    for span in &merged {
        for (i, line) in span.text.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let core = line.trim_matches(' ');
            if core.is_empty() {
                out.push_str(line);
                continue;
            }
            let lead = &line[..line.len() - line.trim_start_matches(' ').len()];
            let trail = &line[line.trim_end_matches(' ').len()..];
            let mut s = escape(core);
            if span.italic {
                s = format!("*{s}*");
            }
            if span.bold {
                s = format!("**{s}**");
            }
            if span.strike {
                s = format!("~~{s}~~");
            }
            if let Some(url) = &span.link {
                s = format!("[{s}]({})", url.replace(')', "%29").replace(' ', "%20"));
            }
            out.push_str(lead);
            out.push_str(&s);
            out.push_str(trail);
        }
    }
    out
}

/// The spans of a body, for the editor. Line and list markers stay text.
pub fn spans(body: &str) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            push(&mut out, Span { text: "\n".into(), ..Default::default() });
        }
        for s in inline(line, &Span::default()) {
            push(&mut out, s);
        }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Bold,
    Italic,
    Strike,
}

/// Parses one line. A marker formats only when it has a partner on the same
/// line and hugs the text (`*a*`, not `* a`); otherwise it is literal.
fn inline(line: &str, base: &Span) -> Vec<Span> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut style = base.clone();
    let mut text = String::new();
    let mut open: Vec<Mark> = Vec::new();
    let mut i = 0;
    let flush = |text: &mut String, style: &Span, out: &mut Vec<Span>| {
        if !text.is_empty() {
            out.push(Span { text: std::mem::take(text), ..style.clone() });
        }
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && SPECIAL.contains(&chars[i + 1]) {
            text.push(chars[i + 1]);
            i += 2;
            continue;
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
            let closes = open.last() == Some(&mark) && before.is_some_and(|b| !b.is_whitespace());
            let opens = !closes
                && !intraword
                && after.is_some_and(|a| !a.is_whitespace())
                && has_closer(&chars, i + len, c, len);
            if closes || opens {
                flush(&mut text, &style, &mut out);
                if closes {
                    open.pop();
                } else {
                    open.push(mark);
                }
                let on = !closes;
                match mark {
                    Mark::Bold => style.bold = on || base.bold,
                    Mark::Italic => style.italic = on || base.italic,
                    Mark::Strike => style.strike = on || base.strike,
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

/// The plain-text part: markers gone, links as `text (url)`, quotes and
/// list markers kept as typed.
pub fn to_text(body: &str) -> String {
    body.split('\n')
        .map(|line| {
            inline(line, &Span::default())
                .into_iter()
                .map(|s| match s.link {
                    Some(url) if url != s.text && url.trim_start_matches("mailto:") != s.text => format!("{} ({url})", s.text),
                    _ => s.text,
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The HTML part: paragraphs, lists, quotes and inline styles, with bare
/// web addresses made into links.
pub fn to_html(body: &str) -> String {
    format!("<div style=\"font-family: -apple-system, Helvetica, Arial, sans-serif; font-size: 14px; line-height: 1.5\">{}</div>", blocks(body))
}

enum Block {
    Lines(Vec<String>),
    Bullets(Vec<String>),
    Numbers(Vec<String>),
    Quote(Vec<String>),
}

fn blocks(body: &str) -> String {
    let mut list: Vec<Block> = Vec::new();
    for line in body.split('\n') {
        let quoted = line.strip_prefix("> ").or_else(|| line.strip_prefix('>'));
        let bullet = ["- ", "* ", "• "].iter().find_map(|m| line.strip_prefix(m));
        let number = line.split_once(". ").filter(|(n, _)| !n.is_empty() && n.len() <= 3 && n.chars().all(|c| c.is_ascii_digit())).map(|(_, rest)| rest);
        match (quoted, bullet, number, list.last_mut()) {
            (Some(q), _, _, Some(Block::Quote(lines))) => lines.push(q.to_string()),
            (Some(q), _, _, _) => list.push(Block::Quote(vec![q.to_string()])),
            (_, Some(b), _, Some(Block::Bullets(items))) => items.push(b.to_string()),
            (_, Some(b), _, _) => list.push(Block::Bullets(vec![b.to_string()])),
            (_, _, Some(n), Some(Block::Numbers(items))) => items.push(n.to_string()),
            (_, _, Some(n), _) => list.push(Block::Numbers(vec![n.to_string()])),
            (_, _, _, Some(Block::Lines(lines))) => lines.push(line.to_string()),
            _ => list.push(Block::Lines(vec![line.to_string()])),
        }
    }
    let mut html = String::new();
    for block in list {
        match block {
            Block::Lines(lines) => {
                // One div per line, as Gmail writes them; an empty line keeps its height.
                for line in lines {
                    let h = inline_html(&line);
                    html.push_str(&format!("<div>{}</div>", if h.trim().is_empty() { "<br>".into() } else { h }));
                }
            }
            Block::Bullets(items) => {
                html.push_str("<ul>");
                for item in items {
                    html.push_str(&format!("<li>{}</li>", inline_html(&item)));
                }
                html.push_str("</ul>");
            }
            Block::Numbers(items) => {
                html.push_str("<ol>");
                for item in items {
                    html.push_str(&format!("<li>{}</li>", inline_html(&item)));
                }
                html.push_str("</ol>");
            }
            Block::Quote(lines) => html.push_str(&format!(
                "<blockquote style=\"margin: 0 0 0 0.8ex; border-left: 1px solid #ccc; padding-left: 1ex; color: #555\">{}</blockquote>",
                blocks(&lines.join("\n"))
            )),
        }
    }
    html
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn inline_html(line: &str) -> String {
    let mut out = String::new();
    for s in inline(line, &Span::default()) {
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

    #[test]
    fn spans_round_trip_through_markdown() {
        let original = vec![
            span("Hi Ann,\n\nThe "),
            Span { text: "new price".into(), bold: true, ..Default::default() },
            span(" is "),
            Span { text: "5 * 3 = 15".into(), italic: true, ..Default::default() },
            span(" EUR, see "),
            Span { text: "the offer".into(), link: Some("https://shop.test/a_b(1)".into()), ..Default::default() },
            span(".\n"),
            Span { text: "old line\nsecond".into(), strike: true, bold: true, ..Default::default() },
            span("\nsnake_case and a \\ backslash"),
        ];
        let md = markdown(&original);
        let back = spans(&md);
        assert_eq!(back.iter().map(|s| s.text.as_str()).collect::<String>(), original.iter().map(|s| s.text.as_str()).collect::<String>());
        assert!(back.iter().any(|s| s.bold && s.text == "new price"), "{md}");
        assert!(back.iter().any(|s| s.italic && s.text == "5 * 3 = 15"), "{md}");
        assert!(back.iter().any(|s| s.link.as_deref() == Some("https://shop.test/a_b(1%29") && s.text == "the offer"), "{md}");
        assert!(back.iter().any(|s| s.strike && s.bold && s.text == "second"), "{md}");
    }

    #[test]
    fn spaces_stay_outside_markers() {
        let md = markdown(&[span("a"), Span { text: " bold ".into(), bold: true, ..Default::default() }, span("b")]);
        assert_eq!(md, "a **bold** b");
    }

    #[test]
    fn plain_text_is_not_mistaken_for_formatting() {
        let body = "5 * 3 and 2 * 4, my_var_name, ** alone";
        assert_eq!(spans(body), vec![span(body)]);
        assert_eq!(to_text(body), body);
    }

    #[test]
    fn text_part_drops_markers_and_spells_out_links() {
        assert_eq!(to_text("**Hi** *there*, see [the page](https://x.test)\n> quoted"), "Hi there, see the page (https://x.test)\n> quoted");
        assert_eq!(to_text("[https://x.test](https://x.test)"), "https://x.test");
    }

    #[test]
    fn html_part_has_lists_quotes_and_links() {
        let html = to_html("Hi <you> & **all**\n\n- one\n- two\n1. first\n> On Monday Ann wrote:\n> > deep\nvisit https://x.test/a?b=1.");
        assert!(html.contains("Hi &lt;you&gt; &amp; <b>all</b>"), "{html}");
        assert!(html.contains("<ul><li>one</li><li>two</li></ul><ol><li>first</li></ol>"), "{html}");
        assert!(html.contains("<blockquote") && html.matches("<blockquote").count() == 2, "{html}");
        assert!(html.contains("<a href=\"https://x.test/a?b=1\">https://x.test/a?b=1</a>."), "{html}");
        assert!(html.contains("<div><br></div>"), "blank lines keep their space: {html}");
    }

    #[test]
    fn escaped_text_stays_literal() {
        let raw = "*not italic* and [not](a link)";
        assert_eq!(spans(&escape(raw)), vec![span(raw)]);
        assert_eq!(to_text(&escape(raw)), raw);
    }
}
