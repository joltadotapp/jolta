//! `kite`: a developer tool over the Kite core.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use kite_core::imap::{self, SyncEvent, SyncOptions};
use kite_core::search::{self, ThreadRow};
use kite_core::store::{self, NewAccount};
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
    /// Generate a synthetic account with this many messages.
    Gen {
        #[arg(long, default_value_t = 100_000)]
        messages: u64,
    },
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
}

#[derive(Clone, Copy, ValueEnum)]
enum Provider {
    Icloud,
    Fastmail,
    Imap,
}

fn guess_provider(address: &str) -> Provider {
    let domain = address.rsplit('@').next().unwrap_or("").to_lowercase();
    match domain.as_str() {
        "icloud.com" | "me.com" | "mac.com" => Provider::Icloud,
        "fastmail.com" | "fastmail.fm" | "fastmail.net" | "messagingengine.com" => Provider::Fastmail,
        _ => Provider::Imap,
    }
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
            let provider = provider.unwrap_or_else(|| guess_provider(&address));
            let (imap_host, imap_port, smtp_host, smtp_port) = match (provider, imap_host) {
                (_, Some(h)) => {
                    let smtp = smtp_host.unwrap_or_else(|| h.replacen("imap", "smtp", 1));
                    (h, imap_port, smtp, smtp_port)
                }
                (Provider::Icloud, None) => ("imap.mail.me.com".into(), 993, "smtp.mail.me.com".into(), 587),
                (Provider::Fastmail, None) => ("imap.fastmail.com".into(), 993, "smtp.fastmail.com".into(), 465),
                (Provider::Imap, None) => bail!("unknown provider for {address}; pass --imap-host"),
            };
            let password = read_password(password_stdin, &address)?;

            let mut usernames = vec![username.unwrap_or_else(|| address.clone())];
            if matches!(provider, Provider::Icloud) {
                usernames.push(address.split('@').next().unwrap_or(&address).to_string());
            }
            let mut last_err = None;
            let mut working = None;
            for u in &usernames {
                eprintln!("Signing in to {imap_host} as {u}...");
                match imap::connect(&imap_host, imap_port, u, &password).await {
                    Ok(mut s) => {
                        s.logout().await.ok();
                        working = Some(u.clone());
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            let Some(username) = working else {
                bail!("sign-in failed: {}", last_err.map(|e| e.to_string()).unwrap_or_default());
            };

            let new = NewAccount {
                kind: AccountKind::Imap,
                display_name: name.unwrap_or_default(),
                address,
                username,
                imap_host,
                imap_port,
                smtp_host,
                smtp_port,
            };
            let id = kite.store.write(|tx| store::insert_account(tx, &new))?;
            if let Err(e) = secrets::set_password(id, &password) {
                kite.store.write(|tx| store::delete_account(tx, id))?;
                return Err(e).context("saving the password in the Keychain");
            }
            println!("Added account {id}: {}. Run `kite sync {id}` next.", new.address);
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
            secrets::delete(a.id)?;
            println!("Removed account {} ({}).", a.id, a.address);
            Ok(())
        }
    }
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
        None => kite.store.read(store::accounts)?.into_iter().filter(|a| !a.imap_host.is_empty()).collect(),
    };
    for a in accounts {
        let t = Instant::now();
        eprintln!("Syncing {} ({})", a.id, a.address);
        let report = imap::sync_account(kite, &a, SyncOptions { bodies }, |e| match e {
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
        })
        .await?;
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
