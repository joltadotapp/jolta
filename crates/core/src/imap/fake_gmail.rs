//! A Gmail IMAP server in memory, for tests of Gmail mode. Like Gmail, it
//! keeps each message in exactly one of All Mail, Trash and Spam, and the
//! other folders are labels on All Mail's messages. It speaks the commands
//! Jolta's Gmail sync and ops send, and nothing else.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::Session;

pub const ALL: &str = "[Gmail]/All Mail";
pub const TRASH: &str = "[Gmail]/Trash";
pub const SPAM: &str = "[Gmail]/Spam";
const HOMES: [&str; 3] = [ALL, TRASH, SPAM];
const LIST: [(&str, &str); 6] = [
    ("INBOX", "\\HasNoChildren"),
    (ALL, "\\HasNoChildren \\All"),
    (TRASH, "\\HasNoChildren \\Trash"),
    (SPAM, "\\HasNoChildren \\Junk"),
    ("[Gmail]/Drafts", "\\HasNoChildren \\Drafts"),
    ("Work", "\\HasNoChildren"),
];

#[derive(Clone, Debug)]
struct Message {
    home: &'static str,
    uid: u32,
    msgid: u64,
    labels: Vec<String>,
    seen: bool,
    subject: String,
}

#[derive(Default)]
struct State {
    messages: Vec<Message>,
    next_uid: u32,
    next_msgid: u64,
}

#[derive(Clone, Default)]
pub struct FakeGmail(Arc<Mutex<State>>);

fn home_of(name: &str) -> Option<&'static str> {
    HOMES.into_iter().find(|h| *h == name)
}

impl FakeGmail {
    /// A new message in All Mail with these labels, such as `\Inbox`.
    pub fn deliver(&self, subject: &str, labels: &[&str]) {
        let mut s = self.0.lock().unwrap();
        s.next_uid += 1;
        s.next_msgid += 1;
        let (uid, msgid) = (s.next_uid, 1_000 + s.next_msgid);
        s.messages.push(Message {
            home: ALL,
            uid,
            msgid,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            seen: false,
            subject: subject.into(),
        });
    }

    /// Moves a message as Gmail on the web does: to Trash or Spam, or back.
    pub fn move_to(&self, subject: &str, folder: &str) {
        let mut s = self.0.lock().unwrap();
        let i = s.messages.iter().position(|m| m.subject == subject).expect("the message exists");
        s.move_message(i, folder);
    }

    /// The subjects in a folder: a home, or a label of All Mail's messages.
    pub fn subjects(&self, folder: &str) -> Vec<String> {
        let s = self.0.lock().unwrap();
        let mut out: Vec<String> = s.messages.iter().filter(|m| m.in_folder(folder)).map(|m| m.subject.clone()).collect();
        out.sort();
        out
    }

    pub fn seen(&self, subject: &str) -> bool {
        self.0.lock().unwrap().messages.iter().any(|m| m.subject == subject && m.seen)
    }

    /// A signed-in session on a connection of its own.
    pub async fn connect(&self) -> Session {
        let (client, server) = tokio::io::duplex(1 << 20);
        let fake = self.clone();
        tokio::spawn(async move { fake.serve(server).await });
        let client = async_imap::Client::new(Box::new(client) as Box<dyn super::Io>);
        client.login("user", "password").await.map_err(|(e, _)| e).expect("the fake signs in")
    }

