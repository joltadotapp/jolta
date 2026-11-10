//! AI settings, model downloads, summaries and drafts for the app.

use std::sync::{Arc, Mutex};

use jolta_core::ai::{self, Cancel, Finish, Phase, Progress, Provider, download, local, tasks};
use jolta_core::compose;
use jolta_core::{AccountId, ThreadId};

use crate::{ComposeData, Core, CoreError, CoreEvent, Result};

#[derive(uniffi::Enum, Clone, Copy, PartialEq, Eq)]
pub enum AiProvider {
    Local,
    OpenAi,
    Anthropic,
}

impl From<AiProvider> for Provider {
    fn from(p: AiProvider) -> Self {
        match p {
            AiProvider::Local => Provider::Local,
            AiProvider::OpenAi => Provider::OpenAi,
            AiProvider::Anthropic => Provider::Anthropic,
        }
    }
}

impl From<Provider> for AiProvider {
    fn from(p: Provider) -> Self {
        match p {
            Provider::Local => AiProvider::Local,
            Provider::OpenAi => AiProvider::OpenAi,
            Provider::Anthropic => AiProvider::Anthropic,
        }
    }
}

/// The AI settings. Keys stay in the Keychain; only whether one is saved
/// crosses the bridge.
#[derive(uniffi::Record, Clone)]
pub struct AiSettingsInfo {
    pub provider: Option<AiProvider>,
    pub local_model: String,
    pub openai_url: String,
    pub openai_model: String,
    pub openai_key_saved: bool,
    pub anthropic_url: String,
    pub anthropic_model: String,
    pub anthropic_key_saved: bool,
    pub auto_summary: bool,
    /// Sort new mail: None follows the default (on for this Mac, off for a
    /// cloud provider). `sorts_mail` is the result.
    pub sort_mail: Option<bool>,
    pub sorts_mail: bool,
    /// What summaries name as their author, such as "Gemma 4 E2B".
    pub label: String,
}

#[derive(uniffi::Record)]
pub struct AiModelInfo {
    pub id: String,
    pub name: String,
}

#[derive(uniffi::Enum, Clone)]
pub enum ModelState {
    NotDownloaded,
    /// Stopped partway; downloading again carries on.
    Paused { done: u64 },
    Downloading { done: u64, bytes_per_second: f64 },
    Verifying,
    Ready,
    Failed { message: String },
}

#[derive(uniffi::Record)]
pub struct LocalModelInfo {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub size: u64,
    pub memory: String,
    pub state: ModelState,
}

#[derive(uniffi::Enum, Clone, Copy)]
pub enum AiPhase {
    Starting,
    Reading,
    Writing,
}

#[derive(uniffi::Enum)]
pub enum AiOutcome {
    Done { text: String, model: String },
    /// Short enough to read; no summary was made.
    NotNeeded,
    NotSetUp,
    Cancelled,
    Failed { message: String },
}

#[derive(uniffi::Record)]
pub struct SummaryInfo {
    pub text: String,
    pub model: String,
    /// False when messages arrived after it was written.
    pub current: bool,
}

/// Receives one request's progress. `on_text` gets all the text so far.
#[uniffi::export(with_foreign)]
pub trait AiListener: Send + Sync {
    fn on_phase(&self, phase: AiPhase);
    fn on_text(&self, text: String);
    fn on_finish(&self, outcome: AiOutcome);
}

/// A running request; `cancel` stops it.
#[derive(uniffi::Object)]
pub struct AiTask {
    cancel: Cancel,
}

#[uniffi::export]
impl AiTask {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

fn progress(listener: &Arc<dyn AiListener>) -> impl FnMut(Progress) + Send + use<> {
    let listener = listener.clone();
    move |p| match p {
        Progress::Phase(phase) => listener.on_phase(match phase {
            Phase::Starting => AiPhase::Starting,
            Phase::Reading => AiPhase::Reading,
            Phase::Writing => AiPhase::Writing,
        }),
        Progress::Text(text) => listener.on_text(text.to_string()),
    }
}

impl Core {
    fn model_state(&self, model: &local::LocalModel) -> ModelState {
        if let Some(state) = self.downloads.lock().unwrap_or_else(|e| e.into_inner()).get(model.id) {
            return state.1.clone();
        }
        match download::state(&self.jolta.data_dir, model) {
            download::State::Missing => ModelState::NotDownloaded,
            download::State::Partial(done) => ModelState::Paused { done },
            download::State::Ready => ModelState::Ready,
        }
    }

    fn model_event(&self, id: &str, state: ModelState) {
        self.listener.on_event(CoreEvent::ModelChanged { id: id.to_string(), state });
    }
}

#[uniffi::export]
impl Core {
    pub fn ai_settings(&self) -> Result<AiSettingsInfo> {
        let s = self.jolta.store.read(ai::settings)?;
        Ok(AiSettingsInfo {
            provider: s.provider.map(Into::into),
            local_model: s.local_model.clone(),
            openai_url: s.openai.base_url.clone(),
            openai_model: s.openai.model.clone(),
            openai_key_saved: ai::key(Provider::OpenAi).is_some(),
            anthropic_url: s.anthropic.base_url.clone(),
            anthropic_model: s.anthropic.model.clone(),
            anthropic_key_saved: ai::key(Provider::Anthropic).is_some(),
            auto_summary: s.auto_summary,
            sort_mail: s.sort_mail,
            sorts_mail: s.sorts_new_mail(),
            label: s.label(),
        })
    }

