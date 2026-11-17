//! MCP agents: what one connection may do is decided by the agent the user
//! added in Settings, never by the name the connection gives itself.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use jolta_core::MailboxRole::Inbox;
use jolta_core::store::{self, Grant};

use crate::fixture::{App, Mail, unique};
use crate::server::wait;

const ME: &str = "me@test.invalid";

/// An MCP client speaking JSON-RPC, one message per line.
pub struct Agent {
    stream: BufReader<UnixStream>,
    next: u64,
}

impl Agent {
    pub fn open(socket: &Path) -> Agent {
        let mut stream = None;
        wait(5, "the MCP server listens", || {
            stream = UnixStream::connect(socket).ok();
            stream.is_some()
        });
        Agent { stream: BufReader::new(stream.unwrap()), next: 0 }
    }

    pub fn connect(socket: &Path, name: &str) -> Agent {
        let mut agent = Agent::open(socket);
        agent.initialize(name);
        agent
    }

    pub fn initialize(&mut self, name: &str) -> String {
        self.request("initialize", &format!(r#"{{"protocolVersion":"2025-06-18","clientInfo":{{"name":"{name}"}}}}"#))
    }

    pub fn request(&mut self, method: &str, params: &str) -> String {
        self.next += 1;
        let line = format!(r#"{{"jsonrpc":"2.0","id":{},"method":"{method}","params":{params}}}"#, self.next);
        self.stream.get_mut().write_all(format!("{line}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        self.stream.read_line(&mut reply).unwrap();
        reply
    }

    pub fn call(&mut self, tool: &str, arguments: &str) -> String {
        self.request("tools/call", &format!(r#"{{"name":"{tool}","arguments":{arguments}}}"#))
    }
}

pub fn mcp_socket(app: &App) -> PathBuf {
    let socket = std::env::temp_dir().join(format!("jolta-mcp-{}-{}.sock", std::process::id(), unique()));
    app.core.start_mcp(socket.to_string_lossy().into(), 0).unwrap();
    socket
}

/// An agent row as it stood before agents were told apart by token: found
/// by its name alone, approved by the user.
fn approved_by_name(app: &App, name: &str, grants: &[Grant]) {
    app.jolta
        .store
        .write(|tx| {
            tx.execute("INSERT INTO agent (name, state, first_seen, last_seen) VALUES (?1, 'pending', 0, 0)", [name])?;
            store::decide_agent(tx, tx.last_insert_rowid(), true, grants)
        })
        .unwrap();
}

/// One thread in the inbox, and its id.
fn one_thread(app: &App) -> i64 {
    let me = app.account(ME);
    app.thread_of(app.deliver(&me, Inbox, &Mail::new("ann@test.invalid", ME, "Payroll", "The secret numbers."))).0
}

/// A connection that calls itself by an approved agent's name gets none of
/// that agent's grants: it can't search, read or write a draft.
#[test]
fn a_name_alone_gets_no_grants() {
    let app = App::new();
    let thread = one_thread(&app);
    approved_by_name(&app, "Trusted agent", &[Grant::Read, Grant::Organize, Grant::Draft]);
    let socket = mcp_socket(&app);

    let mut impostor = Agent::connect(&socket, "Trusted agent");
    let found = impostor.call("search", r#"{"query":"Payroll"}"#);
    let read = impostor.call("read_thread", &format!(r#"{{"thread_id":{thread}}}"#));
    let draft = impostor.call("create_draft", r#"{"to":"me@test.invalid","subject":"x","body":"x"}"#);
    let _ = std::fs::remove_file(&socket);

    assert!(!found.contains(&format!("thread_id {thread} ")), "search found nothing: {found}");
    assert!(!read.contains("secret numbers"), "read_thread was refused: {read}");
    assert!(draft.contains(r#""isError":true"#), "create_draft was refused: {draft}");
    assert_eq!(app.count("SELECT count(*) FROM draft"), 0, "no draft was written");
}

/// An agent the user turned away can't take on another agent's grants by
/// sending initialize again under that agent's name.
#[test]
fn a_second_initialize_changes_nothing() {
    let app = App::new();
    let thread = one_thread(&app);
    approved_by_name(&app, "Trusted agent", &[Grant::Read, Grant::Draft]);
    let socket = mcp_socket(&app);

    let mut turned_away = Agent::connect(&socket, "Turned away");
    let again = turned_away.initialize("Trusted agent");
    let read = turned_away.call("read_thread", &format!(r#"{{"thread_id":{thread}}}"#));
    let _ = std::fs::remove_file(&socket);

    assert!(again.contains(r#""error""#), "the second initialize was refused: {again}");
    assert!(!read.contains("secret numbers"), "read_thread was refused: {read}");
}
