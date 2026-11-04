//! The two AI features: a summary at the top of a conversation, and a
//! reply or new message written from a short instruction.
//!
//! Summaries are made only when a conversation is opened, never for old
//! mail in bulk, and kept per conversation with the newest message they
//! cover. When a reply arrives, the next summary starts from the old one and
//! reads only the new messages.

use mail_parser::MessageParser;
use rusqlite::{Connection, OptionalExtension};

use super::{Ai, Cancel, Finish, Progress, Request, Settings};
use crate::compose::{self, Compose};
use crate::model::*;
use crate::reader::{self, ThreadMessage};
use crate::{Jolta, Result, imap, rich, store};

/// Shorter conversations read faster than their summary: one or two
/// messages under this many characters get none unless asked for.
const WORTH_SUMMARISING: usize = 2_500;
const MESSAGE_CAP: usize = 6_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub text: String,
    /// The model that wrote it.
    pub model: String,
    /// The newest message it covers.
    pub last_message: MessageId,
}

pub fn cached_summary(conn: &Connection, thread: ThreadId) -> Result<Option<Summary>> {
    Ok(conn
        .query_row("SELECT text, model, last_message FROM ai_summary WHERE thread_id = ?1", [thread], |r| {
            Ok(Summary { text: r.get(0)?, model: r.get(1)?, last_message: r.get(2)? })
        })
        .optional()?)
}

fn save_summary(conn: &Connection, thread: ThreadId, s: &Summary) -> Result<()> {
    conn.execute(
        "INSERT INTO ai_summary (thread_id, last_message, model, text, at) VALUES (?1, ?2, ?3, ?4, unixepoch())
         ON CONFLICT (thread_id) DO UPDATE SET last_message = excluded.last_message, model = excluded.model,
             text = excluded.text, at = excluded.at",
        rusqlite::params![thread, s.last_message, s.model, s.text],
    )?;
    Ok(())
}

/// The newest message of a conversation, to tell whether a summary is current.
pub fn newest_message(conn: &Connection, thread: ThreadId) -> Result<Option<MessageId>> {
    Ok(reader::thread_messages(conn, thread)?.1.last().map(|m| m.id))
}

/// A message's own words: quoted replies, reply headers and the signature
/// cut, blank runs squeezed.
pub fn message_text(raw: &[u8]) -> String {
    let raw = crate::smime::readable(raw);
    let Some(msg) = MessageParser::new().parse(&raw[..]) else { return String::new() };
    let body = msg.body_text(0).map(|t| t.into_owned()).unwrap_or_default();
    let mut out = String::new();
    let mut blank = 0;
    let lines: Vec<&str> = body.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        let next = lines.get(i + 1).map_or("", |l| l.trim());
        let quote_header = (t.starts_with("On ") && (t.ends_with("wrote:") || next.ends_with("wrote:")))
            || (t.starts_with("Op ") && [t, next].iter().any(|l| l.contains("schreef") && l.ends_with(':')))
            || (t.starts_with("Le ") && [t, next].iter().any(|l| l.contains("a écrit") && l.ends_with(':')))
            || t.starts_with("-----Original Message-----")
            || t.starts_with("________________________________")
            || (t.starts_with("From:") && next.starts_with("Sent:"))
            || (t.starts_with("Van:") && next.starts_with("Verzonden:"));
        if quote_header || *line == "-- " {
            break;
        }
        if t.starts_with('>') {
            continue;
        }
        if t.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    let mut text = out.trim().to_string();
    if text.len() > MESSAGE_CAP {
        let mut end = MESSAGE_CAP;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" […]");
    }
    text
}

/// The language a text is most likely in, from its common words. Small
/// models follow "write in Dutch" far better than "match the language".
pub fn guess_language(text: &str) -> Option<&'static str> {
    const WORDS: [(&str, &[&str]); 5] = [
        ("English", &["the", "and", "is", "to", "of", "you", "we", "it", "for", "that", "this", "with", "have", "are", "will", "i", "can", "be", "on", "thanks"]),
        ("Dutch", &["de", "het", "een", "en", "van", "ik", "je", "we", "dat", "niet", "op", "met", "voor", "zijn", "wij", "jullie", "ook", "maar", "bedankt", "graag"]),
        ("French", &["le", "la", "les", "et", "est", "un", "une", "je", "vous", "nous", "pour", "que", "pas", "sur", "avec", "dans", "ce", "il", "merci", "bonjour"]),
        ("German", &["der", "die", "das", "und", "ist", "ich", "sie", "wir", "nicht", "mit", "für", "auf", "ein", "eine", "zu", "danke", "bitte", "auch", "sind", "haben"]),
        ("Spanish", &["el", "los", "las", "y", "es", "que", "en", "un", "una", "por", "para", "con", "no", "gracias", "hola", "pero", "como", "muy", "está", "lo"]),
    ];
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphabetic()).filter(|w| !w.is_empty()).collect();
    let mut scores: Vec<(usize, &str)> =
        WORDS.iter().map(|(lang, list)| (words.iter().filter(|w| list.contains(w)).count(), *lang)).collect();
    scores.sort_by(|a, b| b.0.cmp(&a.0));
    (scores[0].0 >= 2 && scores[0].0 > scores[1].0).then_some(scores[0].1)
}

