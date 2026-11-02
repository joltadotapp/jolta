//! `kite`: a developer tool over the Kite core.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use kite_core::imap::{self, SyncEvent, SyncOptions};
use kite_core::search::{self, ThreadRow};
use kite_core::store;
use kite_core::{AccountKind, Kite, MessageId, secrets, synth};

#[derive(Parser)]
#[command(name = "kite", version, about = "Kite mail core, from the command line")]
struct Cli {
    /// Data directory; defaults to the app group container shared with Kite.app.
    #[arg(long, env = "KITE_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add, list and remove accounts.
    #[command(subcommand)]
    Account(AccountCommand),
    /// List an account's mailboxes.
    Mailboxes { account: String },
    /// Sync one account, or all of them.
    Sync {
        account: Option<String>,
        /// Fetch headers only; skip downloading bodies.
        #[arg(long)]
        no_bodies: bool,
    },
    /// List threads matching a query, newest first (empty query: all mail).
    Ls {
        query: Vec<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Search and show the match count and timing.
    Search {
        query: Vec<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print one message as text.
    Show {
        id: i64,
        /// Print the raw RFC 822 source.
        #[arg(long)]
        raw: bool,
    },
    /// Store .eml files in a mailbox without a server (fixtures, tests).
    Import {
        account: String,
        /// Mailbox id, or a role such as inbox.
        mailbox: String,
        files: Vec<PathBuf>,
    },
    /// Time opening a thread for the reader, phase by phase.
    Open { thread: i64 },
    /// Choose the AI provider, or summarise and draft with it.
    #[command(subcommand)]
    Ai(AiCommand),
    /// Generate a synthetic account with this many messages.
    Gen {
        #[arg(long, default_value_t = 100_000)]
        messages: u64,
    },
    /// Send a message through the engine (outbox, undo window, Sent copy).
    Send {
        account: String,
        #[arg(long, default_value = "")]
        to: String,
        #[arg(long, default_value = "")]
        subject: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Reply to this message id (prefills recipients, subject and quote).
        #[arg(long)]
        reply: Option<i64>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        attach: Vec<PathBuf>,
    },
    /// Run the engine and the MCP server on a Unix socket (for testing agents).
    McpServe {
        #[arg(long)]
        socket: PathBuf,
    },
    /// List MCP agents, or approve or deny one.
    Agents {
        /// approve or deny
        decision: Option<String>,
        agent: Option<i64>,
        /// Grants for approve: read,organize,draft
        #[arg(long, default_value = "read,organize,draft")]
        grants: String,
    },
    /// Run the sync engine (IDLE, queued actions) and print its events.
    Watch,
    /// Act on a thread through the engine and wait until the server has it.
    Act {
        #[arg(value_enum)]
        action: ActionArg,
        thread: i64,
        /// Mailbox id the action is taken from (defaults to the inbox).
        #[arg(long)]
        from: Option<i64>,
        /// Undo the action right after it reaches the server.
        #[arg(long)]
        undo: bool,
        /// For snooze: when to wake, in plain words ("tomorrow 9am", "in 2h"), in UTC.
        #[arg(long)]
        until: Option<String>,
        /// Only record the change; Kite.app sends it at its next sync. Needs
        /// no Keychain access, so it works for accounts the app signed in.
        #[arg(long)]
        no_wait: bool,
    },
    /// Mark every unread message matching a query read, and wait until the server has it.
    MarkAllRead { query: String },
    /// Delete everything in a mailbox for good (Empty Trash), here and on the server.
    Empty {
        mailbox: i64,
        /// Only record it; Kite.app deletes on the server at its next sync.
        #[arg(long)]
        no_wait: bool,
    },
    /// Change a mailbox on the server directly, as another mail client would.
    #[command(subcommand)]
    Dev(DevCommand),
    /// Time list and search queries over the store.
    Bench {
        #[arg(long, default_value_t = 30)]
        runs: usize,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Add an account; asks for its (app-specific) password.
    Add {
        address: String,
        #[arg(long, value_enum)]
        provider: Option<Provider>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        imap_host: Option<String>,
        #[arg(long, default_value_t = 993)]
        imap_port: u16,
        #[arg(long)]
        smtp_host: Option<String>,
        #[arg(long, default_value_t = 465)]
        smtp_port: u16,
        /// Read the password from standard input instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    List,
    Remove { account: String },
    /// Sign in with Google (Gmail, Workspace) in the browser.
    AddGoogle {
        /// Listen for the sign-in on this port, for an SSH tunnel (see add-microsoft).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Sign in with Microsoft 365 in the browser. From another machine:
    /// ssh -L 8765:127.0.0.1:8765 <this mac>, then run with --port 8765.
    AddMicrosoft {
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum DevCommand {
    /// APPEND .eml files to a mailbox (by server path, such as INBOX).
    Append {
        account: String,
        mailbox: String,
        files: Vec<PathBuf>,
        #[arg(long)]
        seen: bool,
    },
    /// UID SEARCH, printing matching UIDs. Example criteria: SUBJECT "test".
    Search { account: String, mailbox: String, criteria: Vec<String> },
    /// UID STORE. Example: kite dev store 3 INBOX 12,14 '+FLAGS (\Flagged)'
    Store { account: String, mailbox: String, uids: String, change: String },
    /// Mark UIDs deleted and expunge them.
    Expunge { account: String, mailbox: String, uids: String },
    /// GET a Microsoft Graph path (such as /me/mailFolders/inbox/messages) and print the JSON.
    Graph { account: String, path: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum ActionArg {
    Archive,
    Trash,
    Read,
    Unread,
    Flag,
    Unflag,
    Snooze,
}

#[derive(Clone, Copy, ValueEnum)]
enum Provider {
    Icloud,
    Fastmail,
    Imap,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let cli = Cli::parse();
    let data_dir = cli.data_dir.unwrap_or_else(kite_core::default_data_dir);
    let kite = Kite::open(&data_dir).with_context(|| format!("opening {}", data_dir.display()))?;

    match cli.command {
        Command::Account(cmd) => account(&kite, cmd).await,
        Command::Mailboxes { account } => {
            let a = kite.store.read(|c| store::find_account(c, &account))?;
            for m in kite.store.read(|c| store::mailboxes(c, a.id))? {
                let count: i64 = kite.store.read(|c| {
                    Ok(c.query_row("SELECT count(*) FROM message_mailbox WHERE mailbox_id = ?1", [m.id], |r| r.get(0))?)
                })?;
                let role = m.role.map(|r| r.to_string()).unwrap_or_default();
                println!("{:>5}  {:<8} {:>7}  {}", m.id, role, count, m.name);
            }
            Ok(())
        }
        Command::Sync { account, no_bodies } => sync(&kite, account, !no_bodies).await,
        Command::Ls { query, limit, offset } => {
            let q = search::parse(&query.join(" "))?;
            let rows = kite.store.read(|c| search::query_page(c, &q, offset, limit))?;
            print_rows(&rows);
            Ok(())
        }
        Command::Search { query, limit } => {
            let q = search::parse(&query.join(" "))?;
            let t = Instant::now();
            let rows = kite.store.read(|c| search::query_page(c, &q, 0, limit))?;
            let page_ms = t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            let count = kite.store.read(|c| search::query_count(c, &q))?;
            let count_ms = t.elapsed().as_secs_f64() * 1000.0;
            print_rows(&rows);
            println!("\n{count} threads. First page {page_ms:.2} ms, count {count_ms:.2} ms.");
            Ok(())
        }
        Command::Show { id, raw } => show(&kite, MessageId(id), raw).await,
        Command::Ai(cmd) => ai(&kite, cmd).await,
        Command::Open { thread } => {
            let t = Instant::now();
            let (_, messages) = kite.store.read(|c| kite_core::reader::thread_messages(c, kite_core::ThreadId(thread)))?;
            println!("load rows: {:.2} ms ({} messages)", t.elapsed().as_secs_f64() * 1000.0, messages.len());
            for m in &messages {
                let t = Instant::now();
                let raw = imap::message_source(&kite, m.id).await?;
                println!("  source {}: {:.2} ms, {} bytes", m.id.0, t.elapsed().as_secs_f64() * 1000.0, raw.len());
            }
            for _ in 0..3 {
                let t = Instant::now();
                let detail = kite_core::reader::open_thread(&kite, kite_core::ThreadId(thread), "", 0).await?;
                println!("open_thread total: {:.2} ms, {} bytes of HTML", t.elapsed().as_secs_f64() * 1000.0, detail.rendered.html.len());
            }
            Ok(())
        }
        Command::Import { account, mailbox, files } => {
            let a = kite.store.read(|c| store::find_account(c, &account))?;
            let boxes = kite.store.read(|c| store::mailboxes(c, a.id))?;
            let target = boxes
                .iter()
                .find(|b| b.id.0.to_string() == mailbox || b.role.is_some_and(|r| r.as_str() == mailbox))
                .with_context(|| format!("no mailbox {mailbox} in {}", a.address))?;
            for f in files {
                let raw = std::fs::read(&f).with_context(|| format!("reading {}", f.display()))?;
                let id = kite_core::import::import_raw(&kite, a.id, target.id, &raw)?;
                println!("{id}  {}", f.display());
            }
            Ok(())
        }
        Command::Gen { messages } => {
            let t = Instant::now();
            let id = synth::generate(&kite, messages, |n| {
                eprint!("\r{n} / {messages}");
                std::io::stderr().flush().ok();
            })?;
            eprintln!();
            println!("Account {id} ({}) has {messages} messages, built in {:.1} s.", synth::ADDRESS, t.elapsed().as_secs_f64());
            Ok(())
        }
        Command::Bench { runs } => bench(&kite, runs),
        Command::Dev(cmd) => dev(&kite, cmd).await,
        Command::McpServe { socket } => {
            let t0 = Instant::now();
            let sink: kite_core::engine::Sink = Arc::new(move |e| eprintln!("{:>8.2}s  {e:?}", t0.elapsed().as_secs_f64()));
            let engine = kite_core::engine::Engine::start(kite.clone(), sink.clone(), tokio::runtime::Handle::current())?;
            eprintln!("MCP server on {}", socket.display());
            kite_core::mcp::serve(&socket, kite.clone(), engine, sink).await?;
            Ok(())
        }
        Command::Agents { decision, agent, grants } => {
            if let (Some(d), Some(id)) = (decision, agent) {
                let grants: Vec<_> = grants.split(',').filter_map(|g| store::Grant::parse(g.trim())).collect();
                kite.store.write(|tx| store::decide_agent(tx, id, d == "approve", &grants))?;
            }
            for a in kite.store.read(store::agents)? {
                let grants: Vec<&str> = a.grants.iter().map(|g| g.as_str()).collect();
                println!("{:>3}  {:<24} {:?}  {}", a.id, a.name, a.state, grants.join(","));
            }
            for e in kite.store.read(|c| store::audit_log(c, 10))? {
                println!("  audit: {} {} {} -> {}", e.agent, e.tool, e.summary, e.outcome);
            }
            Ok(())
        }
        Command::Watch => {
            let t0 = Instant::now();
            let sink: kite_core::engine::Sink = Arc::new(move |e| println!("{:>8.2}s  {e:?}", t0.elapsed().as_secs_f64()));
            let _engine = kite_core::engine::Engine::start(kite.clone(), sink, tokio::runtime::Handle::current())?;
            tokio::signal::ctrl_c().await?;
            Ok(())
        }
        Command::Act { action, thread, from, undo, until, no_wait } => act(&kite, action, thread, from, undo, until, no_wait).await,
        Command::MarkAllRead { query } => {
            let q = kite_core::search::parse(&format!("{query} is:unread"))?;
            list_action(&kite, |engine| Ok(engine.mark_all_read(&q)?.len())).await
        }
        Command::Empty { mailbox, no_wait: true } => {
            kite.store.write(|tx| kite_core::actions::empty(tx, kite_core::MailboxId(mailbox)))?;
            println!("recorded; Kite.app empties it on the server at its next sync");
            Ok(())
        }
        Command::Empty { mailbox, no_wait: false } => {
            list_action(&kite, |engine| Ok(engine.empty(&[kite_core::MailboxId(mailbox)]).map(|_| 1)?)).await
        }
        Command::Send { account, to, subject, body, reply, all, attach } => {
            send(&kite, &account, to, subject, body, reply, all, attach).await
        }
    }
}

async fn account(kite: &Kite, cmd: AccountCommand) -> Result<()> {
    match cmd {
        AccountCommand::Add {
            address,
            provider,
            name,
            username,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            password_stdin,
        } => {
            // --provider names the servers for a custom domain on iCloud or Fastmail.
            let (imap_host, imap_port, smtp_host, smtp_port) = match (provider, imap_host) {
                (Some(Provider::Icloud), None) => (Some("imap.mail.me.com".into()), 993, Some("smtp.mail.me.com".into()), 587),
                (Some(Provider::Fastmail), None) => (Some("imap.fastmail.com".into()), 993, Some("smtp.fastmail.com".into()), 465),
                (_, h) => (h, imap_port, smtp_host, smtp_port),
            };
            let password = read_password(password_stdin, &address)?;
            eprintln!("Signing in to check the password...");
            let setup = imap::ImapSetup {
                address,
                password,
                display_name: name.unwrap_or_default(),
                username,
                imap_port: Some(imap_port),
                smtp_port: Some(smtp_port),
                imap_host,
                smtp_host,
            };
            let added = imap::add_account(&kite, setup).await.context("adding the account")?;
            println!("Added account {}: {}. Run `kite sync {}` next.", added.id, added.address, added.id);
            Ok(())
        }
        AccountCommand::AddGoogle { port } | AccountCommand::AddMicrosoft { port } => {
            use kite_core::oauth::{self, Provider};
            let (provider, label) = match cmd {
                AccountCommand::AddGoogle { .. } => (Provider::Google, "Google"),
                _ => (Provider::Microsoft, "Microsoft"),
            };
            let client = provider.client(&kite.data_dir)?;
            let pending = oauth::begin(provider, client, port).await?;
            println!("Opening your browser to sign in with {label}. If it does not open, visit:\n\n{}\n", pending.url);
            if port == 0 {
                println!("Browser on another machine? Connect with ssh -L 8765:127.0.0.1:8765 and run this again with --port 8765.");
                println!("For short addresses, pasting the 127.0.0.1 or localhost address it ends on here also works.");
            } else {
                println!("Waiting on port {port}. Through an SSH tunnel, the browser's redirect comes straight here.");
            }
            if std::env::var_os("KITE_NO_BROWSER").is_none() {
                std::process::Command::new("open").arg(&pending.url).status().ok();
            }
            // A terminal in line-editing mode takes at most 1024 characters per
            // line; Microsoft's redirect is longer. Read raw input instead.
            let raw_tty = std::io::stdin().is_terminal()
                && std::process::Command::new("stty").arg("-icanon").stdin(std::process::Stdio::inherit()).status().is_ok_and(|s| s.success());
            let pasted = async {
                let mut line = String::new();
                let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
                use tokio::io::AsyncBufReadExt;
                match stdin.read_line(&mut line).await {
                    Ok(n) if n > 0 && line.contains("code=") => Some(line),
                    _ => std::future::pending().await,
                }
            };
            let signed = pending.finish_or_paste(pasted).await;
            if raw_tty {
                std::process::Command::new("stty").arg("icanon").stdin(std::process::Stdio::inherit()).status().ok();
            }
            let signed = signed.map_err(|e| anyhow::anyhow!(kite_core::graph::explain_sign_in_error(&e.to_string())))?;
            let account = oauth::save_account(kite, provider, &signed)?;
            println!("Added account {}: {}. Run `kite sync {}` next.", account.id, account.address, account.id);
            Ok(())
        }
        AccountCommand::List => {
            for a in kite.store.read(store::accounts)? {
                println!("{:>3}  {:<6} {:<40} {}", a.id, a.kind, a.address, a.imap_host);
            }
            Ok(())
        }
        AccountCommand::Remove { account } => {
            let a = kite.store.read(|c| store::find_account(c, &account))?;
            kite.store.write(|tx| store::delete_account(tx, a.id))?;
            secrets::delete(&a.secret_ref)?;
            println!("Removed account {} ({}).", a.id, a.address);
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send(
    kite: &Kite,
    account: &str,
    to: String,
    subject: String,
    body: String,
    reply: Option<i64>,
    all: bool,
    attach: Vec<PathBuf>,
) -> Result<()> {
    use kite_core::compose::{self, AttachmentFile, Compose, ReplyKind};
    let a = kite.store.read(|c| store::find_account(c, account))?;
    let mut c = match reply {
        Some(id) => {
            let raw = imap::message_source(kite, MessageId(id)).await?;
            let kind = if all { ReplyKind::ReplyAll } else { ReplyKind::Reply };
            let mut c = kite.store.read(|conn| compose::reply(conn, MessageId(id), &raw, kind, 0))?;
            c.body = format!("{body}{}", c.body);
            c
        }
        None => Compose { account: Some(a.id), to, subject, body, ..Default::default() },
    };
    for path in attach {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        c.attachments.push(AttachmentFile { name, mime_type: "application/octet-stream".into(), path: path.display().to_string() });
    }
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: kite_core::engine::Sink = Arc::new(move |e| {
        let _ = events.send(e);
    });
    let engine = kite_core::engine::Engine::start(kite.clone(), sink, tokio::runtime::Handle::current())?;
    let t0 = Instant::now();
    let id = engine.send(&c, None, None)?;
    println!("queued as outbox {id}: to {} cc {} subject {:?}", c.to, c.cc, c.subject);
    while let Some(e) = rx.recv().await {
        match e {
            kite_core::engine::Event::Sent { subject, .. } => {
                println!("sent {subject:?} after {:.1} s (undo window {} s)", t0.elapsed().as_secs_f64(), kite_core::engine::UNDO_SEND_SECONDS);
                break;
            }
            kite_core::engine::Event::SendFailed(m) => anyhow::bail!("send failed: {m}"),
            _ => {}
        }
    }
    // Let the engine file the Sent copy and mark the original answered.
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;
    Ok(())
}

async fn act(kite: &Kite, action: ActionArg, thread: i64, from: Option<i64>, undo: bool, until: Option<String>, no_wait: bool) -> Result<()> {
    use kite_core::actions::Action;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let wake_at = match &until {
        Some(text) => Some(kite_core::when::parse(text, now, 0).with_context(|| format!("can't read {text:?} as a time"))?),
        None => None,
    };
    let action = match action {
        ActionArg::Archive => Action::Archive,
        ActionArg::Trash => Action::Trash,
        ActionArg::Read => Action::MarkRead,
        ActionArg::Unread => Action::MarkUnread,
        ActionArg::Flag => Action::Flag,
        ActionArg::Unflag => Action::Unflag,
        ActionArg::Snooze => Action::Snooze(wake_at.context("snooze needs --until")?),
    };
    let context = from.map_or(kite_core::actions::Context::Inbox, |m| kite_core::actions::Context::Mailbox(kite_core::MailboxId(m)));
    if no_wait {
        let op = kite.store.write(|tx| kite_core::actions::perform(tx, kite_core::ThreadId(thread), action, context))?;
        println!("{}", if op.is_some() { "recorded; Kite.app sends it at its next sync" } else { "nothing to change" });
        return Ok(());
    }
    let sink: kite_core::engine::Sink = Arc::new(|_| {});
    let engine = kite_core::engine::Engine::start(kite.clone(), sink, tokio::runtime::Handle::current())?;
    let t0 = Instant::now();
    let account = kite.store.read(|c| {
        Ok(c.query_row("SELECT account_id FROM thread WHERE id = ?1", [thread], |r| r.get::<_, kite_core::AccountId>(0))?)
    })?;
    let wait_sent = || async {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if !kite.store.read(|c| store::has_pending_ops(c, account))? {
                return anyhow::Ok(());
            }
        }
    };
    let op = engine.perform(&[kite_core::ThreadId(thread)], action, context)?.pop();
    println!("applied locally in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
    let Some(op) = op else {
        println!("nothing to change");
        return Ok(());
    };
    wait_sent().await?;
    println!("on the server after {:.2} s", t0.elapsed().as_secs_f64());
    if undo {
        let t1 = Instant::now();
        engine.undo(&[op])?;
        wait_sent().await?;
        println!("undone on the server after {:.2} s", t1.elapsed().as_secs_f64());
    }
    Ok(())
}

/// Runs an action over a whole list through the engine and waits until
/// every account has sent its queued ops.
async fn list_action(kite: &Kite, action: impl FnOnce(&kite_core::engine::Engine) -> Result<usize>) -> Result<()> {
    let sink: kite_core::engine::Sink = Arc::new(|_| {});
    let engine = kite_core::engine::Engine::start(kite.clone(), sink, tokio::runtime::Handle::current())?;
    let last: i64 = kite.store.read(|c| Ok(c.query_row("SELECT coalesce(max(id), 0) FROM op", [], |r| r.get(0))?))?;
    let t0 = Instant::now();
    let ops = action(&engine)?;
    println!("{ops} op(s) applied locally in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let pending: bool = kite
            .store
            .read(|c| Ok(c.query_row("SELECT EXISTS (SELECT 1 FROM op WHERE id > ?1 AND state = 'pending')", [last], |r| r.get(0))?))?;
        if !pending {
            break;
        }
    }
    println!("on the server after {:.2} s", t0.elapsed().as_secs_f64());
    Ok(())
}

async fn dev(kite: &Kite, cmd: DevCommand) -> Result<()> {
    use futures::TryStreamExt;
    if let DevCommand::Graph { account, path } = &cmd {
        let a = kite.store.read(|c| store::find_account(c, account))?;
        let json = kite_core::graph::Graph::new(&a)?.get(path).await?;
        println!("{}", serde_json::to_string_pretty(&json)?);
        return Ok(());
    }
    let (account, mailbox) = match &cmd {
        DevCommand::Append { account, mailbox, .. }
        | DevCommand::Search { account, mailbox, .. }
        | DevCommand::Store { account, mailbox, .. }
        | DevCommand::Expunge { account, mailbox, .. } => (account.clone(), mailbox.clone()),
        DevCommand::Graph { .. } => unreachable!("handled above"),
    };
    let a = kite.store.read(|c| store::find_account(c, &account))?;
    let mut session = imap::connect_account(&a).await?;
    match cmd {
        DevCommand::Append { files, seen, .. } => {
            for f in files {
                let raw = std::fs::read(&f).with_context(|| format!("reading {}", f.display()))?;
                session.append(&mailbox, if seen { Some("(\\Seen)") } else { None }, None, &raw).await?;
                println!("appended {}", f.display());
            }
        }
        DevCommand::Search { criteria, .. } => {
            session.select(&mailbox).await?;
            let mut uids: Vec<u32> = session.uid_search(criteria.join(" ")).await?.into_iter().collect();
            uids.sort_unstable();
            println!("{}", uids.iter().map(u32::to_string).collect::<Vec<_>>().join(" "));
        }
        DevCommand::Store { uids, change, .. } => {
            session.select(&mailbox).await?;
            let n = session.uid_store(&uids, &change).await?.try_collect::<Vec<_>>().await?.len();
            println!("changed {n} message(s)");
        }
        DevCommand::Graph { .. } => unreachable!("handled above"),
        DevCommand::Expunge { uids, .. } => {
            session.select(&mailbox).await?;
            session.uid_store(&uids, "+FLAGS.SILENT (\\Deleted)").await?.try_collect::<Vec<_>>().await?;
            let n = session.expunge().await?.try_collect::<Vec<_>>().await?.len();
            println!("expunged {n} message(s)");
        }
    }
    session.logout().await.ok();
    Ok(())
}

fn read_password(from_stdin: bool, address: &str) -> Result<String> {
    let password = if from_stdin || !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        rpassword::prompt_password(format!("App-specific password for {address}: "))?
    };
    let password = password.trim().to_string();
    if password.is_empty() {
        bail!("no password given");
    }
    Ok(password)
}

async fn sync(kite: &Kite, account: Option<String>, bodies: bool) -> Result<()> {
    let accounts = match account {
        Some(spec) => vec![kite.store.read(|c| store::find_account(c, &spec))?],
        None => kite
            .store
            .read(store::accounts)?
            .into_iter()
            .filter(|a| !a.imap_host.is_empty() || a.kind == AccountKind::Graph)
            .collect(),
    };
    for a in accounts {
        let t = Instant::now();
        eprintln!("Syncing {} ({})", a.id, a.address);
        let mut on_event = |e| match e {
            SyncEvent::Mailboxes(n) => eprintln!("  {n} mailboxes"),
            SyncEvent::Headers { mailbox, done, total } => {
                eprint!("\r  {mailbox}: headers {done}/{total}   ");
                std::io::stderr().flush().ok();
            }
            SyncEvent::Mailbox { name, new, removed, flag_changes } => {
                if new > 0 {
                    eprintln!();
                }
                eprintln!("  {name}: {new} new, {removed} removed, {flag_changes} flag changes");
            }
            SyncEvent::Bodies { done } => {
                eprint!("\r  bodies: {done}   ");
                std::io::stderr().flush().ok();
            }
        };
        let report = if a.kind == AccountKind::Graph {
            kite_core::graph::Graph::new(&a)?.sync(kite, &imap::Scope::All, &mut on_event).await?
        } else {
            imap::sync_account(kite, &a, SyncOptions { bodies }, on_event).await?
        };
        if report.bodies > 0 {
            eprintln!();
        }
        println!(
            "{}: {} mailboxes, {} new, {} removed, {} flag changes, {} bodies in {:.1} s",
            a.address,
            report.mailboxes,
            report.new,
            report.removed,
            report.flag_changes,
            report.bodies,
            t.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

fn format_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n - 1).collect::<String>() + "…" }
}

fn print_rows(rows: &[ThreadRow]) {
    for r in rows {
        let from = if r.from_name.is_empty() { &r.from_addr } else { &r.from_name };
        let marks = format!(
            "{}{}{}",
            if r.unread_count > 0 { '●' } else { ' ' },
            if r.flagged { '★' } else { ' ' },
            if r.has_attachments { '📎' } else { ' ' }
        );
        let count = if r.message_count > 1 { format!(" ({})", r.message_count) } else { String::new() };
        println!(
            "{:>16}  {}  {}  {:<22}  {}{}  {}",
            r.message_id.0,
            format_date(r.date),
            marks,
            truncate(from, 22),
            truncate(&r.subject, 60),
            count,
            truncate(&r.snippet, 50)
        );
    }
}

async fn show(kite: &Kite, id: MessageId, raw: bool) -> Result<()> {
    let source = imap::message_source(kite, id).await?;
    if raw {
        std::io::stdout().write_all(&source)?;
        return Ok(());
    }
    let parsed = mail_parser_text(&source);
    println!("{parsed}");
    Ok(())
}

fn mail_parser_text(raw: &[u8]) -> String {
    let (e, body) = kite_core::mime::parse_full(raw, String::new());
    if e.subject.is_empty() && e.from_addr.is_empty() {
        return String::from_utf8_lossy(raw).into_owned();
    }
    format!(
        "From: {} <{}>\nTo: {}\n{}Subject: {}\n\n{}",
        e.from_name,
        e.from_addr,
        e.to,
        if e.cc.is_empty() { String::new() } else { format!("Cc: {}\n", e.cc) },
        e.subject,
        body.text
    )
}

fn bench(kite: &Kite, runs: usize) -> Result<()> {
    let total: i64 = kite.store.read(|c| Ok(c.query_row("SELECT count(*) FROM message", [], |r| r.get(0))?))?;
    // Words of different frequency in the synthetic vocabulary: rank 1, 401 and 20,001.
    let common = synth::word(0);
    let mid = synth::word(400);
    let rare = synth::word(20_000);
    let queries = [
        String::new(),
        "in:inbox".into(),
        "is:unread in:inbox".into(),
        common.clone(),
        mid.clone(),
        rare.clone(),
        format!("{common} {mid}"),
        format!("{}", &common[..2]),
        format!("{}", &mid[..3]),
        format!("from:{}", "peeters"),
        format!("subject:{mid}"),
        format!("has:attachment {mid}"),
        format!("\"{common} {}\"", synth::word(1)),
        "after:2025-01-01 before:2025-02-01".into(),
        format!("-{common} {mid}"),
    ];
    println!("{total} messages, {runs} runs per query; times are first page of 50 threads + match count.\n");
    println!("{:<40} {:>9} {:>9} {:>9} {:>9}", "query", "page p50", "page p95", "count p95", "threads");
    let mut worst: f64 = 0.0;
    for q in &queries {
        let parsed = search::parse(q)?;
        let mut page = Vec::with_capacity(runs);
        let mut count = Vec::with_capacity(runs);
        let mut n = 0;
        for _ in 0..runs {
            let t = Instant::now();
            kite.store.read(|c| search::query_page(c, &parsed, 0, 50))?;
            page.push(t.elapsed().as_secs_f64() * 1000.0);
            let t = Instant::now();
            n = kite.store.read(|c| search::query_count(c, &parsed))?;
            count.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let (p50, p95, c95) = (pct(&mut page, 0.50), pct(&mut page, 0.95), pct(&mut count, 0.95));
        worst = worst.max(p95);
        let label = if q.is_empty() { "(all mail)".to_string() } else { q.clone() };
        println!("{:<40} {:>7.2}ms {:>7.2}ms {:>7.2}ms {:>9}", truncate(&label, 40), p50, p95, c95, n);
    }
    println!("\nWorst first-page p95: {worst:.2} ms (budget 100 ms).");
    Ok(())
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

#[derive(Subcommand)]
enum AiCommand {
    /// Set the provider: off, local, openai or anthropic. The key comes from
    /// KITE_AI_KEY, so it stays out of the shell history.
    Use {
        provider: String,
        #[arg(long)]
        model: Option<String>,
        /// Another address, for a compatible server.
        #[arg(long)]
        url: Option<String>,
    },
    /// List the provider's models.
    Models { provider: String },
    /// Download a local model (gemma-4-e2b or gemma-4-e4b).
    Download { model: String },
    /// Summarise a conversation, streaming the text.
    Summary {
        thread: i64,
        /// Ignore the saved summary.
        #[arg(long)]
        fresh: bool,
        /// Stop after this many seconds, to try cancelling.
        #[arg(long)]
        cancel_after: Option<f64>,
    },
    /// Draft a reply to a message from an instruction.
    Draft { message: i64, instruction: Vec<String> },
    /// Sort the unsorted inbox mail of the last two weeks, printing each answer.
    Sort {
        /// Sort again what was sorted before.
        #[arg(long)]
        again: bool,
        /// Compare with the X-Kite-Expect header of scripts/ai-fixtures.py mail.
        #[arg(long)]
        check: bool,
    },
}

fn ai_provider(name: &str) -> Result<Option<kite_core::ai::Provider>> {
    use kite_core::ai::Provider;
    Ok(match name {
        "off" => None,
        "local" => Some(Provider::Local),
        "openai" => Some(Provider::OpenAi),
        "anthropic" => Some(Provider::Anthropic),
        _ => anyhow::bail!("unknown provider {name}: use off, local, openai or anthropic"),
    })
}

async fn ai(kite: &Kite, cmd: AiCommand) -> Result<()> {
    use kite_core::ai::{self, Progress, tasks};
    use std::io::Write as _;
    let helper = std::env::var_os("KITE_AI_HELPER")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().unwrap_or_default().with_file_name("kite-ai"));
    let engine = ai::Ai::new(&kite.data_dir, helper);
    let start = Instant::now();
    let mut printed = 0;
    let mut on = |p: Progress| match p {
        Progress::Phase(phase) => eprintln!("[{phase:?} after {:.2} s]", start.elapsed().as_secs_f64()),
        Progress::Text(text) => {
            print!("{}", &text[printed..]);
            let _ = std::io::stdout().flush();
            printed = text.len();
        }
    };
    match cmd {
        AiCommand::Use { provider, model, url } => {
            let provider = ai_provider(&provider)?;
            if let (Some(p), Ok(key)) = (provider, std::env::var("KITE_AI_KEY")) {
                ai::set_key(p, &key)?;
            }
            kite.store.write(|tx| {
                let mut s = ai::settings(tx)?;
                s.provider = provider;
                let remote = match provider {
                    Some(ai::Provider::OpenAi) => Some(&mut s.openai),
                    Some(ai::Provider::Anthropic) => Some(&mut s.anthropic),
                    Some(ai::Provider::Local) => {
                        if let Some(m) = &model {
                            s.local_model = m.clone();
                        }
                        None
                    }
                    None => None,
                };
                if let Some(r) = remote {
                    if let Some(m) = &model {
                        r.model = m.clone();
                    }
                    if let Some(u) = &url {
                        r.base_url = u.clone();
                    }
                }
                ai::save_settings(tx, &s)
            })?;
            println!("{:?}", kite.store.read(ai::settings)?);
        }
        AiCommand::Models { provider } => {
            let p = ai_provider(&provider)?.context("models of which provider?")?;
            let s = kite.store.read(ai::settings)?;
            let url = if p == ai::Provider::Anthropic { s.anthropic.base_url } else { s.openai.base_url };
            for m in ai::models(p, &url, ai::key(p)).await? {
                println!("{}  {}", m.id, m.name);
            }
        }
        AiCommand::Download { model } => {
            let m = ai::local::model(&model);
            let done = ai::download::download(&kite.data_dir, m, &ai::Cancel::default(), &mut |step| eprint!("\r{step:?}          ")).await?;
            eprintln!("\n{} {}", m.name, if done { "is ready" } else { "paused" });
        }
        AiCommand::Summary { thread, fresh, cancel_after } => {
            let cancel = ai::Cancel::default();
            if let Some(after) = cancel_after {
                let c = cancel.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs_f64(after)).await;
                    c.cancel();
                });
            }
            let outcome = tasks::summarise(kite, &engine, kite_core::ThreadId(thread), false, fresh, &cancel, &mut on).await?;
            println!();
            eprintln!("[{:.2} s] {outcome:?}", start.elapsed().as_secs_f64());
        }
        AiCommand::Sort { again, check } => {
            let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
                - ai::background::CATCH_UP_DAYS * 86_400;
            if again {
                kite.store.write(|tx| Ok(tx.execute("DELETE FROM ai_triage", [])?))?;
            }
            let ids = kite.store.read(|c| ai::triage::unsorted_inbox(c, since))?;
            let (mut right, mut checked) = ((0, 0), 0);
            for id in ids {
                let t = Instant::now();
                let Some(got) = ai::triage::sort(kite, &engine, id, &ai::Cancel::default()).await? else { continue };
                let raw = imap::message_source(kite, id).await?;
                let subject = kite.store.read(|c| Ok(c.query_row("SELECT subject FROM message WHERE id = ?1", [id], |r| r.get::<_, String>(0))?))?;
                let answer = format!("{} {}", got.category, if got.needs_reply { "yes" } else { "no" });
                let expect = check
                    .then(|| String::from_utf8_lossy(&raw).lines().find_map(|l| l.strip_prefix("X-Kite-Expect:").map(|v| v.trim().to_string())))
                    .flatten();
                let mark = match &expect {
                    Some(e) => {
                        checked += 1;
                        let (ec, er) = e.split_once(' ').unwrap_or((e, ""));
                        right.0 += usize::from(ec == got.category);
                        right.1 += usize::from(er == if got.needs_reply { "yes" } else { "no" });
                        if *e == answer { "ok ".to_string() } else { format!("WANT {e}") }
                    }
                    None => String::new(),
                };
                println!("{:>5.2} s  {:<18} {:<14} {}", t.elapsed().as_secs_f64(), answer, mark, subject);
            }
            if checked > 0 {
                println!("categories {}/{checked}, needs reply {}/{checked}", right.0, right.1);
            }
        }
        AiCommand::Draft { message, instruction } => {
            let raw = imap::message_source(kite, MessageId(message)).await?;
            let compose = kite.store.read(|c| kite_core::compose::reply(c, MessageId(message), &raw, kite_core::compose::ReplyKind::Reply, 0))?;
            let outcome = tasks::draft(kite, &engine, &compose, &instruction.join(" "), &ai::Cancel::default(), &mut on).await?;
            println!();
            if let Some(ai::Finish::Done(text)) = outcome {
                println!("--- body ---\n{}", kite_core::compose::with_written(&compose.body, &kite.store.read(|c| store::account(c, compose.account.expect("reply account")))?.signature, &text));
            }
        }
    }
    Ok(())
}