    async fn serve(self, stream: DuplexStream) {
        let (read, mut write) = tokio::io::split(stream);
        let mut lines = BufReader::new(read).lines();
        let mut selected: Option<String> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let Some((tag, command)) = line.split_once(' ') else { continue };
            let reply = self.answer(command, &mut selected);
            let done = command.eq_ignore_ascii_case("LOGOUT");
            if write.write_all(format!("{reply}{tag} OK done\r\n").as_bytes()).await.is_err() || done {
                return;
            }
        }
    }

    fn answer(&self, command: &str, selected: &mut Option<String>) -> String {
        let words = split_words(command);
        let upper: Vec<String> = words.iter().map(|w| w.to_uppercase()).collect();
        let mut s = self.0.lock().unwrap();
        match upper.first().map(String::as_str) {
            Some("LIST") => LIST.iter().map(|(name, attrs)| format!("* LIST ({attrs}) \"/\" \"{name}\"\r\n")).collect(),
            Some("SELECT" | "EXAMINE") => {
                let name = words[1].clone();
                let exists = s.messages.iter().filter(|m| m.in_folder(&name)).count();
                *selected = Some(name);
                format!("* {exists} EXISTS\r\n* OK [UIDVALIDITY 7] ok\r\n")
            }
            Some("UID") => {
                let folder = selected.clone().expect("a folder is selected");
                let set = parse_set(&words[2]);
                let in_set = |m: &Message| m.in_folder(&folder) && set.iter().any(|(a, b)| (*a..=*b).contains(&m.uid));
                match upper[1].as_str() {
                    "SEARCH" => {
                        if upper.get(2).map(String::as_str) == Some("X-GM-MSGID") {
                            let msgid: u64 = words[3].parse().unwrap();
                            let uids: Vec<String> =
                                s.messages.iter().filter(|m| m.in_folder(&folder) && m.msgid == msgid).map(|m| m.uid.to_string()).collect();
                            return format!("* SEARCH {}\r\n", uids.join(" "));
                        }
                        let uids: Vec<String> = s.messages.iter().filter(|m| m.in_folder(&folder)).map(|m| m.uid.to_string()).collect();
                        format!("* SEARCH {}\r\n", uids.join(" "))
                    }
                    "FETCH" => {
                        let headers = upper[3..].iter().any(|w| w.contains("BODY.PEEK[HEADER]"));
                        let mut out = String::new();
                        for (n, m) in s.messages.iter().filter(|m| m.in_folder(&folder)).enumerate() {
                            if in_set(m) {
                                out.push_str(&m.fetch_line(n + 1, headers));
                            }
                        }
                        out
                    }
                    "STORE" => {
                        let item = upper[3].as_str();
                        let values: Vec<String> = words[4..].iter().map(|w| w.trim_matches(|c| c == '(' || c == ')').to_string()).collect();
                        for m in s.messages.iter_mut().filter(|m| in_set(m)) {
                            match item {
                                "+FLAGS.SILENT" => m.seen |= values.iter().any(|v| v == "\\Seen"),
                                "-FLAGS.SILENT" => m.seen &= !values.iter().any(|v| v == "\\Seen"),
                                "+X-GM-LABELS" => m.labels.extend(values.iter().cloned()),
                                "-X-GM-LABELS" => m.labels.retain(|l| !values.contains(l)),
                                other => panic!("the fake doesn't know STORE {other}"),
                            }
                        }
                        String::new()
                    }
                    "MOVE" => {
                        let target = words[3].clone();
                        let moving: Vec<usize> = (0..s.messages.len()).filter(|&i| in_set(&s.messages[i])).collect();
                        for i in moving {
                            s.move_message(i, &target);
                        }
                        String::new()
                    }
                    other => panic!("the fake doesn't know UID {other}"),
                }
            }
            _ => String::new(),
        }
    }
}

impl State {
    /// Gmail's MOVE: into Trash or Spam the message leaves All Mail; into any
    /// other folder it lands in All Mail with that folder's label.
    fn move_message(&mut self, i: usize, folder: &str) {
        self.next_uid += 1;
        let uid = self.next_uid;
        let m = &mut self.messages[i];
        m.uid = uid;
        match home_of(folder) {
            Some(home) => {
                m.home = home;
                if home != ALL {
                    m.labels.retain(|l| l != "\\Inbox");
                }
            }
            None => {
                m.home = ALL;
                let label = if folder == "INBOX" { "\\Inbox".to_string() } else { folder.to_string() };
                if !m.labels.contains(&label) {
                    m.labels.push(label);
                }
            }
        }
    }
}

impl Message {
    fn in_folder(&self, folder: &str) -> bool {
        match home_of(folder) {
            Some(home) => self.home == home,
            None if folder == "INBOX" => self.home == ALL && self.labels.iter().any(|l| l == "\\Inbox"),
            None => self.home == ALL && self.labels.iter().any(|l| l == folder),
        }
    }

    fn fetch_line(&self, seq: usize, headers: bool) -> String {
        let flags = if self.seen { "\\Seen" } else { "" };
        let labels: Vec<String> = self.labels.iter().map(|l| format!("\"{}\"", l.replace('\\', "\\\\"))).collect();
        let mut line = format!("* {seq} FETCH (UID {} FLAGS ({flags}) X-GM-LABELS ({})", self.uid, labels.join(" "));
        if headers {
            let header = format!("Message-ID: <{}@fake.invalid>\r\nSubject: {}\r\nFrom: ann@test.invalid\r\n\r\n", self.msgid, self.subject);
            line.push_str(&format!(
                " INTERNALDATE \"09-Nov-2026 10:00:00 +0000\" RFC822.SIZE 100 X-GM-MSGID {} X-GM-THRID {} BODY[HEADER] {{{}}}\r\n{header}",
                self.msgid,
                self.msgid,
                header.len()
            ));
        }
        line.push_str(")\r\n");
        line
    }
}

/// The command's words, with quoted strings as one word without quotes.
fn split_words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => word.extend(chars.next()),
            ' ' if !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// `1:3,7,9:*` as inclusive ranges.
fn parse_set(set: &str) -> Vec<(u32, u32)> {
    let n = |s: &str| if s == "*" { u32::MAX } else { s.parse().unwrap_or(0) };
    set.split(',')
        .map(|part| match part.split_once(':') {
            Some((a, b)) => (n(a), n(b)),
            None => (n(part), n(part)),
        })
        .collect()
}
