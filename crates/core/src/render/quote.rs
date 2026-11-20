//! Folds quoted history that repeats what a conversation already shows.
//!
//! A message's quote is found from the marks mail programs leave in the
//! sanitized body: any `<blockquote>`, Gmail's and Yahoo's quote containers,
//! a Thunderbird cite prefix or "On … wrote:" line before a blockquote, an
//! Outlook header block (From, Sent, To, Subject, in English, Dutch, French
//! or German) or "-----Original Message-----" line with everything after it,
//! and runs of `>` lines in plain text. A message folds only when it has one
//! such quote and nothing but blank space or a Thunderbird signature after
//! it, so answers typed between or below quoted parts keep them shown.
//!
//! The quote folds when every run of four words in it, header lines left
//! out, was in an earlier message of the conversation. Words are compared
//! lowercased, without `>` marks, punctuation-only tokens and zero-width
//! characters, across line breaks, so rewrapped plain text still matches. A
//! word typed into the quote makes runs no earlier message has. Fewer than
//! four words never fold.

use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};

/// Words per compared run.
const RUN: usize = 4;

/// The runs of words the earlier messages of a conversation showed.
#[derive(Default)]
pub(super) struct Conversation {
    seen: HashSet<u64, BuildHasherDefault<Hashed>>,
}

/// The hasher for values that are hashes already.
#[derive(Default)]
struct Hashed(u64);

impl Hasher for Hashed {
    fn write(&mut self, _: &[u8]) {
        unreachable!("only u64 values are hashed")
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

impl Conversation {
    /// `html`, a sanitized body, with its quote in a toggle when the
    /// conversation so far already showed it. The body then counts as shown.
    pub(super) fn fold(&mut self, html: String) -> String {
        let body = Body::read(&html);
        let words = |from: usize, to: usize| -> Vec<u64> {
            body.lines.iter().filter(|l| !l.header && l.start >= from && l.end <= to).flat_map(|l| words(&l.text)).collect()
        };
        let quote = body.quote().filter(|q| {
            let quoted = words(q.header_end, q.end);
            quoted.len() >= RUN && runs(&quoted).all(|r| self.seen.contains(&r))
        });
        self.seen.extend(runs(&words(0, html.len())));
        match quote {
            Some(q) => format!(
                "{}<details class=\"jolta-quoted\"><summary aria-label=\"Show quoted text\">•••</summary>{}</details>{}",
                &html[..q.start],
                &html[q.start..q.end],
                &html[q.end..]
            ),
            None => html,
        }
    }
}

fn words(text: &str) -> impl Iterator<Item = u64> + '_ {
    text.split(char::is_whitespace).filter(|w| w.chars().any(char::is_alphanumeric)).map(|w| {
        let shown = w.chars().filter(|c| !matches!(c, '\u{ad}' | '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}'));
        shown.flat_map(char::to_lowercase).fold(0xcbf2_9ce4_8422_2325u64, |h, c| (h ^ c as u64).wrapping_mul(0x100_0000_01b3))
    })
}

fn runs(words: &[u64]) -> impl Iterator<Item = u64> + '_ {
    words.windows(RUN).map(|w| w.iter().fold(0u64, |h, &w| (h.rotate_left(17) ^ w).wrapping_mul(0x9e37_79b9_7f4a_7c15)))
}

/// An element of a sanitized body, by byte offsets: `start` and `end` around
/// the whole element, `inner` and `close` around its content.
struct Element {
    tag: String,
    class: String,
    start: usize,
    inner: usize,
    close: usize,
    end: usize,
    parent: Option<usize>,
}

/// A line of text as the reader shows it: what `<br>` and block elements
/// separate. `start` is where the break before it ends, `end` where the
/// break after it starts, `next` where that break ends. `text` is
/// lowercased, with single spaces and without the `>` marks that `quoted`
/// tells about; `header` marks lines left out of comparisons.
struct Line {
    text: String,
    quoted: bool,
    header: bool,
    start: usize,
    end: usize,
    next: usize,
}

/// The part of a body to fold. Lines before `header_end` are its header.
struct Quote {
    start: usize,
    end: usize,
    header_end: usize,
}

struct Body<'a> {
    html: &'a str,
    elements: Vec<Element>,
    lines: Vec<Line>,
}