struct Message {
    meta: ThreadMessage,
    text: String,
}

async fn load(jolta: &Jolta, thread: ThreadId) -> Result<(String, Vec<Message>)> {
    let (subject, metas) = jolta.store.read_async(move |c| reader::thread_messages(c, thread)).await?;
    let mut out = Vec::with_capacity(metas.len());
    for meta in metas {
        let raw = imap::message_source(jolta, meta.id).await.unwrap_or_default();
        out.push(Message { text: message_text(&raw), meta });
    }
    Ok((subject, out))
}

/// The messages as plain text, oldest first, as many of the newest as fit
/// in `budget` characters.
fn transcript(messages: &[Message], budget: usize) -> String {
    let mut blocks = Vec::new();
    let mut used = 0;
    for m in messages.iter().rev() {
        let from = match m.meta.from_name.is_empty() {
            true => m.meta.from_addr.clone(),
            false => format!("{} <{}>", m.meta.from_name, m.meta.from_addr),
        };
        let mut block = format!("From: {from}\n");
        if !m.meta.to.is_empty() {
            block.push_str(&format!("To: {}\n", m.meta.to));
        }
        if !m.meta.cc.is_empty() {
            block.push_str(&format!("Cc: {}\n", m.meta.cc));
        }
        block.push_str(&format!("Date: {}\n\n{}\n", reader::format_date(m.meta.date, 0), m.text));
        if used + block.len() > budget && !blocks.is_empty() {
            let left = messages.len() - blocks.len();
            blocks.push(format!("[{left} earlier message{} left out]\n", if left == 1 { "" } else { "s" }));
            break;
        }
        used += block.len();
        blocks.push(block);
    }
    blocks.reverse();
    blocks.join("\n---\n\n")
}

/// The language summaries are written in, whatever the mail's language:
/// the app's. Jolta's interface is English only for now.
pub const APP_LANGUAGE: &str = "English";

fn summary_system(language: &str) -> String {
    format!(
        "You summarise email conversations for a busy reader. \
Write 2 to 5 bullet points in {language}, each on its own line starting with \"- \". \
Always write in {language}, also when the emails are in another language; keep names, places and quoted terms as they are. \
Cover what was decided, the current dates, times, amounts and places, and what is still open and who has to act. \
When something changed during the conversation, give only the latest value. \
Each bullet is one short sentence. Never write a bullet about what the email doesn't say. \
No title, no introduction, no closing remark, no bold."
    )
}

fn update_system(language: &str) -> String {
    format!(
        "You keep a summary of an email conversation up to date for a busy reader. \
You get the summary so far and the messages that arrived since. Write the summary of the whole conversation now: \
2 to 5 bullet points in {language}, each on its own line starting with \"- \". \
Always write in {language}, also when the emails are in another language; keep names, places and quoted terms as they are. \
Cover what was decided, the current dates, times, amounts and places, and what is still open and who has to act. \
When a new message changes something, give only the latest value. \
Each bullet is one short sentence. Never write a bullet about what the email doesn't say. \
No title, no introduction, no closing remark, no bold."
    )
}

/// How a summary request ended.
#[derive(Debug, PartialEq)]
pub enum SummaryOutcome {
    Done(Summary),
    /// The conversation is short enough to read; nothing was made.
    NotNeeded,
    NotSetUp,
    Cancelled,
}

