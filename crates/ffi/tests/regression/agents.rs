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

    /// Connects the way jolta-mcp does: the token first, then initialize.
    pub fn paired(socket: &Path, token: &str) -> Agent {
        let mut agent = Agent::open(socket);
        agent.send_token(token);
        agent.initialize("Some agent");
        agent
    }

    pub fn send_token(&mut self, token: &str) {
        let line = format!(r#"{{"jsonrpc":"2.0","method":"jolta/token","params":{{"token":"{token}"}}}}"#);
        self.stream.get_mut().write_all(format!("{line}\n").as_bytes()).unwrap();
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

fn add(app: &App, name: &str, grants: &[Grant]) -> String {
    app.jolta.store.write(|tx| store::add_agent(tx, name, grants)).unwrap().1
}

/// An agent added in Settings connects with its token and gets exactly the
/// grants it was given there, whatever name it sends.
#[test]
fn an_added_agent_gets_its_own_grants() {
    let app = App::new();
    let thread = one_thread(&app);
    let token = add(&app, "Reader", &[Grant::Read]);
    let socket = mcp_socket(&app);

    let mut reader = Agent::paired(&socket, &token);
    let read = reader.call("read_thread", &format!(r#"{{"thread_id":{thread}}}"#));
    let draft = reader.call("create_draft", r#"{"to":"me@test.invalid","subject":"x","body":"x"}"#);
    let _ = std::fs::remove_file(&socket);

    assert!(read.contains("secret numbers"), "read_thread worked: {read}");
    assert!(draft.contains(r#""isError":true"#) && app.count("SELECT count(*) FROM draft") == 0, "create_draft was refused: {draft}");
    let added = &app.core.agents().unwrap()[0];
    assert_eq!((added.name.as_str(), added.state.as_str(), added.paired), ("Reader", "approved", true));
}

#[test]
fn agent_draft_body_preserves_plain_text_and_reply_quotes() {
    use jolta_core::rich;

    let app = App::new();
    let thread = one_thread(&app);
    let token = add(&app, "Writer", &[Grant::Draft]);
    let socket = mcp_socket(&app);
    let mut agent = Agent::paired(&socket, &token);

    let typed = "See [section 2], 2*3*4 = 24, snake_case_name and ~approx~.\nVisit https://example.test/help";
    for reply_to in [None, Some(thread)] {
        let arguments = serde_json::json!({"body": typed, "reply_to_thread_id": reply_to});
        let result = agent.call("create_draft", &arguments.to_string());
        let id = app.count("SELECT coalesce(max(id), 0) FROM draft");
        let draft = app.core.draft(id).unwrap().unwrap_or_else(|| panic!("draft was not saved: {result}"));
        let plain = rich::to_text(&draft.body);
        assert!(plain.starts_with(typed), "plain text changed: {plain}");
        assert!(rich::images(&draft.body).is_empty());
        let html = rich::to_html(&draft.body, &|_| panic!("an image was not expected"));
        assert!(html.contains("2*3*4 = 24"));
        assert!(html.contains(r#"href="https://example.test/help""#));
        if reply_to.is_some() {
            assert!(plain.contains("> The secret numbers."));
            assert!(html.contains("<blockquote"));
        } else {
            assert_eq!(plain, typed);
        }
    }
    let _ = std::fs::remove_file(&socket);
}

/// A read-only agent that knows another agent's token still can't switch
/// to it on an open connection.
#[test]
fn a_paired_connection_keeps_its_agent() {
    let app = App::new();
    one_thread(&app);
    let reader = add(&app, "Reader", &[Grant::Read]);
    let writer = add(&app, "Writer", &[Grant::Read, Grant::Draft]);
    let socket = mcp_socket(&app);

    let mut agent = Agent::paired(&socket, &reader);
    agent.send_token(&writer);
    let again = agent.initialize("Writer");
    let draft = agent.call("create_draft", r#"{"to":"me@test.invalid","subject":"x","body":"x"}"#);
    let _ = std::fs::remove_file(&socket);

    assert!(again.contains("already initialized"), "the second initialize was refused: {again}");
    assert!(draft.contains(r#""isError":true"#) && app.count("SELECT count(*) FROM draft") == 0, "create_draft was refused: {draft}");
}

/// Pairing an agent again gives it a new token; the old one stops working.
#[test]
fn pairing_again_retires_the_old_token() {
    let app = App::new();
    let thread = one_thread(&app);
    let old = add(&app, "Reader", &[Grant::Read]);
    let id = app.core.agents().unwrap()[0].id;
    let new = app.core.pair_agent(id, vec!["read".into()]).unwrap();
    let socket = mcp_socket(&app);

    let read_with = |token: &str| Agent::paired(&socket, token).call("read_thread", &format!(r#"{{"thread_id":{thread}}}"#));
    let (with_old, with_new) = (read_with(&old), read_with(&new));
    let _ = std::fs::remove_file(&socket);

    assert!(with_old.contains("Settings › Agents"), "the old token is unknown: {with_old}");
    assert!(with_new.contains("secret numbers"), "the new token works: {with_new}");
}

/// A connection that is already open loses its agent when the user pairs that agent again.
#[test]
fn pairing_again_cuts_off_an_open_connection() {
    let app = App::new();
    let thread = one_thread(&app);
    let old = add(&app, "Reader", &[Grant::Read]);
    let id = app.core.agents().unwrap()[0].id;
    let socket = mcp_socket(&app);
    let mut open = Agent::paired(&socket, &old);
    let read = format!(r#"{{"thread_id":{thread}}}"#);
    let before = open.call("read_thread", &read);
    app.core.pair_agent(id, vec!["read".into()]).unwrap();
    let after = open.call("read_thread", &read);
    let _ = std::fs::remove_file(&socket);

    assert!(before.contains("secret numbers"), "the token works at first: {before}");
    assert!(after.contains("Settings › Agents"), "the old token stops working: {after}");
}

/// No two agents share a name, so Settings and the audit log stay readable.
#[test]
fn an_agent_name_is_used_once() {
    let app = App::new();
    add(&app, "Reader", &[Grant::Read]);
    let again = app.core.add_agent("Reader".into(), vec![]);
    assert!(again.is_err_and(|e| e.to_string().contains("already an agent called Reader")));
}