const VOID: &[&str] = &["br", "hr", "img", "wbr", "col", "area"];

const BLOCK: &[&str] = &[
    "address", "article", "aside", "blockquote", "br", "caption", "center", "dd", "details", "div", "dl", "dt", "figcaption", "figure", "footer",
    "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "li", "ol", "p", "pre", "section", "summary", "table", "tbody", "td", "tfoot", "th",
    "thead", "tr", "ul",
];

/// Elements a `<details>` can't sit in without the page's parser moving it.
const NO_DETAILS: &[&str] = &["p", "a", "h1", "h2", "h3", "h4", "h5", "h6", "table", "tbody", "thead", "tfoot", "tr", "ul", "ol", "dl", "colgroup"];

impl<'a> Body<'a> {
    /// Reads ammonia's output, which closes every element and quotes every
    /// attribute.
    fn read(html: &'a str) -> Self {
        let mut elements: Vec<Element> = Vec::new();
        let mut lines = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        let mut text = String::new();
        let mut line_start = 0;
        let mut i = 0;
        let mut brk = |text: &mut String, line_start: &mut usize, at: usize, after: usize| {
            if !text.trim().is_empty() {
                let quoted = text.trim_start().starts_with('>');
                lines.push(Line { text: plain(text), quoted, header: false, start: *line_start, end: at, next: after });
            }
            text.clear();
            *line_start = after;
        };
        while i < html.len() {
            if !html[i..].starts_with('<') {
                let next = html[i..].find('<').map_or(html.len(), |n| i + n);
                decode_into(&mut text, &html[i..next]);
                i = next;
                continue;
            }
            let end = tag_end(html, i);
            let src = &html[i + 1..end - 1];
            if let Some(name) = src.strip_prefix('/') {
                let name = name.trim().to_ascii_lowercase();
                if let Some(pos) = open.iter().rposition(|&e| elements[e].tag == name) {
                    for &e in &open[pos..] {
                        elements[e].close = i;
                        elements[e].end = end;
                    }
                    open.truncate(pos);
                }
                if BLOCK.contains(&name.as_str()) {
                    brk(&mut text, &mut line_start, i, end);
                }
            } else {
                let name: String = src.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
                let class = class(src);
                elements.push(Element { tag: name.clone(), class, start: i, inner: end, close: end, end, parent: open.last().copied() });
                if BLOCK.contains(&name.as_str()) {
                    brk(&mut text, &mut line_start, i, end);
                }
                if !VOID.contains(&name.as_str()) {
                    open.push(elements.len() - 1);
                }
            }
            i = end;
        }
        brk(&mut text, &mut line_start, html.len(), html.len());
        mark_headers(&mut lines);
        for e in open {
            elements[e].close = html.len();
            elements[e].end = html.len();
        }
        Body { html, elements, lines }
    }

    /// The one quote to fold, if the body has exactly one and only blank
    /// space or a Thunderbird signature follows it.
    fn quote(&self) -> Option<Quote> {
        let mut found = self.quoted_elements();
        found.extend(self.header_blocks());
        found.extend(self.quoted_lines());
        found.sort_by_key(|q| (q.start, std::cmp::Reverse(q.end)));
        let mut outer = found.into_iter();
        let first = outer.next()?;
        let mut rest = outer.filter(|q| q.start >= first.end);
        if rest.next().is_some() {
            return None;
        }
        let mut after = first.end;
        for e in self.elements.iter().filter(|e| e.class.split_whitespace().any(|c| c == "moz-signature")) {
            if e.start >= after && !self.has_content(after, e.start) {
                after = e.end;
            }
        }
        (!self.has_content(after, self.html.len())).then_some(first)
    }

    /// Blockquotes and Gmail's and Yahoo's containers, with the attribution
    /// line or blank space right before them.
    fn quoted_elements(&self) -> Vec<Quote> {
        let classes = ["gmail_quote", "gmail_quote_container", "yahoo_quoted"];
        self.elements
            .iter()
            .enumerate()
            .filter(|(_, e)| e.tag == "blockquote" || e.class.split_whitespace().any(|c| classes.contains(&c)))
            .filter(|(_, e)| self.holds_details(e.parent))
            .map(|(i, e)| {
                let header_end = match e.tag.as_str() {
                    "blockquote" => e.inner,
                    _ => self.elements[i + 1..].iter().take_while(|d| d.start < e.close).find(|d| d.tag == "blockquote").map_or(e.inner, |d| d.start),
                };
                Quote { start: self.extend_back(i, true), end: e.end, header_end }
            })
            .collect()
    }

