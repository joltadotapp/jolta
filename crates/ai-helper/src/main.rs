//! `jolta-ai`: runs a local language model for Jolta, one request at a time.
//! It lives in its own process so a llama.cpp abort can't take the app down,
//! and so quitting it gives the model's memory back.
//!
//! One JSON object per line on stdin:
//!   {"op":"load","model":"/path/model.gguf","ctx":16384}
//!   {"op":"generate","prompt":"…","max_tokens":600,"temperature":0,"grammar":null}
//!   {"op":"cancel"}
//! and one per line on stdout:
//!   {"event":"loaded","seconds":0.5}
//!   {"event":"reading","tokens":5210}
//!   {"event":"text","text":"…"}
//!   {"event":"done","prompt_tokens":5210,"tokens":180,"seconds":4.1,"cancelled":false}
//!   {"event":"error","message":"…"}
//! The prompt arrives already in the model's chat format. EOF on stdin quits.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use serde_json::{Value, json};

fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn fail(message: impl std::fmt::Display) {
    emit(json!({"event": "error", "message": message.to_string()}));
}

fn main() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Value>();
    // Reads stdin on its own thread so a cancel arrives mid-generation.
    let flag = cancel.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(request) = serde_json::from_str::<Value>(&line) else {
                fail(format!("not a request: {line}"));
                continue;
            };
            if request["op"] == "cancel" {
                flag.store(true, Ordering::SeqCst);
            } else if tx.send(request).is_err() {
                break;
            }
        }
    });
    engine::run(rx, cancel);
}

#[cfg(target_arch = "aarch64")]
mod engine {
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Instant;

    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::LlamaModel;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::sampling::LlamaSampler;
    use serde_json::{Value, json};

    use super::{emit, fail};

    const N_BATCH: usize = 2048;

    struct Loaded {
        path: String,
        ctx: u32,
        model: LlamaModel,
    }

    pub fn run(rx: mpsc::Receiver<Value>, cancel: Arc<AtomicBool>) {
        let Ok(mut backend) = LlamaBackend::init() else { return fail("llama.cpp could not start") };
        backend.void_logs();
        let mut loaded: Option<Loaded> = None;
        for request in rx {
            match request["op"].as_str() {
                Some("load") => {
                    let path = request["model"].as_str().unwrap_or_default().to_string();
                    let ctx = request["ctx"].as_u64().unwrap_or(16_384) as u32;
                    if loaded.as_ref().is_some_and(|l| l.path == path && l.ctx == ctx) {
                        emit(json!({"event": "loaded", "seconds": 0.0}));
                        continue;
                    }
                    loaded = None;
                    let started = Instant::now();
                    // Lazy mode 2 reads Gemma 4's per-layer embeddings from disk
                    // row by row instead of loading 1.3 GB of them.
                    let params = LlamaModelParams::default().with_n_gpu_layers(999).with_lazy_mode(2);
                    match LlamaModel::load_from_file(&backend, &path, &params) {
                        Ok(model) => {
                            loaded = Some(Loaded { path, ctx, model });
                            emit(json!({"event": "loaded", "seconds": started.elapsed().as_secs_f64()}));
                        }
                        Err(e) => fail(format!("the model could not be loaded: {e}")),
                    }
                }
                Some("generate") => {
                    cancel.store(false, Ordering::SeqCst);
                    match &loaded {
                        Some(l) => {
                            if let Err(e) = generate(&backend, l, &request, &cancel) {
                                fail(e);
                            }
                        }
                        None => fail("no model is loaded"),
                    }
                }
                _ => fail(format!("unknown request: {request}")),
            }
        }
    }

