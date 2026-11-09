//! The local Dovecot and Mailpit containers from `scripts/test-servers.sh`.
//! Each test signs in as its own new user, so it starts with an empty
//! mailbox and tests can run side by side.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::TryStreamExt;
use jolta_core::imap::{Endpoint, ImapSetup, Session};
use jolta_core::{Account, Security};

use crate::fixture::{App, Mail, block_on, unique};

const IMAP: &str = "127.0.0.1:31143";
const MAILPIT_API: &str = "127.0.0.1:31080";
const PASSWORD: &str = "jolta-test";

/// True when the servers answer. Without them the sync tests skip, unless
/// `JOLTA_SYNC_TESTS=1` asks for them, in which case they fail.
pub fn available() -> bool {
    let up = TcpStream::connect_timeout(&IMAP.parse::<SocketAddr>().unwrap(), Duration::from_millis(300)).is_ok();
    if !up {
        assert!(std::env::var_os("JOLTA_SYNC_TESTS").is_none(), "JOLTA_SYNC_TESTS is set but Dovecot isn't running; run scripts/test-servers.sh up");
        eprintln!("skipped: the test mail servers aren't running (scripts/test-servers.sh up)");
    }
    up
}

pub struct Mailbox {
    pub user: String,
}

impl Mailbox {
    pub fn new() -> Mailbox {
        Mailbox { user: format!("u{}-{}-{}@test.invalid", std::process::id(), crate::fixture::now(), unique()) }
    }

    /// Runs `f` on a fresh IMAP session of this user.
    pub fn with<T>(&self, f: impl AsyncFnOnce(&mut Session) -> T) -> T {
        block_on(async {
            let endpoint = Endpoint { host: "127.0.0.1".into(), port: 31143, security: Security::Plain, verify_certs: false };
            let mut session = jolta_core::imap::connect(&endpoint, &self.user, PASSWORD).await.expect("Dovecot signs in");
            let out = f(&mut session).await;
            session.logout().await.ok();
            out
        })
    }

    pub fn append(&self, folder: &str, mail: &Mail) {
        let raw = mail.raw();
        self.with(async |s| s.append(folder, None, None, &raw).await.expect("APPEND works"));
    }

    /// The subjects in a server folder, sorted.
    pub fn subjects(&self, folder: &str) -> Vec<String> {
        let folder = folder.to_string();
        let mut out = self.with(async move |s| {
            s.select(&folder).await.expect("the folder exists");
            let fetched: Vec<_> = s.fetch("1:*", "ENVELOPE").await.expect("FETCH works").try_collect().await.unwrap_or_default();
            fetched
                .iter()
                .filter_map(|f| f.envelope()?.subject.as_ref().map(|s| String::from_utf8_lossy(s).into_owned()))
                .collect::<Vec<_>>()
        });
        out.sort();
        out
    }

    /// The flags of the message with this subject in a server folder.
    pub fn flags(&self, folder: &str, subject: &str) -> Vec<String> {
        let (folder, subject) = (folder.to_string(), subject.to_string());
        self.with(async move |s| {
            s.select(&folder).await.expect("the folder exists");
            let uids = s.uid_search(format!("SUBJECT \"{subject}\"")).await.expect("SEARCH works");
            let uid = uids.into_iter().next().expect("the message is on the server");
            let fetched: Vec<_> = s.uid_fetch(uid.to_string(), "FLAGS").await.unwrap().try_collect().await.unwrap();
            fetched[0].flags().map(|f| format!("{f:?}")).collect()
        })
    }

    /// Runs one command on a selected folder, such as `UID STORE`.
    pub fn store(&self, folder: &str, subject: &str, change: &str) {
        let (folder, subject, change) = (folder.to_string(), subject.to_string(), change.to_string());
        self.with(async move |s| {
            s.select(&folder).await.unwrap();
            let uid = s.uid_search(format!("SUBJECT \"{subject}\"")).await.unwrap().into_iter().next().expect("the message is on the server");
            let _: Vec<_> = s.uid_store(uid.to_string(), &change).await.unwrap().try_collect().await.unwrap();
            if change.contains("Deleted") {
                let _: Vec<_> = s.expunge().await.unwrap().try_collect().await.unwrap();
            }
        });
    }

    /// Closes every connection this user has open, as Gmail does to an
    /// IMAP connection it considers idle.
    pub fn drop_connections(&self) {
        let out = std::process::Command::new("docker")
            .args(["exec", "jolta-test-dovecot", "doveadm", "kick", &self.user])
            .output()
            .expect("docker runs");
        assert!(out.status.success(), "doveadm kick failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Adds this user as an account and starts the app.
    pub fn sign_in(&self, app: &App) -> Account {
        let account = self.add(app);
        app.core.start().unwrap();
        account
    }

    /// Adds the account without starting the engine.
    pub fn add(&self, app: &App) -> Account {
        let setup = ImapSetup {
            address: self.user.clone(),
            password: PASSWORD.into(),
            display_name: "Tester".into(),
            imap_host: Some("127.0.0.1".into()),
            imap_port: Some(31143),
            smtp_host: Some("127.0.0.1".into()),
            smtp_port: Some(31025),
            ..Default::default()
        };
        block_on(jolta_core::imap::add_account(&app.jolta, setup)).expect("the account signs in")
    }
}

/// Passes connections through to Dovecot until `freeze`, after which the
/// ones already open go silent both ways while staying open, as when a
/// router forgets a connection. Connections opened later pass again.
pub struct Relay {
    pub port: u16,
    generation: Arc<AtomicU64>,
}

impl Relay {
    pub fn start() -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let Ok(server) = TcpStream::connect(IMAP) else { continue };
                let born = current.load(Ordering::SeqCst);
                for (mut from, mut to) in [(client.try_clone().unwrap(), server.try_clone().unwrap()), (server, client)] {
                    let current = current.clone();
                    std::thread::spawn(move || {
                        let mut buf = [0u8; 65536];
                        while let Ok(n @ 1..) = from.read(&mut buf) {
                            if current.load(Ordering::SeqCst) != born {
                                return std::thread::park();
                            }
                            if to.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        to.shutdown(std::net::Shutdown::Write).ok();
                    });
                }
            }
        });
        Relay { port, generation }
    }

    pub fn freeze(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// Waits up to `seconds` for `ready`, polling every 100 ms.
pub fn wait(seconds: u64, what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out after {seconds} s waiting until {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Messages Mailpit caught whose subject contains `subject`, as raw JSON.
pub fn caught(subject: &str) -> String {
    let mut stream = TcpStream::connect(MAILPIT_API).expect("Mailpit answers");
    let query: String = subject.chars().map(|c| if c == ' ' { '+' } else { c }).collect();
    write!(stream, "GET /api/v1/search?query=subject:%22{query}%22 HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response.split_once("\r\n\r\n").map(|(_, body)| body.to_string()).unwrap_or_default()
}