    /// From an Outlook header block or an "-----Original Message-----" line
    /// to the end of the element that holds it.
    fn header_blocks(&self) -> Vec<Quote> {
        let mut out = Vec::new();
        for (n, line) in self.lines.iter().enumerate() {
            let t = &line.text;
            let after_rule = n > 0 && rule(&self.lines[n - 1].text);
            let starts_block = line.header && field(t).is_some() && (n == 0 || !self.lines[n - 1].header || after_rule);
            if !((separator(t) && !rule(t)) || starts_block) {
                continue;
            }
            let line = if after_rule { &self.lines[n - 1] } else { line };
            let holder = self.holder(line.start);
            let inner = holder.map_or(0, |h| self.elements[h].inner);
            let (start, parent) = if self.has_content(inner, line.start) {
                (line.start, holder)
            } else if let Some(mut s) = holder {
                while let Some(p) = self.elements[s].parent {
                    if self.has_content(self.elements[p].inner, self.elements[s].start) {
                        break;
                    }
                    s = p;
                }
                (self.extend_back(s, false), self.elements[s].parent)
            } else {
                (0, None)
            };
            if self.holds_details(parent) {
                let end = parent.map_or(self.html.len(), |p| self.elements[p].close);
                out.push(Quote { start, end, header_end: start });
            }
        }
        out
    }

    /// Runs of `>` lines in a plain-text body, with the attribution line
    /// before them.
    fn quoted_lines(&self) -> Vec<Quote> {
        if !self.html.starts_with("<div class=\"jolta-plain\">") {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut n = 0;
        while n < self.lines.len() {
            if !self.lines[n].quoted {
                n += 1;
                continue;
            }
            let first = n;
            while n < self.lines.len() && self.lines[n].quoted {
                n += 1;
            }
            let mut from = first;
            if from > 0 && attribution(&self.lines[from - 1].text) {
                from -= 1;
                let opening = |t: &str| ["on ", "op ", "le ", "am "].iter().any(|o| t.starts_with(o));
                if from > 0 && !opening(&self.lines[from].text) && opening(&self.lines[from - 1].text) {
                    from -= 1;
                }
            }
            out.push(Quote { start: self.lines[from].start, end: self.lines[n - 1].next, header_end: self.lines[first].start });
        }
        out
    }

    /// Where a quote that starts at element `i` begins: before the blank
    /// elements right in front of it and, with `attributed`, one element
    /// that only says who wrote the quote.
    fn extend_back(&self, i: usize, attributed: bool) -> usize {
        let mut start = self.elements[i].start;
        let parent = self.elements[i].parent;
        let siblings = self.elements[..i].iter().rev().filter(|e| e.parent == parent);
        for e in siblings {
            if e.end > start || self.has_content(e.end, start) {
                break;
            }
            if !self.has_content(e.start, e.end) {
                start = e.start;
                continue;
            }
            let lines: Vec<&Line> = self.lines.iter().filter(|l| l.start >= e.start && l.end <= e.end).collect();
            let cites = e.class.split_whitespace().any(|c| c == "moz-cite-prefix")
                || (lines.len() == 1 && attribution(&lines[0].text));
            if attributed && cites {
                start = e.start;
            }
            break;
        }
        start
    }

    /// The innermost element whose content holds byte `at`.
    fn holder(&self, at: usize) -> Option<usize> {
        self.elements.iter().rposition(|e| e.inner <= at && at <= e.close && !VOID.contains(&e.tag.as_str()))
    }

    fn holds_details(&self, parent: Option<usize>) -> bool {
        parent.is_none_or(|p| !NO_DETAILS.contains(&self.elements[p].tag.as_str()))
    }

    /// Whether the HTML between two offsets shows text or an image.
    fn has_content(&self, from: usize, to: usize) -> bool {
        if from >= to {
            return false;
        }
        let mut text = String::new();
        let mut rest = &self.html[from..to];
        while !rest.is_empty() {
            match rest.find('<') {
                Some(0) => {
                    if rest[1..].starts_with("img") {
                        return true;
                    }
                    rest = &rest[tag_end(rest, 0)..];
                }
                Some(n) => {
                    decode_into(&mut text, &rest[..n]);
                    rest = &rest[n..];
                }
                None => {
                    decode_into(&mut text, rest);
                    rest = "";
                }
            }
        }
        !text.trim().is_empty()
    }
}

/// The end of the tag that starts at `at`, past quoted attribute values.
fn tag_end(html: &str, at: usize) -> usize {
    let mut quote = None;
    for (i, c) in html[at..].char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if q == c => quote = None,
            (None, '>') => return at + i + 1,
            _ => {}
        }
    }
    html.len()
}

