//! A Jolta data folder in a temporary directory, opened through the same
//! `Core` the Mac app uses, with accounts that have no server: the engine
//! starts no sync for them, so every test runs offline and in milliseconds.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use jolta_core::store::{self, NewAccount, RemoteMailbox};
use jolta_core::{Account, AccountId, AccountKind, Jolta, MailboxId, MailboxRole, MessageId, ThreadId};
use jolta_ffi::{Core, CoreEvent, CoreListener, ListSort, SortKey, ThreadRow};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub const BY_DATE: ListSort = ListSort { key: SortKey::Date, reversed: false };

pub fn unique() -> u64 {
    NEXT.fetch_add(1, Ordering::SeqCst)
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

#[derive(Default)]
pub struct Events(pub Mutex<Vec<&'static str>>);

impl Events {
    pub fn seen(&self, name: &str) -> bool {
        self.0.lock().unwrap().contains(&name)
    }
}

impl CoreListener for Events {
    fn on_event(&self, event: CoreEvent) {
        let name = match event {
            CoreEvent::ListsChanged => "lists",
            CoreEvent::NewMail { .. } => "new mail",
            CoreEvent::Sent { .. } => "sent",
            CoreEvent::SyncFinished { .. } => "synced",
            CoreEvent::ActionFailed { .. } => "action failed",
            CoreEvent::SendFailed { .. } => "send failed",
            CoreEvent::CalendarChanged { .. } => "calendar changed",
            CoreEvent::CalendarSynced { .. } => "calendar synced",
            _ => "other",
        };
        self.0.lock().unwrap().push(name);
    }
}

pub struct App {
    pub core: Arc<Core>,
    /// A second handle on the same folder, for setting up and checking state
    /// the app has no call for.
    pub jolta: Jolta,
    pub dir: PathBuf,
    pub events: Arc<Events>,
}

impl App {
    pub fn new() -> App {
        let app = App::unstarted();
        app.core.start().expect("the engine starts");
        app
    }

    /// The app before `start`: accounts added now are synced from the start.
    pub fn unstarted() -> App {
        jolta_core::secrets::use_memory_store();
        let dir = std::env::temp_dir().join(format!("jolta-regression-{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&dir);
        let events = Arc::new(Events::default());
        let core = Core::open(dir.to_string_lossy().into(), events.clone()).expect("the data folder opens");
        let jolta = Jolta::open(&dir).expect("a second handle opens");
        App { core, jolta, dir, events }
    }

    /// An IMAP account with the usual folders and no server.
    pub fn account(&self, address: &str) -> Account {
        let folders = [
            ("INBOX", Some(MailboxRole::Inbox)),
            ("Archive", Some(MailboxRole::Archive)),
            ("Sent", Some(MailboxRole::Sent)),
            ("Drafts", Some(MailboxRole::Drafts)),
            ("Trash", Some(MailboxRole::Trash)),
            ("Junk", Some(MailboxRole::Spam)),
            ("Projects", None),
        ];
        self.account_with(AccountKind::Imap, address, &folders)
    }

    /// A Gmail account with Gmail's folders, the labels Clients and
    /// Receipts, and no server. Deliver with `deliver_gmail`.
    pub fn gmail_account(&self, address: &str) -> Account {
        let folders = [
            ("INBOX", Some(MailboxRole::Inbox)),
            ("[Gmail]/All Mail", Some(MailboxRole::All)),
            ("[Gmail]/Sent Mail", Some(MailboxRole::Sent)),
            ("[Gmail]/Drafts", Some(MailboxRole::Drafts)),
            ("[Gmail]/Trash", Some(MailboxRole::Trash)),
            ("[Gmail]/Spam", Some(MailboxRole::Spam)),
            ("[Gmail]/Starred", Some(MailboxRole::System)),
            ("Clients", None),
            ("Receipts", None),
        ];
        self.account_with(AccountKind::Gmail, address, &folders)
    }

    /// A message in a Gmail mailbox and, as on Gmail, in All Mail with a UID.
    pub fn deliver_gmail(&self, account: &Account, role: MailboxRole, mail: &Mail) -> MessageId {
        let id = self.deliver(account, role, mail);
        let all = self.mailbox(account.id, MailboxRole::All);
        self.jolta
            .store
            .write(|tx| {
                Ok(tx.execute(
                    "INSERT INTO message_mailbox (mailbox_id, message_id, uid)
                     VALUES (?1, ?2, (SELECT coalesce(max(uid), 0) + 1 FROM message_mailbox WHERE mailbox_id = ?1))",
                    (all, id),
                )?)
            })
            .unwrap();
        id
    }

    pub fn named_mailbox(&self, account: AccountId, name: &str) -> MailboxId {
        self.jolta
            .store
            .read(|c| Ok(store::mailboxes(c, account)?.into_iter().find(|m| m.name == name).expect("the mailbox exists").id))
            .unwrap()
    }

    fn account_with(&self, kind: AccountKind, address: &str, folders: &[(&str, Option<MailboxRole>)]) -> Account {
        self.jolta
            .store
            .write(|tx| {
                let account = store::insert_account(
                    tx,
                    &NewAccount {
                        kind,
                        address: address.into(),
                        display_name: address.split('@').next().unwrap_or_default().into(),
                        username: address.into(),
                        imap_host: String::new(),
                        imap_port: 993,
                        smtp_host: "smtp.test.invalid".into(),
                        smtp_port: 465,
                    },
                )?;
                let remote: Vec<RemoteMailbox> = folders
                    .iter()
                    .map(|(name, role)| RemoteMailbox {
                        important: false,
                        remote_id: name.to_string(),
                        name: name.to_string(),
                        role: *role,
                        delimiter: Some("/".into()),
                        selectable: true,
                    })
                    .collect();
                store::sync_mailbox_list(tx, account.id, &remote)?;
                Ok(account)
            })
            .expect("the account is stored")
    }

    pub fn mailbox(&self, account: AccountId, role: MailboxRole) -> MailboxId {
        self.jolta
            .store
            .read(|c| Ok(store::mailboxes(c, account)?.into_iter().find(|m| m.role == Some(role)).expect("the role exists").id))
            .unwrap()
    }

    pub fn deliver(&self, account: &Account, role: MailboxRole, mail: &Mail) -> MessageId {
        let mailbox = self.mailbox(account.id, role);
        jolta_core::import::import_raw(&self.jolta, account.id, mailbox, &mail.raw()).expect("the message imports")
    }

    pub fn thread_of(&self, message: MessageId) -> ThreadId {
        self.jolta.store.read(|c| Ok(c.query_row("SELECT thread_id FROM message WHERE id = ?1", [message], |r| r.get(0))?)).unwrap()
    }

    /// The list the app would show for this query, newest first. Changes
    /// made through `jolta` don't reach the app's cached list order, so the
    /// cache is pushed out first with another query.
    pub fn list(&self, query: &str) -> Vec<ThreadRow> {
        self.core.list_count("mailbox:0".into(), BY_DATE).unwrap();
        let count = self.core.list_count(query.into(), BY_DATE).expect("the query runs");
        self.core.query_page(query.into(), BY_DATE, 0, count).unwrap()
    }

    pub fn subjects(&self, query: &str) -> Vec<String> {
        self.list(query).into_iter().map(|r| r.subject).collect()
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.jolta.store.read(|c| Ok(c.query_row(sql, [], |r| r.get(0))?)).unwrap()
    }

    pub fn open(&self, thread: ThreadId) -> jolta_ffi::ThreadView {
        block_on(self.core.open_thread(thread.0, String::new(), 3600)).expect("the thread opens")
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

/// A message to import, written as RFC 822 text.
#[derive(Clone)]
pub struct Mail {
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub body: String,
    pub date: i64,
    pub id: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachment: Option<(String, String)>,
    pub headers: Vec<(String, String)>,
}

impl Mail {
    pub fn new(from: &str, to: &str, subject: &str, body: &str) -> Mail {
        Mail {
            from: from.into(),
            to: to.into(),
            cc: String::new(),
            subject: subject.into(),
            body: body.into(),
            date: now() - 3600,
            id: format!("<{}.{}.{}@test.invalid>", std::process::id(), now(), unique()),
            in_reply_to: None,
            references: Vec::new(),
            attachment: None,
            headers: Vec::new(),
        }
    }

    /// A reply to `parent` from `from`, with the usual threading headers.
    pub fn reply(parent: &Mail, from: &str, body: &str) -> Mail {
        let mut m = Mail::new(from, &parent.from, &format!("Re: {}", parent.subject.trim_start_matches("Re: ")), body);
        m.in_reply_to = Some(parent.id.clone());
        m.references = parent.references.iter().cloned().chain([parent.id.clone()]).collect();
        m.date = parent.date + 600;
        m
    }

    pub fn at(mut self, date: i64) -> Mail {
        self.date = date;
        self
    }

    pub fn cc(mut self, cc: &str) -> Mail {
        self.cc = cc.into();
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Mail {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn attach(mut self, name: &str, text: &str) -> Mail {
        self.attachment = Some((name.into(), text.into()));
        self
    }

    pub fn raw(&self) -> Vec<u8> {
        let mut head = format!("From: {}\r\nTo: {}\r\n", self.from, self.to);
        if !self.cc.is_empty() {
            head += &format!("Cc: {}\r\n", self.cc);
        }
        head += &format!(
            "Subject: {}\r\nDate: {}\r\nMessage-ID: {}\r\nMIME-Version: 1.0\r\n",
            self.subject,
            rfc2822(self.date),
            self.id
        );
        if let Some(parent) = &self.in_reply_to {
            head += &format!("In-Reply-To: {parent}\r\n");
        }
        if !self.references.is_empty() {
            head += &format!("References: {}\r\n", self.references.join(" "));
        }
        for (name, value) in &self.headers {
            head += &format!("{name}: {value}\r\n");
        }
        let body = match &self.attachment {
            None => format!("Content-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n", self.body),
            Some((name, text)) => {
                let b = "jolta-boundary";
                format!(
                    "Content-Type: multipart/mixed; boundary=\"{b}\"\r\n\r\n--{b}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n\
                     --{b}\r\nContent-Type: text/plain; name=\"{name}\"\r\nContent-Disposition: attachment; filename=\"{name}\"\r\n\r\n{text}\r\n--{b}--\r\n",
                    self.body
                )
            }
        };
        (head + &body).into_bytes()
    }
}

/// An RFC 2822 date in UTC.
fn rfc2822(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][m as usize - 1];
    format!("{weekday}, {d:02} {month} {y} {:02}:{:02}:{:02} +0000", secs / 3600, secs % 3600 / 60, secs % 60)
}

fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}