/// Summarises a conversation, or brings its summary up to date. With
/// `automatic`, short conversations are skipped and so is everything when
/// the user turned automatic summaries off. `fresh` writes a new summary
/// even when the saved one is current.
pub async fn summarise(
    jolta: &Jolta,
    ai: &Ai,
    thread: ThreadId,
    automatic: bool,
    fresh: bool,
    cancel: &Cancel,
    on: &mut (dyn FnMut(Progress) + Send),
) -> Result<SummaryOutcome> {
    let (settings, cached) = jolta.store.read_async(move |c| Ok((super::settings(c)?, cached_summary(c, thread)?))).await?;
    let cached = cached.filter(|_| !fresh);
    if settings.provider.is_none() {
        return Ok(SummaryOutcome::NotSetUp);
    }
    if automatic && !settings.auto_summary {
        return Ok(SummaryOutcome::NotNeeded);
    }
    let (subject, messages) = load(jolta, thread).await?;
    let Some(newest) = messages.last().map(|m| m.meta.id) else { return Ok(SummaryOutcome::NotNeeded) };
    if let Some(c) = cached.as_ref().filter(|c| c.last_message == newest) {
        return Ok(SummaryOutcome::Done(c.clone()));
    }
    let length: usize = messages.iter().map(|m| m.text.len()).sum();
    if automatic && cached.is_none() && messages.len() < 3 && length < WORTH_SUMMARISING {
        return Ok(SummaryOutcome::NotNeeded);
    }
    let budget = Ai::budget(&settings);
    let earlier = cached.as_ref().and_then(|c| messages.iter().position(|m| m.meta.id == c.last_message).map(|i| (c, i)));
    let request = match earlier {
        Some((c, i)) => Request {
            system: update_system(APP_LANGUAGE),
            user: format!(
                "Conversation: {subject}\n\nSummary so far:\n{}\n\nNew messages:\n\n{}",
                c.text,
                transcript(&messages[i + 1..], budget)
            ) + &format!("\n\nWrite the summary in {APP_LANGUAGE}."),
            max_tokens: 500,
            temperature: 0.0,
            grammar: None,
        },
        None => Request {
            system: summary_system(APP_LANGUAGE),
            user: format!("Conversation: {subject}\n\n{}\n\nWrite the summary in {APP_LANGUAGE}.", transcript(&messages, budget)),
            max_tokens: 500,
            temperature: 0.0,
            grammar: None,
        },
    };
    let text = match ai.generate(&settings, &request, cancel, on).await? {
        Finish::Done(text) => clean_bullets(&text),
        Finish::Cancelled => return Ok(SummaryOutcome::Cancelled),
    };
    let summary = Summary { text, model: settings.label(), last_message: newest };
    let saved = summary.clone();
    jolta.store.write(|tx| save_summary(tx, thread, &saved))?;
    Ok(SummaryOutcome::Done(summary))
}

/// Keeps the bullet lines and drops any preamble a model adds anyway.
fn clean_bullets(text: &str) -> String {
    let bullets: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")).or_else(|| l.strip_prefix("• ")))
        .map(|l| format!("- {}", l.replace("**", "").trim()))
        .collect();
    if bullets.is_empty() { text.trim().to_string() } else { bullets.join("\n") }
}

