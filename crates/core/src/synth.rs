//! A synthetic mailbox for timing: seeded, so every run builds the same mail.
//! Words follow a Zipf distribution over a made-up vocabulary, like real text.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::model::*;
use crate::store::{self, NewAccount, RemoteMailbox};
use crate::{Kite, Result};

pub const ADDRESS: &str = "synthetic@kite.invalid";
const VOCABULARY: usize = 80_000;
const ZIPF_S: f64 = 1.05;
const YEARS: i64 = 5;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn float(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, p: f64) -> bool {
        self.float() < p
    }
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.float().max(1e-12), self.float());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}

const SYLLABLES: &[&str] = &[
    "ka", "lo", "mi", "ne", "ru", "sa", "te", "vo", "xi", "zu", "bra", "cle", "dri", "fro", "gla",
    "pli", "qua", "sto", "tre", "vin", "an", "el", "is", "or", "um", "est", "ion", "ent", "ar", "ol",
];
const FIRST: &[&str] = &[
    "Ada", "Bram", "Chloe", "Daan", "Elise", "Femke", "Gert", "Hanna", "Ilse", "Jonas", "Kato",
    "Lars", "Mila", "Noor", "Otto", "Pieter", "Rosa", "Sven", "Tess", "Wout",
];
const LAST: &[&str] = &[
    "Aerts", "Claes", "Dubois", "Goossens", "Jacobs", "Janssens", "Lambert", "Maes", "Mertens",
    "Peeters", "Smet", "Willems", "Wouters", "Vermeulen", "Hermans",
];
const DOMAINS: &[&str] = &["example.com", "example.org", "example.net", "mail.invalid", "corp.invalid"];

/// The vocabulary word of frequency rank `i` (0 is the most common).
pub fn word(i: usize) -> String {
    let mut n = i + 1;
    let mut w = String::new();
    while n > 0 {
        w.push_str(SYLLABLES[n % SYLLABLES.len()]);
        n /= SYLLABLES.len();
    }
    w
}

struct Corpus {
    words: Vec<String>,
    cumulative: Vec<f64>,
}

impl Corpus {
    fn new() -> Corpus {
        let mut cumulative = Vec::with_capacity(VOCABULARY);
        let mut sum = 0.0;
        for k in 1..=VOCABULARY {
            sum += 1.0 / (k as f64).powf(ZIPF_S);
            cumulative.push(sum);
        }
        Corpus { words: (0..VOCABULARY).map(word).collect(), cumulative }
    }

    fn word(&self, rng: &mut Rng) -> &str {
        let x = rng.float() * self.cumulative[VOCABULARY - 1];
        let i = self.cumulative.partition_point(|&c| c < x).min(VOCABULARY - 1);
        &self.words[i]
    }

    fn words(&self, rng: &mut Rng, n: usize) -> String {
        let mut s = String::new();
        for i in 0..n {
            if i > 0 {
                s.push(if i % 14 == 0 { '\n' } else { ' ' });
            }
            s.push_str(self.word(rng));
        }
        s
    }
}

/// Body text of synthetic message `n`; also used to show it when opened.
pub fn body_text(n: u64) -> String {
    thread_local! {
        static CORPUS: Corpus = Corpus::new();
    }
    let mut rng = Rng(n.wrapping_mul(0xA24B_AED4_963E_E407) ^ 0x5EED);
    let len = (120.0 * (rng.normal() * 0.8).exp()).clamp(5.0, 3000.0) as usize;
    CORPUS.with(|c| c.words(&mut rng, len))
}