    fn generate(backend: &LlamaBackend, l: &Loaded, request: &Value, cancel: &AtomicBool) -> Result<(), String> {
        let started = Instant::now();
        let prompt = request["prompt"].as_str().unwrap_or_default();
        let max_tokens = request["max_tokens"].as_u64().unwrap_or(600) as usize;
        let temperature = request["temperature"].as_f64().unwrap_or(0.0) as f32;
        let vocab = l.model.vocab();
        let tokens = vocab.tokenize(prompt.as_bytes(), true, true);
        if tokens.len() + max_tokens > l.ctx as usize {
            return Err(format!("the request is too long for the model ({} tokens, {} fit)", tokens.len(), l.ctx as usize - max_tokens));
        }
        emit(json!({"event": "reading", "tokens": tokens.len()}));
        let params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(l.ctx))
            .with_n_batch(N_BATCH as u32)
            .with_n_ubatch(512);
        let mut ctx = l.model.new_context(backend, params).map_err(|e| format!("no room for the model's context: {e}"))?;
        let mut batch = LlamaBatch::new(N_BATCH, 1);
        let mut pos = 0i32;
        for chunk in tokens.chunks(N_BATCH) {
            if cancel.load(Ordering::SeqCst) {
                emit(json!({"event": "done", "prompt_tokens": tokens.len(), "tokens": 0, "seconds": started.elapsed().as_secs_f64(), "cancelled": true}));
                return Ok(());
            }
            batch.clear();
            for (i, t) in chunk.iter().enumerate() {
                let last = pos as usize + i == tokens.len() - 1;
                batch.add(*t, pos + i as i32, &[0], last).map_err(|e| e.to_string())?;
            }
            ctx.decode(&mut batch).map_err(|e| format!("reading the prompt failed: {e}"))?;
            pos += chunk.len() as i32;
        }
        let grammar = match request["grammar"].as_str() {
            Some(g) => Some(LlamaSampler::grammar(&l.model, g, "root").map_err(|e| format!("bad grammar: {e}"))?),
            None => None,
        };
        let mut sampler = if let Some(grammar) = grammar {
            LlamaSampler::chain_simple([grammar, LlamaSampler::greedy()])
        } else if temperature <= 0.0 {
            LlamaSampler::greedy()
        } else {
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
            LlamaSampler::chain_simple([
                LlamaSampler::top_k(40),
                LlamaSampler::top_p(0.95, 1),
                LlamaSampler::temp(temperature),
                LlamaSampler::dist(seed),
            ])
        };
        let mut pending = Vec::new();
        let mut produced = 0;
        let mut index = batch.n_tokens() - 1;
        let mut cancelled = false;
        for _ in 0..max_tokens {
            if cancel.load(Ordering::SeqCst) {
                cancelled = true;
                break;
            }
            // `sample` also accepts the token into the sampler chain.
            let token = sampler.sample(&ctx, index);
            if vocab.is_eog(token) {
                break;
            }
            produced += 1;
            pending.extend(vocab.token_to_piece(token, false, None));
            let text = take_utf8(&mut pending);
            if !text.is_empty() {
                emit(json!({"event": "text", "text": text}));
            }
            batch.clear();
            batch.add(token, pos, &[0], true).map_err(|e| e.to_string())?;
            pos += 1;
            ctx.decode(&mut batch).map_err(|e| format!("writing failed: {e}"))?;
            index = 0;
        }
        if !pending.is_empty() {
            emit(json!({"event": "text", "text": String::from_utf8_lossy(&pending)}));
        }
        emit(json!({"event": "done", "prompt_tokens": tokens.len(), "tokens": produced,
                    "seconds": started.elapsed().as_secs_f64(), "cancelled": cancelled}));
        Ok(())
    }

    /// The complete characters at the front of `bytes`; a character split
    /// across two tokens waits for the rest of it.
    fn take_utf8(bytes: &mut Vec<u8>) -> String {
        let valid = match std::str::from_utf8(bytes) {
            Ok(s) => s.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => {
                let text = String::from_utf8_lossy(bytes).into_owned();
                bytes.clear();
                return text;
            }
        };
        let rest = bytes.split_off(valid);
        let text = String::from_utf8(std::mem::replace(bytes, rest)).unwrap_or_default();
        text
    }
}

#[cfg(not(target_arch = "aarch64"))]
mod engine {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, mpsc};

    use serde_json::Value;

    pub fn run(rx: mpsc::Receiver<Value>, _cancel: Arc<AtomicBool>) {
        for _ in rx {
            super::fail("local models need a Mac with Apple silicon");
        }
    }
}