fn class(tag: &str) -> String {
    let Some(at) = tag.find(" class=\"").map(|i| i + 8) else { return String::new() };
    tag[at..].split('"').next().unwrap_or_default().to_string()
}

/// Appends HTML text with its character references decoded.
fn decode_into(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest[1..].find(';').filter(|&n| n <= 10).and_then(|n| {
            let name = &rest[1..n + 1];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok().and_then(char::from_u32),
                    None => name.strip_prefix('#').and_then(|d| d.parse().ok()).and_then(char::from_u32),
                },
            };
            c.map(|c| (c, n + 2))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

/// A line lowercased, with single spaces and without its `>` marks.
fn plain(text: &str) -> String {
    let text = text.trim_start_matches(|c: char| c == '>' || c.is_whitespace());
    text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// "-----Original Message-----", "________", "Begin forwarded message:" and
/// their Dutch, French and German forms.
fn separator(t: &str) -> bool {
    let ruled = t.len() >= 6 && t.starts_with("--") && t.ends_with("--");
    let forwarded = ["begin forwarded message", "begin doorgestuurd bericht", "début du message réexpédié", "anfang der weitergeleiteten nachricht"];
    ruled || rule(t) || forwarded.iter().any(|f| t.starts_with(f))
}

/// A line of only dashes or underscores, as Outlook writes above a header
/// block in plain text and many signatures hold.
fn rule(t: &str) -> bool {
    t.chars().count() >= 6 && t.chars().all(|c| c == '_' || c == '-')
}

/// "On … wrote:" and the forms Gmail, Apple Mail and Outlook write in Dutch,
/// French and German.
fn attribution(t: &str) -> bool {
    t.len() <= 400
        && (["wrote:", "writes:", "geschreven:", "het volgende:", "a écrit :", "a écrit:", "escribió:"].iter().any(|e| t.ends_with(e))
            || (t.ends_with(':') && (t.contains(" schreef ") || t.contains(" schrieb "))))
}

/// The label of a header field line, and whether it names the sender.
fn field(t: &str) -> Option<bool> {
    const FROM: &[&str] = &["from", "van", "de", "von"];
    const OTHER: &[&str] = &[
        "sent", "date", "to", "cc", "bcc", "subject", "reply-to", "verzonden", "datum", "aan", "onderwerp", "envoyé", "à", "objet", "gesendet", "an",
        "betreff",
    ];
    let label = t[..t.find(':')?].trim();
    if FROM.contains(&label) {
        Some(true)
    } else {
        OTHER.contains(&label).then_some(false)
    }
}

/// Marks the header lines: separators, attributions, and runs of three or
/// more header fields that name a sender. They are left out of the
/// comparison on both sides.
fn mark_headers(lines: &mut [Line]) {
    for l in lines.iter_mut() {
        l.header = separator(&l.text) || attribution(&l.text);
    }
    let mut n = 0;
    while n < lines.len() {
        let run = lines[n..].iter().take_while(|l| field(&l.text).is_some()).count();
        if run >= 3 && lines[n..n + run].iter().any(|l| field(&l.text) == Some(true)) {
            lines[n..n + run].iter_mut().for_each(|l| l.header = true);
        }
        n += run.max(1);
    }
}

#[cfg(test)]
mod tests {
    use crate::render::{MessageView, render_thread};
    use crate::smime::Protection;

    const ANN: &str = "<div>Hi Bob,</div><div><br></div><div>Can you send me the Q3 numbers before Thursday? The board meets on Friday morning.</div>\
        <div><br></div><div>Thanks,</div><div>Ann</div>";

    const ANN_TEXT: &str = "Hi Bob,\n\nCan you send me the Q3 numbers before Thursday? The board meets on Friday\nmorning.\n\nThanks,\nAnn";

    fn html(body: &str) -> Vec<u8> {
        format!("From: a@example.com\r\nSubject: Q3 numbers\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{body}\r\n").into_bytes()
    }

    fn text(body: &str) -> Vec<u8> {
        format!("From: a@example.com\r\nSubject: Q3 numbers\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n", body.replace('\n', "\r\n")).into_bytes()
    }

    /// Each message's body as rendered: what shows, and what a quote toggle hides.
    fn rendered(raws: &[Vec<u8>]) -> Vec<(String, String)> {
        let views: Vec<MessageView> = raws
            .iter()
            .enumerate()
            .map(|(i, raw)| MessageView {
                id: i as i64 + 1,
                from: "a",
                to: "b",
                date: "d",
                snippet: "s",
                raw,
                open: true,
                vip: false,
                protection: &Protection::None,
                cards: &[],
            })
            .collect();
        let html = render_thread(&views, "").html;
        (1..=raws.len())
            .map(|id| {
                let start = html.find(&format!("id=\"m{id}\"")).unwrap();
                let end = html[start..].find("<details class=\"jolta-message\"").map_or(html.len(), |i| start + i);
                let body = &html[start..end];
                match body.find("<details class=\"jolta-quoted\">") {
                    Some(fold) => {
                        let close = fold + body[fold..].find("</details>").unwrap() + "</details>".len();
                        (format!("{}{}", &body[..fold], &body[close..]), body[fold..close].to_string())
                    }
                    None => (body.to_string(), String::new()),
                }
            })
            .collect()
    }

    fn assert_folds(raws: &[Vec<u8>], shown: &[&str], hidden: &[&str]) {
        let last = rendered(raws).pop().unwrap();
        assert!(!last.1.is_empty(), "the quote folds:\n{}", last.0);
        for s in shown {
            assert!(last.0.contains(s), "{s:?} stays shown:\n{}\n---\n{}", last.0, last.1);
        }
        for h in hidden {
            assert!(last.1.contains(h) && !last.0.contains(h), "{h:?} folds away:\n{}\n---\n{}", last.0, last.1);
        }
        assert!(
            last.1.starts_with("<details class=\"jolta-quoted\"><summary aria-label=\"Show quoted text\">"),
            "the toggle has a label: {}",
            last.1
        );
    }

    fn assert_shown(raws: &[Vec<u8>]) {
        for (i, (_, hidden)) in rendered(raws).iter().enumerate() {
            assert!(hidden.is_empty(), "message {} folds nothing: {hidden}", i + 1);
        }
    }

    #[test]
    fn a_gmail_quote_of_an_earlier_message_folds_with_its_attribution() {
        let reply = html(&format!(
            "<div dir=\"ltr\">Here they are, see the second tab.<div><br></div><div>Bob</div></div><br>\
             <div class=\"gmail_quote gmail_quote_container\"><div dir=\"ltr\" class=\"gmail_attr\">On Mon, 3 Nov 2026 at 09:12, Ann Example \
             &lt;<a href=\"mailto:ann@example.com\">ann@example.com</a>&gt; wrote:<br></div>\
             <blockquote class=\"gmail_quote\" style=\"margin:0px 0px 0px 0.8ex;border-left:1px solid rgb(204,204,204);padding-left:1ex\">{ANN}</blockquote></div>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn an_older_dutch_gmail_quote_folds() {
        let reply = html(&format!(
            "<div>Hier zijn ze.</div><div>Bob</div><br><div class=\"gmail_quote\">Op 3 november 2026 09:12 schreef Ann Example \
             <span dir=\"ltr\">&lt;<a href=\"mailto:ann@example.com\">ann@example.com</a>&gt;</span> het volgende:<br>\
             <blockquote class=\"gmail_quote\" style=\"margin:0 0 0 .8ex;border-left:1px #ccc solid;padding-left:1ex;\">{ANN}</blockquote></div>"
        ));
        assert_folds(&[html(ANN), reply], &["Hier zijn ze"], &["schreef Ann Example", "board meets"]);
    }

    #[test]
    fn an_apple_mail_quote_folds_with_its_attribution() {
        let mac = html(&format!(
            "<div>Here they are, see the second tab.</div><div>Bob</div><div><br><div><div>On 3 Nov 2026, at 09:12, Ann Example \
             &lt;ann@example.com&gt; wrote:</div><br class=\"Apple-interchange-newline\"><blockquote type=\"cite\">{ANN}</blockquote></div></div>"
        ));
        assert_folds(&[html(ANN), mac], &["Here they are"], &["Ann Example", "board meets"]);

        let phone = html(&format!(
            "<div dir=\"ltr\">Here they are, see the second tab.</div><div dir=\"ltr\">Bob</div><div dir=\"ltr\"><br>\
             <blockquote type=\"cite\">On 3 Nov 2026, at 09:12, Ann Example &lt;ann@example.com&gt; wrote:<br><br></blockquote></div>\
             <blockquote type=\"cite\"><div dir=\"ltr\">{ANN}</div></blockquote>"
        ));
        assert_folds(&[html(ANN), phone], &["Here they are"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn a_thunderbird_quote_folds_with_its_cite_prefix() {
        let reply = html(&format!(
            "<p>Here they are, see the second tab.</p><p>Bob</p><div class=\"moz-cite-prefix\">On 03/11/2026 09:12, Ann Example wrote:<br></div>\
             <blockquote type=\"cite\" cite=\"mid:q3@example.com\">{ANN}</blockquote>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn an_outlook_web_quote_folds_with_its_header() {
        let reply = html(&format!(
            "<div style=\"font-family:Aptos\">Here they are, see the second tab.</div><div>Bob</div><div id=\"appendonsend\"></div>\
             <hr style=\"display:inline-block;width:98%\" tabindex=\"-1\"><div id=\"divRplyFwdMsg\" dir=\"ltr\">\
             <font face=\"Calibri, sans-serif\" style=\"font-size:11pt\" color=\"#000000\"><b>From:</b> Ann Example &lt;ann@example.com&gt;<br>\
             <b>Sent:</b> Monday, November 3, 2026 9:12 AM<br><b>To:</b> Bob Example &lt;bob@example.com&gt;<br><b>Subject:</b> Q3 numbers</font>\
             <div>&nbsp;</div></div><div>{ANN}</div>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Ann Example", "Q3 numbers</font>", "board meets", "<hr"]);
    }

    fn outlook_desktop(labels: [&str; 4]) -> Vec<u8> {
        let [from, sent, to, subject] = labels;
        let quoted: String = ANN
            .split("</div>")
            .filter(|l| !l.is_empty())
            .map(|l| format!("<p class=\"MsoNormal\">{}</p>", l.trim_start_matches("<div>").replace("<br>", "&nbsp;")))
            .collect();
        html(&format!(
            "<div class=\"WordSection1\"><p class=\"MsoNormal\">Here they are, see the second tab.</p><p class=\"MsoNormal\">Bob</p>\
             <p class=\"MsoNormal\">&nbsp;</p><div><div style=\"border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm\">\
             <p class=\"MsoNormal\"><b>{from}</b> Ann Example &lt;ann@example.com&gt; <br><b>{sent}</b> 3 November 2026 09:12<br>\
             <b>{to}</b> Bob Example &lt;bob@example.com&gt;<br><b>{subject}</b> Q3 numbers</p></div></div><p class=\"MsoNormal\">&nbsp;</p>\
             {quoted}</div>"
        ))
    }

    #[test]
    fn an_outlook_quote_folds_with_its_dutch_header() {
        let reply = outlook_desktop(["Van:", "Verzonden:", "Aan:", "Onderwerp:"]);
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Verzonden:", "Ann Example", "board meets"]);
    }

    #[test]
    fn an_outlook_quote_folds_with_its_header_in_english_french_or_german() {
        for labels in [["From:", "Sent:", "To:", "Subject:"], ["De&nbsp;:", "Envoyé&nbsp;:", "À&nbsp;:", "Objet&nbsp;:"], ["Von:", "Gesendet:", "An:", "Betreff:"]] {
            assert_folds(&[html(ANN), outlook_desktop(labels)], &["Here they are"], &[labels[1], "Ann Example", "board meets"]);
        }
    }

    #[test]
    fn an_original_message_line_folds_with_what_follows() {
        let reply = text(&format!(
            "Here they are, see the second tab.\n\nBob\n\n-----Original Message-----\nFrom: Ann Example <ann@example.com>\n\
             Sent: Monday, November 3, 2026 9:12 AM\nTo: Bob Example <bob@example.com>\nSubject: Q3 numbers\n\n{ANN_TEXT}"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Original Message", "board meets"]);

        let dutch = text(&format!(
            "Hier zijn ze.\n\n-----Oorspronkelijk bericht-----\nVan: Ann Example <ann@example.com>\nVerzonden: maandag 3 november 2026 09:12\n\
             Aan: Bob Example <bob@example.com>\nOnderwerp: Q3 numbers\n\n{ANN_TEXT}"
        ));
        assert_folds(&[html(ANN), dutch], &["Hier zijn ze"], &["Oorspronkelijk bericht", "board meets"]);
    }

    #[test]
    fn a_yahoo_quote_folds() {
        let reply = html(&format!(
            "<div class=\"ydp1\"><div>Here they are, see the second tab.</div><div>Bob</div></div>\
             <div id=\"yahoo_quoted_4242\" class=\"yahoo_quoted\"><div style=\"font-family:Helvetica\">\
             <div>On Monday, November 3, 2026 at 09:12:00 CET, Ann Example &lt;ann@example.com&gt; wrote:</div><div><br></div><div><br></div>\
             <div><div dir=\"ltr\">{ANN}</div></div></div></div>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn a_plain_text_quote_folds_with_its_attribution() {
        let quoted: String = ANN_TEXT.lines().map(|l| if l.is_empty() { ">\n".to_string() } else { format!("> {l}\n") }).collect();
        let reply = text(&format!("Here they are, see the second tab.\n\nBob\n\nOn Mon, 3 Nov 2026 at 09:12, Ann Example <ann@example.com> wrote:\n\n{quoted}"));
        assert_folds(&[html(ANN), reply], &["Here they are", "Bob"], &["Ann Example", "board meets"]);
    }

    #[test]
    fn a_quote_of_a_quote_folds_when_both_were_shown_before() {
        let bob = format!(
            "<div dir=\"ltr\">Here they are, see the second tab.<div>Bob</div></div><br><div class=\"gmail_quote\">\
             <div class=\"gmail_attr\">On Mon, 3 Nov 2026 at 09:12, Ann Example wrote:<br></div><blockquote class=\"gmail_quote\">{ANN}</blockquote></div>"
        );
        let ann = format!(
            "<div dir=\"ltr\">Got them, thank you.</div><br><div class=\"gmail_quote\">\
             <div class=\"gmail_attr\">On Mon, 3 Nov 2026 at 10:30, Bob Example wrote:<br></div><blockquote class=\"gmail_quote\">{bob}</blockquote></div>"
        );
        let raws = [html(ANN), html(&bob), html(&ann)];
        assert_folds(&raws, &["Got them"], &["Here they are", "board meets"]);
        assert!(!rendered(&raws)[0].0.contains("jolta-quoted"));
    }

    #[test]
    fn answers_typed_into_the_quote_stay_shown() {
        let between = html(
            "<div>Answers below.</div><div><br><blockquote type=\"cite\"><div>On 3 Nov 2026, at 09:12, Ann Example wrote:</div>\
             <br class=\"Apple-interchange-newline\"><div>Hi Bob,</div><div><br></div>\
             <div>Can you send me the Q3 numbers before Thursday? The board meets on Friday morning.</div></blockquote></div>\
             <div>Yes, attached. Friday works.</div><blockquote type=\"cite\"><div>Thanks,</div><div>Ann</div></blockquote>",
        );
        assert_shown(&[html(ANN), between]);

        let inside = html(
            "<div>See my answer in red.</div><div class=\"gmail_quote\"><div class=\"gmail_attr\">On Mon, 3 Nov 2026, Ann Example wrote:</div>\
             <blockquote class=\"gmail_quote\"><div>Hi Bob,</div><div><br></div>\
             <div>Can you send me the Q3 numbers before Thursday? The board meets on Friday morning.</div>\
             <div style=\"color:red\">Yes, they are attached.</div><div><br></div><div>Thanks,</div><div>Ann</div></blockquote></div>",
        );
        assert_shown(&[html(ANN), inside]);

        let plain = text(
            "On Mon, 3 Nov 2026 at 09:12, Ann Example <ann@example.com> wrote:\n> Hi Bob,\n>\n\
             > Can you send me the Q3 numbers before Thursday? The board meets on Friday\n> morning.\n\nYes, attached.\n\n> Thanks,\n> Ann\n",
        );
        assert_shown(&[html(ANN), plain]);
    }

    #[test]
    fn text_written_in_a_gmail_or_yahoo_container_before_its_quote_stays_shown() {
        let gmail = html(&format!(
            "<div>Here they are.</div><div class=\"gmail_quote\"><div class=\"gmail_attr\">On Mon, 3 Nov 2026, Ann Example wrote:</div>\
             <div>Note that the board moved to Monday morning.</div><blockquote class=\"gmail_quote\">{ANN}</blockquote></div>"
        ));
        assert_shown(&[html(ANN), gmail]);

        let yahoo = html(&format!(
            "<div>Here they are.</div><div class=\"yahoo_quoted\"><div>Note that the board moved to Monday morning.</div>\
             <div>On Monday, November 3, 2026 at 09:12:00 CET, Ann Example wrote:</div><div><div dir=\"ltr\">{ANN}</div></div></div>"
        ));
        assert_shown(&[html(ANN), yahoo]);
    }

    #[test]
    fn an_emoji_or_check_mark_typed_into_the_quote_stays_shown() {
        for mark in ["👍", "✓", "✗"] {
            let reply = html(&format!(
                "<div>See below.</div><div><br><blockquote type=\"cite\"><div>Hi Bob,</div><div><br></div>\
                 <div>Can you send me the Q3 numbers before Thursday?</div><div>{mark}</div><div>The board meets on Friday morning.</div>\
                 <div><br></div><div>Thanks,</div><div>Ann</div></blockquote></div>"
            ));
            assert_shown(&[html(ANN), reply]);
        }
    }

    #[test]
    fn punctuation_typed_into_the_quote_still_folds() {
        let reply = html(&format!(
            "<div>Here they are.</div><div><br><blockquote type=\"cite\"><div>Hi Bob,</div><div>--</div><div>...</div>\
             <div>Can you send me the Q3 numbers before Thursday? The board meets on Friday morning.</div>\
             <div><br></div><div>Thanks,</div><div>Ann</div></blockquote></div>"
        ));
        assert_folds(&[html(ANN), reply], &["Here they are"], &["board meets"]);
    }

    #[test]
    fn a_forward_of_mail_outside_the_conversation_stays_shown() {
        let forward = html(
            "<div>Bob, see what Carol wrote about the numbers.</div><div id=\"appendonsend\"></div><hr style=\"display:inline-block;width:98%\">\
             <div id=\"divRplyFwdMsg\" dir=\"ltr\"><font face=\"Calibri, sans-serif\"><b>From:</b> Carol Example &lt;carol@example.org&gt;<br>\
             <b>Sent:</b> Friday, October 31, 2026 4:02 PM<br><b>To:</b> Ann Example &lt;ann@example.com&gt;<br><b>Subject:</b> Audit</font></div>\
             <div>Ann, the auditors need the Q3 numbers by Thursday as well. Please send them both.</div>",
        );
        assert_shown(&[html(ANN), forward]);
    }

    #[test]
    fn the_first_message_never_folds() {
        let reply = html(&format!(
            "<div>Here they are.</div><div class=\"gmail_quote\"><div class=\"gmail_attr\">On Mon, 3 Nov 2026, Ann Example wrote:</div>\
             <blockquote class=\"gmail_quote\">{ANN}</blockquote></div>"
        ));
        assert_shown(std::slice::from_ref(&reply));
        assert_shown(&[reply, html("<div>Thanks!</div>")]);
    }
}
