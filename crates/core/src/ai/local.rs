//! Models that run on this Mac, through the `jolta-ai` helper process. The
//! helper starts on the first request and quits after `IDLE` without one,
//! which gives its 2 GB or so of memory back.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::{Cancel, Finish, Phase, Progress, Request};
use crate::{Error, Result};

/// A model Jolta offers to download.
#[derive(Debug, PartialEq)]
pub struct LocalModel {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    /// Memory in use while it runs, for the settings screen.
    pub memory: &'static str,
}

/// Sizes and SHA-256 hashes of the files on Hugging Face.
pub const MODELS: &[LocalModel] = &[
    LocalModel {
        id: "gemma-4-e2b",
        name: "Gemma 4 E2B",
        detail: "Fast, and good at multiple languages.",
        file: "gemma-4-E2B-it-Q4_0.gguf",
        url: "https://huggingface.co/ggml-org/gemma-4-E2B-it-GGUF/resolve/main/gemma-4-E2B-it-Q4_0.gguf",
        size: 2_841_481_184,
        sha256: "8e30dff3ac4c8434c49a7036fa15564bdbb6044e42bf04550bf1a096ad7e6a52",
        memory: "2.1 GB",
    },
    LocalModel {
        id: "gemma-4-e4b",
        name: "Gemma 4 E4B",
        detail: "A little sharper, half as fast.",
        file: "gemma-4-E4B_q4_0-it.gguf",
        url: "https://huggingface.co/google/gemma-4-E4B-it-qat-q4_0-gguf/resolve/main/gemma-4-E4B_q4_0-it.gguf",
        size: 5_154_941_280,
        sha256: "676c35070db6dbe52f93e9c864ee0fba4eddea94b9c875d9cb10daff453fbaee",
        memory: "3.8 GB",
    },
];

/// The catalog entry for `id`, or the default model.
pub fn model(id: &str) -> &'static LocalModel {
    MODELS.iter().find(|m| m.id == id).unwrap_or(&MODELS[0])
}

/// Gemma 4's chat format, with thinking off (no `<|think|>` in the system turn).
fn prompt(request: &Request) -> String {
    format!(
        "<|turn>system\n{}<turn|>\n<|turn>user\n{}<turn|>\n<|turn>model\n",
        request.system.trim(),
        request.user.trim()
    )
}

const CONTEXT: u32 = 16_384;
const IDLE: Duration = Duration::from_secs(5 * 60);

struct Helper {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    model: Option<PathBuf>,
}

impl Helper {
    async fn send(&mut self, v: Value) -> Result<()> {
        self.stdin.write_all(format!("{v}\n").as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn next(&mut self) -> Result<Value> {
        let line = self.lines.next_line().await?.ok_or_else(|| Error::Other("The local model stopped unexpectedly.".into()))?;
        serde_json::from_str(&line).map_err(|_| Error::Other(format!("the local model said: {line}")))
    }
}

pub struct Local {
    path: PathBuf,
    helper: Arc<tokio::sync::Mutex<Option<Helper>>>,
    last_used: Arc<Mutex<Instant>>,
    reaper: Mutex<bool>,
}

impl Local {
    pub fn new(path: PathBuf) -> Local {
        Local {
            path,
            helper: Arc::default(),
            last_used: Arc::new(Mutex::new(Instant::now())),
            reaper: Mutex::new(false),
        }
    }

    fn spawn(&self) -> Result<Helper> {
        let mut child = Command::new(&self.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Other(format!("The local model's helper ({}) didn't start: {e}", self.path.display())))?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Other("no stdin for jolta-ai".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Other("no stdout for jolta-ai".into()))?;
        Ok(Helper { child, stdin, lines: BufReader::new(stdout).lines(), model: None })
    }

    /// Quits the helper once it has sat unused for `IDLE`.
    fn start_reaper(&self) {
        let mut started = self.reaper.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        *started = true;
        let (helper, last_used) = (self.helper.clone(), self.last_used.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let idle = last_used.lock().unwrap_or_else(|e| e.into_inner()).elapsed() >= IDLE;
                if idle {
                    if let Ok(mut slot) = helper.try_lock() {
                        if let Some(mut h) = slot.take() {
                            let _ = h.child.kill().await;
                        }
                    }
                }
            }
        });
    }

    /// Whether the helper has a model in memory right now.
    pub fn is_running(&self) -> bool {
        self.helper.try_lock().map(|h| h.is_some()).unwrap_or(true)
    }

    pub async fn generate(
        &self,
        model: &LocalModel,
        path: &Path,
        request: &Request,
        cancel: &Cancel,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Finish> {
        self.start_reaper();
        let mut slot = self.helper.lock().await;
        let result = self.run(&mut slot, model, path, request, cancel, on).await;
        *self.last_used.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        if result.is_err() {
            // A helper that failed mid-request may be in any state; start fresh next time.
            if let Some(mut h) = slot.take() {
                let _ = h.child.kill().await;
            }
        }
        result
    }

    async fn run(
        &self,
        slot: &mut Option<Helper>,
        model: &LocalModel,
        path: &Path,
        request: &Request,
        cancel: &Cancel,
        on: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<Finish> {
        if slot.as_ref().is_none_or(|h| h.model.as_deref() != Some(path)) {
            on(Progress::Phase(Phase::Starting));
            if slot.is_none() {
                *slot = Some(self.spawn()?);
            }
            let h = slot.as_mut().expect("just started");
            h.send(json!({"op": "load", "model": path, "ctx": CONTEXT})).await?;
            let answer = h.next().await?;
            if answer["event"] != "loaded" {
                return Err(Error::Other(format!(
                    "{} didn't load: {}",
                    model.name,
                    answer["message"].as_str().unwrap_or("unknown error")
                )));
            }
            h.model = Some(path.to_path_buf());
        }
        let h = slot.as_mut().expect("loaded above");
        h.send(json!({"op": "generate", "prompt": prompt(request), "max_tokens": request.max_tokens,
                      "temperature": request.temperature, "grammar": request.grammar}))
            .await?;
        let mut text = String::new();
        let mut cancel_sent = false;
        loop {
            let event = tokio::select! {
                e = h.next() => e?,
                _ = cancel.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    h.send(json!({"op": "cancel"})).await?;
                    continue;
                }
            };
            match event["event"].as_str() {
                Some("reading") => on(Progress::Phase(Phase::Reading)),
                Some("text") => {
                    if text.is_empty() {
                        on(Progress::Phase(Phase::Writing));
                    }
                    text.push_str(event["text"].as_str().unwrap_or_default());
                    on(Progress::Text(&text));
                }
                Some("done") if event["cancelled"] == true => return Ok(Finish::Cancelled),
                Some("done") => return Ok(Finish::Done(text)),
                _ => {
                    return Err(Error::Other(event["message"].as_str().unwrap_or("the local model failed").to_string()));
                }
            }
        }
    }
}