/// Writes the part of a message the writer would type, from their
/// instruction, anything they typed already, and the conversation it
/// answers. Returns the text in the body format, for
/// `compose::with_written`; None when AI is off.
pub async fn draft(
    jolta: &Jolta,
    ai: &Ai,
    c: &Compose,
    instruction: &str,
    cancel: &Cancel,
    on: &mut (dyn FnMut(Progress) + Send),
) -> Result<Option<Finish>> {
    let settings: Settings = jolta.store.read_async(super::settings).await?;
    if settings.provider.is_none() {
        return Ok(None);
    }
    let (account, replying_to) = (c.account, c.replying_to);
    let (sender, thread) = jolta
        .store
        .read_async(move |conn| {
            let sender = account.map(|a| store::account(conn, a)).transpose()?;
            let thread: Option<ThreadId> = match replying_to {
                Some(m) => conn.query_row("SELECT thread_id FROM message WHERE id = ?1", [m], |r| r.get(0)).optional()?,
                None => None,
            };
            Ok((sender, thread))
        })
        .await?;
    let (name, address, signature) = sender.map(|a| (a.display_name, a.address, a.signature)).unwrap_or_default();
    let first = name.split_whitespace().next().unwrap_or("").to_string();
    let writer = if first.is_empty() { "The writer".to_string() } else { first.clone() };
    let notes = compose::written(&c.body, &signature);
    let who = if name.is_empty() { address.clone() } else { format!("{name} <{address}>") };
    // Small models copy the greeting of the message they answer ("Hi Koen")
    // unless told whom to greet.
    let greet: Vec<String> = compose::parse_addresses(&c.to)
        .into_iter()
        .filter_map(|(n, _)| n.split_whitespace().next().map(str::to_string))
        .collect();
    let greeting = match greet.as_slice() {
        [] => "a greeting".to_string(),
        [one] => format!("a greeting to {one}"),
        [a, b] => format!("a greeting to {a} and {b}"),
        _ => "a greeting to everyone".to_string(),
    };
    let mut user = String::new();
    let mut language = None;
    match thread {
        Some(t) => {
            let (subject, messages) = load(jolta, t).await?;
            let text = transcript(&messages, Ai::budget(&settings));
            user.push_str(&format!("The conversation, \"{subject}\":\n\n{text}\n\n"));
            user.push_str(&format!("The reply goes to: {}\n\n", if c.to.is_empty() { "the sender" } else { &c.to }));
            if let Some(last) = messages.iter().rev().find(|m| Some(m.meta.id) == replying_to).or(messages.last()) {
                let from = if last.meta.from_name.is_empty() { &last.meta.from_addr } else { &last.meta.from_name };
                user.push_str(&format!("The message to answer, from {from}:\n{}\n\n", last.text));
                language = guess_language(&last.text);
            }
        }
        None => user.push_str(&format!("A new email.\nTo: {}\nSubject: {}\n\n", c.to, c.subject)),
    }
    let instruction = instruction.trim();
    let language = language.or_else(|| guess_language(&format!("{instruction} {}", rich::to_text(&notes))));
    let closing = match language {
        Some("Dutch") => "\"Groeten,\"",
        Some("French") => "\"Bien à vous,\"",
        Some("German") => "\"Viele Grüße,\"",
        Some("Spanish") => "\"Saludos,\"",
        Some(_) => "\"Best,\"",
        None => "a short closing in the email's language",
    };
    let sign = match (signature.trim().is_empty(), first.is_empty()) {
        (false, _) => format!("End with {closing} and no name: Jolta adds the signature below it."),
        (true, false) => format!("End with {closing} and the name {first}."),
        (true, true) => format!("End with {closing}."),
    };
    let reminder = language.map(|l| format!("\n\nWrite the whole email in {l}.")).unwrap_or_default();
    let language = match language {
        Some(l) => format!("Write the email in {l}, whatever language the notes are in, unless the notes ask for another language."),
        None => "Write in the language of the message to answer, or of the notes when there is none.".to_string(),
    };
    let system = format!(
        "You write emails on behalf of {who}. Write only the body of the email: {greeting}, the message, and a closing. {sign} \
         {language} \
         Match the tone of the conversation. Keep it short and concrete. Say everything the instructions ask for: \
         when they say to ask something, ask it as a question. \
         Never invent facts, dates, prices or promises that the instructions and the conversation don't give; \
         write a placeholder in square brackets, like [time], instead. Plain text, no subject line, no markdown."
    );
    if !notes.is_empty() {
        user.push_str(&format!("{writer} started writing this; turn it into the email, keeping what it says:\n{}\n\n", rich::to_text(&notes)));
    }
    user.push_str(&match (instruction.is_empty(), thread.is_some()) {
        (false, _) => format!("{writer}'s notes on what the email must say (follow them, don't quote them): {instruction}"),
        (true, true) => "Write a fitting reply.".to_string(),
        (true, false) => "Write the email.".to_string(),
    });
    user.push_str(&reminder);
    let request = Request { system, user, max_tokens: 700, temperature: 0.3, grammar: None };
    Ok(Some(match ai.generate(&settings, &request, cancel, on).await? {
        Finish::Done(text) => Finish::Done(rich::escape(text.trim())),
        f => f,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_drops_quotes_and_signature() {
        let raw = "From: a@x.test\r\nSubject: Re: Plan\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
Tuesday works.\r\n\r\n\r\n\r\nSee you then.\r\n-- \r\nAnn\r\nCEO\r\n";
        assert_eq!(message_text(raw.as_bytes()), "Tuesday works.\n\nSee you then.");
        let raw = "From: a@x.test\r\nSubject: Re\r\n\r\nYes.\r\n\r\nOn 2 Nov 2026, 10:00, Bob <b@x.test>\r\nwrote:\r\n> Can you?\r\n";
        assert_eq!(message_text(raw.as_bytes()), "Yes.");
        let raw = "From: a@x.test\r\nSubject: Re\r\n\r\nPrima.\r\n\r\nOp ma 2 nov 2026 om 10:00 schreef Bob:\r\n> Lukt het?\r\n";
        assert_eq!(message_text(raw.as_bytes()), "Prima.");
    }

    #[test]
    fn guesses_the_language() {
        assert_eq!(guess_language("Agreed on Cookiebot, leave it out. Here is the list, so that point is done."), Some("English"));
        assert_eq!(guess_language("Ja op beide. Ik stuur je eind week 46 een testomgeving."), Some("Dutch"));
        assert_eq!(guess_language("Merci pour le devis, nous avons une question sur la date."), Some("French"));
        assert_eq!(guess_language("OK"), None);
    }

    #[test]
    fn bullets_lose_preamble_and_bold() {
        assert_eq!(
            clean_bullets("Here is a summary:\n\n- **Launch** moved to 3 Dec.\n* Pauline owes the URLs.\n"),
            "- Launch moved to 3 Dec.\n- Pauline owes the URLs."
        );
        assert_eq!(clean_bullets("Just one line."), "Just one line.");
    }
}
