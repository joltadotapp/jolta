//! Thread and message ids leave the core: agents get thread ids from MCP
//! search, and the app keeps both in open windows, lists and toasts. An id
//! whose row is deleted must never name a newer row, or a stale id acts on
//! another conversation.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use jolta_core::MailboxRole::Inbox;
use jolta_core::store::{self, Grant};

use crate::fixture::{App, Mail, now, unique};
use crate::server::wait;

const ME: &str = "me@test.invalid";

/// Two conversations, the newer one with the highest thread id, and a reply
/// that names both so the newer one is merged into the older.
struct Merge {
    older: Mail,
    newer: Mail,
}

impl Merge {
    fn new() -> Merge {
        Merge {
            older: Mail::new("ann@test.invalid", ME, "Budget", "First."),
            newer: Mail::new("bob@test.invalid", ME, "Budget numbers", "Second."),
        }
    }

    fn join(&self, app: &App, account: &jolta_core::Account) {
        let mut both = Mail::reply(&self.older, "carl@test.invalid", "Joining these up.");
        both.references.push(self.newer.id.clone());
        app.deliver(account, Inbox, &both);
    }
}

fn unrelated() -> Mail {
    Mail::new("dan@test.invalid", ME, "Holiday photos", "Unrelated.")
}

/// A thread merged into an older one never hands its id to the next new
/// thread, so a window or an agent still holding it names nothing.
#[test]
fn a_merged_thread_keeps_its_id_to_itself() {
    let app = App::new();
    let me = app.account(ME);
    let merge = Merge::new();
    app.deliver(&me, Inbox, &merge.older);
    let merged = app.thread_of(app.deliver(&me, Inbox, &merge.newer));
    merge.join(&app, &me);
    assert_eq!(app.count(&format!("SELECT count(*) FROM thread WHERE id = {}", merged.0)), 0, "the newer thread was merged away");

    let next = app.thread_of(app.deliver(&me, Inbox, &unrelated()));
    assert_ne!(next, merged, "the merged thread's id is not given out again");
}

/// Removing an account never hands its thread ids to another account's
/// new threads.
#[test]
fn a_removed_accounts_thread_ids_are_not_given_out_again() {
    let app = App::new();
    let kept = app.account(ME);
    let gone = app.account("gone@test.invalid");
    app.deliver(&kept, Inbox, &Mail::new("ann@test.invalid", ME, "Kept", "Stays."));
    let old = app.thread_of(app.deliver(&gone, Inbox, &Mail::new("bob@test.invalid", "gone@test.invalid", "Gone", "Goes.")));
    app.core.remove_account(gone.id.0).unwrap();

    let next = app.thread_of(app.deliver(&kept, Inbox, &unrelated()));
    assert_ne!(next, old, "the removed account's thread id is not given out again");
}

/// A message's id carries the second it was received. A message deleted on
/// the server never hands its id to a later message received in the same
/// second.
#[test]
fn a_deleted_message_keeps_its_id_to_itself() {
    let app = App::new();
    let me = app.account(ME);
    let at = now() - 7200;
    app.deliver(&me, Inbox, &Mail::new("ann@test.invalid", ME, "First", "One.").at(at));
    let gone = app.deliver(&me, Inbox, &Mail::new("bob@test.invalid", ME, "Second", "Two.").at(at));
    let inbox = app.mailbox(me.id, Inbox);
    app.jolta.store.write(|tx| store::unlink_messages(tx, inbox, &[gone])).unwrap();

    let later = app.deliver(&me, Inbox, &Mail::new("carl@test.invalid", ME, "Third", "Three.").at(at));
    assert_ne!(later, gone, "the deleted message's id is not given out again");
}

/// An MCP client speaking JSON-RPC, one message per line.
struct Agent {
    stream: BufReader<UnixStream>,
    next: u64,
}

impl Agent {
    fn connect(socket: &Path, name: &str) -> Agent {
        let mut stream = None;
        wait(5, "the MCP server listens", || {
            stream = UnixStream::connect(socket).ok();
            stream.is_some()
        });
        let mut agent = Agent { stream: BufReader::new(stream.unwrap()), next: 0 };
        agent.request("initialize", &format!(r#"{{"protocolVersion":"2025-06-18","clientInfo":{{"name":"{name}"}}}}"#));
        agent
    }

    fn request(&mut self, method: &str, params: &str) -> String {
        self.next += 1;
        let line = format!(r#"{{"jsonrpc":"2.0","id":{},"method":"{method}","params":{params}}}"#, self.next);
        self.stream.get_mut().write_all(format!("{line}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        self.stream.read_line(&mut reply).unwrap();
        reply
    }

    fn call(&mut self, tool: &str, arguments: &str) -> String {
        self.request("tools/call", &format!(r#"{{"name":"{tool}","arguments":{arguments}}}"#))
    }
}

/// An agent searched, then the thread it found was merged into an older
/// one. Archiving with the id it kept archives nothing, where the id used
/// to name the next new conversation and archive that.
#[test]
fn an_agent_holding_a_merged_threads_id_archives_nothing() {
    let app = App::new();
    let me = app.account(ME);
    let merge = Merge::new();
    app.deliver(&me, Inbox, &merge.older);
    let merged = app.thread_of(app.deliver(&me, Inbox, &merge.newer));

    let socket = std::env::temp_dir().join(format!("jolta-ids-{}-{}.sock", std::process::id(), unique()));
    app.core.start_mcp(socket.to_string_lossy().into(), 0).unwrap();
    let mut agent = Agent::connect(&socket, "Stale id agent");
    app.jolta
        .store
        .write(|tx| {
            let (id, _) = store::agent_seen(tx, "Stale id agent")?;
            store::decide_agent(tx, id, true, &[Grant::Read, Grant::Organize])
        })
        .unwrap();
    let found = agent.call("search", r#"{"query":"subject:\"Budget numbers\""}"#);
    assert!(found.contains(&format!("thread_id {} ", merged.0)), "the agent found the newer thread: {found}");

    merge.join(&app, &me);
    app.deliver(&me, Inbox, &unrelated());
    let reply = agent.call("archive", &format!(r#"{{"thread_id":{}}}"#, merged.0));
    let _ = std::fs::remove_file(&socket);
    assert!(app.subjects("in:inbox").contains(&"Holiday photos".to_string()), "the unrelated conversation stays in the inbox; the agent got {reply}");
}
