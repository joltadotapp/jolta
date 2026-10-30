//! The search language: `from:`, `to:`, `subject:`, `has:attachment`,
//! `is:unread`, `in:`, `label:`, `account:`, `before:`, `after:`, quoted
//! phrases, bare words, and `-` to negate any of them.

use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub negated: bool,
    pub kind: TermKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextField {
    From,
    To,
    Subject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Unread,
    Read,
    Flagged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermKind {
    /// Words to find in the search index. `prefix` is set on the word being
    /// typed, so results update as you type.
    Text { field: Option<TextField>, text: String, phrase: bool, prefix: bool },
    HasAttachment,
    /// Mail sent through a mailing list (it has a List-Id): newsletters.
    HasList,
    Is(Status),
    In(String),
    Label(String),
    /// One mailbox by its store id; how the sidebar lists a mailbox.
    Mailbox(i64),
    Account(String),
    /// Unix seconds at the start of the given UTC day.
    Before(i64),
    After(i64),
}

impl Query {
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

struct Token {
    negated: bool,
    key: Option<String>,
    value: String,
    quoted: bool,
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let negated = first == '-';
        if negated {
            chars.next();
        }
        let mut key = None;
        let mut value = String::new();
        let mut quoted = false;
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            chars.next();
            if c == '"' {
                quoted = true;
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    value.push(c);
                }
            } else if c == ':' && key.is_none() && !quoted && !value.is_empty() {
                key = Some(std::mem::take(&mut value).to_lowercase());
            } else {
                value.push(c);
            }
        }
        if negated && key.is_none() && value.is_empty() && !quoted {
            continue;
        }
        out.push(Token { negated, key, value, quoted });
    }
    out
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_date(s: &str) -> Result<i64> {
    let parts: Vec<&str> = s.split(['-', '/']).collect();
    let bad = || Error::Query(format!("expected a date like 2026-10-30, got {s:?}"));
    let [y, m, d] = parts.as_slice() else { return Err(bad()) };
    let (y, m, d): (i64, i64, i64) =
        (y.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?, d.parse().map_err(|_| bad())?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    Ok(days_from_civil(y, m, d) * 86_400)
}

pub fn parse(input: &str) -> Result<Query> {
    let tokens = tokenize(input);
    let ends_mid_word = !input.ends_with(char::is_whitespace);
    let mut terms = Vec::with_capacity(tokens.len());
    let count = tokens.len();
    for (i, t) in tokens.into_iter().enumerate() {
        let is_last = i + 1 == count;
        let text = |field| TermKind::Text {
            field,
            prefix: is_last && ends_mid_word && !t.quoted,
            phrase: t.quoted,
            text: t.value.clone(),
        };
        let kind = match t.key.as_deref() {
            None => text(None),
            Some("from") => text(Some(TextField::From)),
            Some("to") => text(Some(TextField::To)),
            Some("subject") => text(Some(TextField::Subject)),
            Some("has") => match t.value.to_lowercase().as_str() {
                "attachment" | "attachments" => TermKind::HasAttachment,
                "list" => TermKind::HasList,
                v => return Err(Error::Query(format!("has:{v} is not supported; try has:attachment or has:list"))),
            },
            Some("is") => TermKind::Is(match t.value.to_lowercase().as_str() {
                "unread" => Status::Unread,
                "read" => Status::Read,
                "starred" | "flagged" => Status::Flagged,
                v => return Err(Error::Query(format!("is:{v} is not supported; try is:unread, is:read or is:starred"))),
            }),
            Some("in") => TermKind::In(t.value.clone()),
            Some("mailbox") => TermKind::Mailbox(
                t.value.parse().map_err(|_| Error::Query(format!("mailbox:{} needs a mailbox id", t.value)))?,
            ),
            Some("label") => TermKind::Label(t.value.clone()),
            Some("account") => TermKind::Account(t.value.clone()),
            Some("before") => TermKind::Before(parse_date(&t.value)?),
            Some("after") => TermKind::After(parse_date(&t.value)?),
            Some(key) => TermKind::Text {
                field: None,
                text: format!("{key}:{}", t.value),
                phrase: true,
                prefix: false,
            },
        };
        if let TermKind::Text { text, .. } = &kind {
            if text.trim().is_empty() {
                continue;
            }
        }
        terms.push(Term { negated: t.negated, kind });
    }
    Ok(Query { terms })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(field: Option<TextField>, text: &str, phrase: bool, prefix: bool) -> TermKind {
        TermKind::Text { field, text: text.into(), phrase, prefix }
    }

    fn kinds(q: &str) -> Vec<(bool, TermKind)> {
        parse(q).unwrap().terms.into_iter().map(|t| (t.negated, t.kind)).collect()
    }

    #[test]
    fn bare_words_and_the_word_being_typed() {
        assert_eq!(
            kinds("invoice pai"),
            vec![(false, text(None, "invoice", false, false)), (false, text(None, "pai", false, true))]
        );
        assert_eq!(kinds("invoice "), vec![(false, text(None, "invoice", false, false))]);
    }

    #[test]
    fn fields_and_phrases() {
        assert_eq!(
            kinds(r#"from:alice@example.com subject:"quarterly report" to:bob"#),
            vec![
                (false, text(Some(TextField::From), "alice@example.com", false, false)),
                (false, text(Some(TextField::Subject), "quarterly report", true, false)),
                (false, text(Some(TextField::To), "bob", false, true)),
            ]
        );
        assert_eq!(kinds(r#""lunch on friday""#), vec![(false, text(None, "lunch on friday", true, false))]);
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds("has:attachment is:unread IS:Starred in:inbox label:Work account:2"),
            vec![
                (false, TermKind::HasAttachment),
                (false, TermKind::Is(Status::Unread)),
                (false, TermKind::Is(Status::Flagged)),
                (false, TermKind::In("inbox".into())),
                (false, TermKind::Label("Work".into())),
                (false, TermKind::Account("2".into())),
            ]
        );
        assert_eq!(kinds("mailbox:12"), vec![(false, TermKind::Mailbox(12))]);
        assert_eq!(kinds("-has:list"), vec![(true, TermKind::HasList)]);
        assert!(parse("mailbox:inbox").is_err());
    }

    #[test]
    fn dates() {
        assert_eq!(kinds("after:2026-10-30"), vec![(false, TermKind::After(1_793_318_400))]);
        assert_eq!(kinds("before:1970/01/02"), vec![(false, TermKind::Before(86_400))]);
        assert!(parse("before:yesterday").is_err());
        assert!(parse("after:2026-13-01").is_err());
    }

    #[test]
    fn negation() {
        assert_eq!(
            kinds("-in:spam -from:noreply report"),
            vec![
                (true, TermKind::In("spam".into())),
                (true, text(Some(TextField::From), "noreply", false, false)),
                (false, text(None, "report", false, true)),
            ]
        );
        assert!(parse("- ").unwrap().is_empty());
    }

    #[test]
    fn unknown_keys_and_odd_input_are_text() {
        assert_eq!(kinds("meet 10:30"), vec![
            (false, text(None, "meet", false, false)),
            (false, text(None, "10:30", true, false)),
        ]);
        assert_eq!(kinds(r#"subject:"unterminated"#), vec![(false, text(Some(TextField::Subject), "unterminated", true, false))]);
        assert!(parse("").unwrap().is_empty());
        assert!(parse("from:").unwrap().is_empty());
        assert!(parse("has:pdf").is_err());
    }
}