/// Adds a synthetic account with `count` messages and returns its id.
pub fn generate(kite: &Kite, count: u64, mut progress: impl FnMut(u64)) -> Result<AccountId> {
    let corpus = Corpus::new();
    let mut rng = Rng(42);
    let (account, inbox, archive, sent) = kite.store.write(|tx| {
        let id = store::insert_account(tx, &NewAccount {
            kind: AccountKind::Imap,
            address: ADDRESS.into(),
            display_name: "Synthetic".into(),
            username: ADDRESS.into(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
        })?
        .id;
        let mb = |name: &str, role| RemoteMailbox {
            important: false,
            remote_id: name.into(),
            name: name.into(),
            role: Some(role),
            delimiter: Some("/".into()),
            selectable: true,
        };
        let boxes = store::sync_mailbox_list(tx, id, &[
            mb("INBOX", MailboxRole::Inbox),
            mb("Archive", MailboxRole::Archive),
            mb("Sent", MailboxRole::Sent),
        ])?;
        let find = |r| boxes.iter().find(|b| b.role == Some(r)).map(|b| b.id).expect("created above");
        Ok((id, find(MailboxRole::Inbox), find(MailboxRole::Archive), find(MailboxRole::Sent)))
    })?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let start = now - YEARS * 365 * 86_400;
    let step = (now - start) as f64 / count.max(1) as f64;
    let people: Vec<(String, String)> = (0..2000)
        .map(|i| {
            let (f, l) = (FIRST[i % FIRST.len()], LAST[(i / FIRST.len()) % LAST.len()]);
            let addr = format!("{}.{}{}@{}", f.to_lowercase(), l.to_lowercase(), i, DOMAINS[i % DOMAINS.len()]);
            (format!("{f} {l}"), addr)
        })
        .collect();
    let lists = ["news@list.example.com", "updates@shop.example.org", "digest@forum.example.net"];

    let mut recent: Vec<(String, String)> = Vec::new();
    let batch = 5000;
    let mut n = 0u64;
    while n < count {
        let end = (n + batch).min(count);
        kite.store.write(|tx| {
            for i in n..end {
                let date = start + (i as f64 * step) as i64 + rng.below(60) as i64;
                let age_days = (now - date) / 86_400;
                let person = &people[(rng.float().powi(3) * people.len() as f64) as usize];
                let from_me = rng.chance(0.1);
                let list = (!from_me && rng.chance(0.15)).then(|| lists[rng.below(lists.len())]);
                let reply = (!recent.is_empty() && rng.chance(0.35)).then(|| recent[rng.below(recent.len())].clone());
                let subject = match &reply {
                    Some((_, s)) => format!("Re: {s}"),
                    None => {
                        let n = 3 + rng.below(7);
                        let s = corpus.words(&mut rng, n).replace('\n', " ");
                        let mut c = s.chars();
                        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or(s)
                    }
                };
                let message_id = format!("synth{i}@kite.invalid");
                let text = body_text(i);
                let (from_name, from_addr) = match (from_me, list) {
                    (true, _) => ("Me".to_string(), ADDRESS.to_string()),
                    (_, Some(l)) => (l.split('@').nth(1).unwrap_or(l).to_string(), l.to_string()),
                    _ => person.clone(),
                };
                let mailbox = if from_me {
                    sent
                } else if age_days < 30 && rng.chance(0.6) || rng.chance(0.01) {
                    inbox
                } else {
                    archive
                };
                let seen = from_me || age_days > 7 || rng.chance(0.5);
                let flags = Flags::default().with(Flags::SEEN, seen).with(Flags::FLAGGED, rng.chance(0.03));
                let has_attachments = rng.chance(0.12);
                store::insert_message(tx, &NewMessage {
                    account_id: account,
                    remote_id: format!("synth/{i}"),
                    mailbox_id: mailbox,
                    uid: Some(i as u32 + 1),
                    date,
                    envelope: Envelope {
                        message_id: Some(message_id.clone()),
                        in_reply_to: reply.iter().map(|(id, _)| id.clone()).collect(),
                        references: reply.iter().map(|(id, _)| id.clone()).collect(),
                        from_name,
                        from_addr,
                        to: if from_me { person.1.clone() } else { ADDRESS.into() },
                        cc: String::new(),
                        subject: subject.clone(),
                        list_id: list.map(str::to_string),
                        list_unsubscribe: None,
                        has_attachments,
                    },
                    flags,
                    size: text.len() as u32 + 600,
                    thread_hint: None,
                    body: Some(BodyInfo {
                        blob: format!("synth:{i}"),
                        snippet: crate::mime::snippet(&text),
                        text,
                        has_attachments,
                        ..Default::default()
                    }),
                })?;
                if reply.is_none() {
                    recent.push((message_id, subject));
                    if recent.len() > 200 {
                        recent.remove(0);
                    }
                }
            }
            Ok(())
        })?;
        n = end;
        progress(n);
    }
    Ok(account)
}

/// A minimal RFC 822 message for synthetic message `n`, so it opens like real mail.
pub fn raw_message(conn: &Connection, id: MessageId, n: u64) -> Result<Vec<u8>> {
    let (from_name, from_addr, to, subject): (String, String, String, String) = conn.query_row(
        "SELECT from_name, from_addr, to_addrs, subject FROM message WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let body = body_text(n).replace('\n', "\r\n");
    Ok(format!(
        "From: {from_name} <{from_addr}>\r\nTo: {to}\r\nSubject: {subject}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
    )
    .into_bytes())
}
