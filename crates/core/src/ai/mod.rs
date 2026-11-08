//! Language models for summaries and reply drafts: a model on this Mac (run
//! by the `jolta-ai` helper), or the user's own OpenAI or Anthropic account.
//! Requests go straight from the Mac to the chosen provider; none of ours
//! sits in between.

pub mod background;
pub mod download;
pub mod local;
mod remote;
pub mod quick;
pub mod search;
pub mod tasks;
pub mod triage;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::{Result, secrets};

pub use remote::{ANTHROPIC_URL, ModelChoice, OPENAI_URL, models};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Local,
    OpenAi,
    Anthropic,
}

impl Provider {
    fn name(self) -> &'static str {
        match self {
            Provider::Local => "local",
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
        }
    }

    fn parse(s: &str) -> Option<Provider> {
        [Provider::Local, Provider::OpenAi, Provider::Anthropic].into_iter().find(|p| p.name() == s)
    }

    /// The Keychain entry for this provider's API key.
    fn key_ref(self) -> String {
        format!("ai:{}", self.name())
    }
}

/// An OpenAI or Anthropic connection. An empty `base_url` means the
/// provider's own address.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Remote {
    pub base_url: String,
    pub model: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// None while AI is off.
    pub provider: Option<Provider>,
    pub local_model: String,
    pub openai: Remote,
    pub anthropic: Remote,
    /// Summarise long conversations as they open.
    pub auto_summary: bool,
    /// Sort new mail (needs reply, category). None means the default: on
    /// for the model on this Mac, off for a cloud provider, which would get
    /// every new message.
    pub sort_mail: Option<bool>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            provider: None,
            local_model: local::MODELS[0].id.to_string(),
            openai: Remote::default(),
            anthropic: Remote::default(),
            auto_summary: true,
            sort_mail: None,
        }
    }
}

impl Settings {
    pub fn sorts_new_mail(&self) -> bool {
        self.provider.is_some() && self.sort_mail.unwrap_or(self.provider == Some(Provider::Local))
    }

    /// What the summary card names as its author.
    pub fn label(&self) -> String {
        match self.provider {
            Some(Provider::Local) => local::model(&self.local_model).name.to_string(),
            Some(Provider::OpenAi) => short(&self.openai.model),
            Some(Provider::Anthropic) => short(&self.anthropic.model),
            None => String::new(),
        }
    }
}

/// A model id as people read it: compatible servers often name models by
/// their file path.
fn short(id: &str) -> String {
    let name = id.rsplit('/').next().unwrap_or(id);
    name.strip_suffix(".gguf").unwrap_or(name).to_string()
}

pub fn settings(conn: &Connection) -> Result<Settings> {
    let raw: Option<String> = conn.query_row("SELECT value FROM setting WHERE key = 'ai'", [], |r| r.get(0)).optional()?;
    let Some(v) = raw.and_then(|r| serde_json::from_str::<Value>(&r).ok()) else { return Ok(Settings::default()) };
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let remote = |v: &Value| Remote { base_url: text(&v["base_url"]), model: text(&v["model"]) };
    let defaults = Settings::default();
    Ok(Settings {
        provider: v["provider"].as_str().and_then(Provider::parse),
        local_model: v["local_model"].as_str().map_or(defaults.local_model, str::to_string),
        openai: remote(&v["openai"]),
        anthropic: remote(&v["anthropic"]),
        auto_summary: v["auto_summary"].as_bool().unwrap_or(defaults.auto_summary),
        sort_mail: v["sort_mail"].as_bool(),
    })
}

pub fn save_settings(conn: &Connection, s: &Settings) -> Result<()> {
    let remote = |r: &Remote| json!({"base_url": r.base_url.trim(), "model": r.model.trim()});
    let value = json!({
        "provider": s.provider.map(Provider::name),
        "local_model": s.local_model,
        "openai": remote(&s.openai),
        "anthropic": remote(&s.anthropic),
        "auto_summary": s.auto_summary,
        "sort_mail": s.sort_mail,
    });
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('ai', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [value.to_string()],
    )?;
    Ok(())
}

/// Saves a provider's API key in the Keychain; an empty key removes it.
pub fn set_key(provider: Provider, key: &str) -> Result<()> {
    match key.trim() {
        "" => secrets::delete(&provider.key_ref()),
        key => secrets::set_password(&provider.key_ref(), key),
    }
}

pub fn key(provider: Provider) -> Option<String> {
    secrets::password(&provider.key_ref()).ok().filter(|k| !k.is_empty())
}

/// One request to a model.
#[derive(Clone, Debug)]
pub struct Request {
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
    /// 0 for the same answer every time; only the local model uses it.
    pub temperature: f32,
    /// A GBNF grammar the local model's answer must follow.
    pub grammar: Option<String>,
}

/// Where a request is, for the "Reading 12 messages…" line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Loading the local model.
    Starting,
    /// The model is reading the prompt.
    Reading,
    /// Text is arriving.
    Writing,
}

pub enum Progress<'a> {
    Phase(Phase),
    Text(&'a str),
}

/// Stops a request from another thread.
#[derive(Clone, Default)]
pub struct Cancel {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        let notified = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// How a request ended without an error.
#[derive(Debug, PartialEq)]
pub enum Finish {
    Done(String),
    Cancelled,
}

/// Runs requests on the configured provider. Holds the local helper
/// process between requests.
pub struct Ai {
    pub local: local::Local,
    data_dir: std::path::PathBuf,
}

impl Ai {
    pub fn new(data_dir: &std::path::Path, helper: std::path::PathBuf) -> Ai {
        Ai { local: local::Local::new(helper), data_dir: data_dir.to_path_buf() }
    }

    /// How much conversation text a request may carry, in characters.
    pub fn budget(settings: &Settings) -> usize {
        match settings.provider {
            // About 8,000 tokens: under 7 s of reading on an M4 Pro.
            Some(Provider::Local) => 26_000,
            _ => 120_000,
        }
    }

    pub async fn generate(
        &self,
        settings: &Settings,
        request: &Request,
        cancel: &Cancel,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Finish> {
        match settings.provider {
            None => Err(crate::Error::Other("AI is off. Choose a model in Settings › AI.".into())),
            Some(Provider::Local) => {
                let model = local::model(&settings.local_model);
                let path = download::path(&self.data_dir, model);
                if !path.exists() {
                    return Err(crate::Error::Other(format!("{} isn't downloaded yet. Download it in Settings › AI.", model.name)));
                }
                self.local.generate(model, &path, request, cancel, on).await
            }
            Some(p @ (Provider::OpenAi | Provider::Anthropic)) => {
                let remote = if p == Provider::OpenAi { &settings.openai } else { &settings.anthropic };
                remote::generate(p, remote, key(p), request, cancel, on).await
            }
        }
    }
}