    pub fn set_ai_settings(&self, settings: AiSettingsInfo) -> Result<()> {
        let s = ai::Settings {
            provider: settings.provider.map(Into::into),
            local_model: settings.local_model,
            openai: ai::Remote { base_url: settings.openai_url, model: settings.openai_model },
            anthropic: ai::Remote { base_url: settings.anthropic_url, model: settings.anthropic_model },
            auto_summary: settings.auto_summary,
            sort_mail: settings.sort_mail,
        };
        self.jolta.store.write(|tx| ai::save_settings(tx, &s))?;
        // Switching sorting on (or to a model that sorts) catches up on two weeks of inbox.
        self.background.catch_up(&self.jolta);
        // Sorting decides part of what is important, and so the badge.
        self.changed();
        Ok(())
    }

    /// Whether background AI may use the local model now. The app says no
    /// on battery, in Low Power Mode or when the Mac runs hot.
    pub fn set_ai_background_allowed(&self, allowed: bool) {
        self.background.set_allowed(allowed);
    }

    /// A search query in Jolta's language for a plain-language request,
    /// already checked by the parser. None when AI is off.
    pub async fn ai_search_query(&self, request: String, utc_offset: i32) -> Result<Option<String>> {
        let (jolta, ai) = (self.jolta.clone(), self.ai.clone());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        Ok(self
            .runtime
            .spawn(async move { jolta_core::ai::search::query(&jolta, &ai, &request, now, utc_offset, &Cancel::default()).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??)
    }

    /// Background jobs waiting: messages to sort and summaries to make.
    pub fn ai_background_pending(&self) -> u32 {
        self.background.pending() as u32
    }

    /// Saves an API key in the Keychain; an empty one removes it.
    pub fn set_ai_key(&self, provider: AiProvider, key: String) -> Result<()> {
        Ok(ai::set_key(provider.into(), &key)?)
    }

    /// The provider's models, which also proves the address and key work.
    /// Without `key`, the saved one is used.
    pub async fn ai_models(&self, provider: AiProvider, base_url: String, key: Option<String>) -> Result<Vec<AiModelInfo>> {
        let provider: Provider = provider.into();
        let key = key.filter(|k| !k.trim().is_empty()).map(|k| k.trim().to_string()).or_else(|| ai::key(provider));
        let list = self
            .runtime
            .spawn(async move { ai::models(provider, &base_url, key).await })
            .await
            .map_err(|e| CoreError::Failed { message: e.to_string() })??;
        Ok(list.into_iter().map(|m| AiModelInfo { id: m.id, name: m.name }).collect())
    }

    /// The default addresses, for the advanced fields' placeholders.
    pub fn ai_default_url(&self, provider: AiProvider) -> String {
        match provider {
            AiProvider::OpenAi => ai::OPENAI_URL.into(),
            AiProvider::Anthropic => ai::ANTHROPIC_URL.into(),
            AiProvider::Local => String::new(),
        }
    }

    pub fn local_models(&self) -> Vec<LocalModelInfo> {
        local::MODELS
            .iter()
            .map(|m| LocalModelInfo {
                id: m.id.into(),
                name: m.name.into(),
                detail: m.detail.into(),
                size: m.size,
                memory: m.memory.into(),
                state: self.model_state(m),
            })
            .collect()
    }

    /// Downloads a local model, carrying on from where a paused one
    /// stopped. Progress arrives as `ModelChanged` events.
    pub fn download_model(self: Arc<Self>, id: String) {
        let model = local::model(&id);
        let cancel = Cancel::default();
        {
            let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
            if downloads.contains_key(model.id) {
                return;
            }
            let done = match download::state(&self.jolta.data_dir, model) {
                download::State::Partial(done) => done,
                _ => 0,
            };
            downloads.insert(model.id.to_string(), (cancel.clone(), ModelState::Downloading { done, bytes_per_second: 0.0 }));
        }
        self.model_event(model.id, self.model_state(model));
        let core = self.clone();
        self.runtime.spawn(async move {
            let data_dir = core.jolta.data_dir.clone();
            let reporter = core.clone();
            let mut on = move |step: download::Step| {
                let state = match step {
                    download::Step::Downloading { done, bytes_per_second, .. } => ModelState::Downloading { done, bytes_per_second },
                    download::Step::Verifying => ModelState::Verifying,
                };
                if let Some(entry) = reporter.downloads.lock().unwrap_or_else(|e| e.into_inner()).get_mut(model.id) {
                    entry.1 = state.clone();
                }
                reporter.model_event(model.id, state);
            };
            let result = download::download(&data_dir, model, &cancel, &mut on).await;
            core.downloads.lock().unwrap_or_else(|e| e.into_inner()).remove(model.id);
            let state = match result {
                Err(e) => ModelState::Failed { message: e.to_string() },
                Ok(_) => core.model_state(model),
            };
            core.model_event(model.id, state);
        });
    }

    /// Stops a download, keeping what arrived.
    pub fn pause_download(&self, id: String) {
        if let Some((cancel, _)) = self.downloads.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            cancel.cancel();
        }
    }

    /// Deletes a downloaded or partial model.
    pub fn remove_model(&self, id: String) -> Result<()> {
        self.pause_download(id.clone());
        let model = local::model(&id);
        download::remove(&self.jolta.data_dir, model)?;
        self.model_event(model.id, ModelState::NotDownloaded);
        Ok(())
    }

    /// The saved summary of a conversation, to show at once while a fresh
    /// one is made.
    pub fn cached_summary(&self, thread_id: i64) -> Result<Option<SummaryInfo>> {
        let thread = ThreadId(thread_id);
        Ok(self.jolta.store.read(|c| {
            let Some(s) = tasks::cached_summary(c, thread)? else { return Ok(None) };
            let newest = tasks::newest_message(c, thread)?;
            Ok(Some(SummaryInfo { current: newest == Some(s.last_message), text: s.text, model: s.model }))
        })?)
    }

    /// Summarises a conversation. `automatic` skips short ones and respects
    /// the automatic-summary setting; `fresh` ignores a saved summary.
    pub fn summarize(&self, thread_id: i64, automatic: bool, fresh: bool, listener: Arc<dyn AiListener>) -> Arc<AiTask> {
        let cancel = Cancel::default();
        let (jolta, ai, stop) = (self.jolta.clone(), self.ai.clone(), cancel.clone());
        self.runtime.spawn(async move {
            let mut on = progress(&listener);
            let outcome = match tasks::summarise(&jolta, &ai, ThreadId(thread_id), automatic, fresh, &stop, &mut on).await {
                Ok(tasks::SummaryOutcome::Done(s)) => AiOutcome::Done { text: s.text, model: s.model },
                Ok(tasks::SummaryOutcome::NotNeeded) => AiOutcome::NotNeeded,
                Ok(tasks::SummaryOutcome::NotSetUp) => AiOutcome::NotSetUp,
                Ok(tasks::SummaryOutcome::Cancelled) => AiOutcome::Cancelled,
                Err(e) => AiOutcome::Failed { message: e.to_string() },
            };
            listener.on_finish(outcome);
        });
        Arc::new(AiTask { cancel })
    }

    /// Writes the message, or the reply, from an instruction. The text
    /// arrives in the body format; put it in place with `with_written`.
    pub fn draft_message(&self, compose: ComposeData, instruction: String, listener: Arc<dyn AiListener>) -> Arc<AiTask> {
        let cancel = Cancel::default();
        let (jolta, ai, stop) = (self.jolta.clone(), self.ai.clone(), cancel.clone());
        self.runtime.spawn(async move {
            let mut on = progress(&listener);
            let label = jolta.store.read(ai::settings).map(|s| s.label()).unwrap_or_default();
            let outcome = match tasks::draft(&jolta, &ai, &compose.into(), &instruction, &stop, &mut on).await {
                Ok(Some(Finish::Done(text))) => AiOutcome::Done { text, model: label },
                Ok(Some(Finish::Cancelled)) => AiOutcome::Cancelled,
                Ok(None) => AiOutcome::NotSetUp,
                Err(e) => AiOutcome::Failed { message: e.to_string() },
            };
            listener.on_finish(outcome);
        });
        Arc::new(AiTask { cancel })
    }

    /// The body with what the writer typed swapped for `text`, keeping the
    /// sender's signature and the quote.
    pub fn with_written(&self, body: String, account_id: Option<i64>, text: String) -> String {
        let signature = self.account_signature(account_id);
        compose::with_written(&body, &signature, &text)
    }

    /// What the writer typed so far, without signature and quote.
    pub fn written(&self, body: String, account_id: Option<i64>) -> String {
        compose::written(&body, &self.account_signature(account_id))
    }
}

impl Core {
    fn account_signature(&self, account_id: Option<i64>) -> String {
        account_id
            .and_then(|a| self.jolta.store.read(|c| jolta_core::store::account(c, AccountId(a))).ok())
            .map(|a| a.signature)
            .unwrap_or_default()
    }
}

/// The helper next to the app's executable (`Jolta.app/Contents/MacOS`).
pub(crate) fn helper_path() -> std::path::PathBuf {
    std::env::var_os("JOLTA_AI_HELPER").map(Into::into).unwrap_or_else(|| {
        std::env::current_exe().unwrap_or_default().with_file_name("jolta-ai")
    })
}

pub(crate) type Downloads = Arc<Mutex<std::collections::HashMap<String, (Cancel, ModelState)>>>;
