//! OpenAI's Responses API and Anthropic's Messages API, streamed. Both also
//! serve OpenAI- or Anthropic-compatible servers (Ollama, LM Studio, a
//! company proxy) at another address.

use std::time::Duration;

use serde_json::{Value, json};

use super::{Cancel, Finish, Phase, Progress, Provider, Remote, Request};
use crate::{Error, Result};

pub const OPENAI_URL: &str = "https://api.openai.com/v1";
pub const ANTHROPIC_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

fn base(provider: Provider, base_url: &str) -> String {
    let url = base_url.trim().trim_end_matches('/');
    match (url.is_empty(), provider) {
        (true, Provider::Anthropic) => ANTHROPIC_URL.into(),
        (true, _) => OPENAI_URL.into(),
        (false, _) => url.into(),
    }
}

fn company(provider: Provider) -> &'static str {
    if provider == Provider::Anthropic { "Anthropic" } else { "OpenAI" }
}

fn client(timeout: Option<Duration>) -> Result<reqwest::Client> {
    crate::oauth::init_tls();
    let mut builder = reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).read_timeout(Duration::from_secs(90));
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    builder.build().map_err(|e| Error::Other(format!("HTTP client: {e}")))
}

fn authorise(provider: Provider, request: reqwest::RequestBuilder, key: Option<&str>) -> reqwest::RequestBuilder {
    match (provider, key) {
        (Provider::Anthropic, Some(key)) => {
            request.header("x-api-key", key).header("anthropic-version", ANTHROPIC_VERSION)
        }
        (Provider::Anthropic, None) => request.header("anthropic-version", ANTHROPIC_VERSION),
        (_, Some(key)) => request.bearer_auth(key),
        (_, None) => request,
    }
}

/// A plain-language reason for a refused request, from its status and body.
fn refusal(provider: Provider, status: reqwest::StatusCode, body: &str) -> Error {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().or(v["message"].as_str()).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect());
    let name = company(provider);
    let message = match status.as_u16() {
        401 | 403 => format!("{name} didn't accept the API key. Check it in Settings › AI. ({detail})"),
        404 => format!("{name} doesn't know this model or address. ({detail})"),
        429 if body.contains("insufficient_quota") || body.contains("credit") => {
            format!("Your {name} account has no credit left. ({detail})")
        }
        429 => format!("{name} is limiting requests right now. Try again in a minute. ({detail})"),
        500..=599 => format!("{name} had a problem answering ({status}). Try again in a minute."),
        _ => format!("{name} refused the request ({status}): {detail}"),
    };
    Error::Other(message)
}

fn unreachable(provider: Provider, e: reqwest::Error) -> Error {
    Error::Other(format!("Couldn't reach {}: {e}", company(provider)))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
}

/// The provider's models, newest first. Also checks that the address and
/// key work. On api.openai.com only the text models are listed.
pub async fn models(provider: Provider, base_url: &str, key: Option<String>) -> Result<Vec<ModelChoice>> {
    let base = base(provider, base_url);
    let url = match provider {
        Provider::Anthropic => format!("{base}/v1/models?limit=100"),
        _ => format!("{base}/models"),
    };
    let request = authorise(provider, client(Some(Duration::from_secs(20)))?.get(&url), key.as_deref());
    let response = request.send().await.map_err(|e| unreachable(provider, e))?;
    let status = response.status();
    let body = response.text().await.map_err(|e| unreachable(provider, e))?;
    if !status.is_success() {
        return Err(refusal(provider, status, &body));
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| Error::Other(format!("{url} didn't answer like {}'s API", company(provider))))?;
    let mut list: Vec<(i64, ModelChoice)> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let name = m["display_name"].as_str().unwrap_or(&id).to_string();
            let created = m["created"].as_i64().unwrap_or(0);
            Some((created, ModelChoice { id, name }))
        })
        .collect();
    if base == OPENAI_URL {
        list.retain(|(_, m)| openai_text_model(&m.id));
        list.sort_by(|a, b| b.0.cmp(&a.0));
    }
    Ok(list.into_iter().map(|(_, m)| m).collect())
}

/// OpenAI lists image, audio, embedding and moderation models too.
fn openai_text_model(id: &str) -> bool {
    let chat = id.starts_with("gpt-") || (id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit()));
    const OTHER: [&str; 9] = ["audio", "realtime", "tts", "transcribe", "image", "search", "embedding", "moderation", "codex"];
    chat && !OTHER.iter().any(|o| id.contains(o))
}

/// OpenAI's reasoning models think before writing unless told not to bother.
fn reasons(model: &str) -> bool {
    model.starts_with("gpt-5") || (model.starts_with('o') && model[1..].starts_with(|c: char| c.is_ascii_digit()))
}

pub async fn generate(
    provider: Provider,
    remote: &Remote,
    key: Option<String>,
    request: &Request,
    cancel: &Cancel,
    on: &mut (dyn FnMut(Progress) + Send),
) -> Result<Finish> {
    if remote.model.trim().is_empty() {
        return Err(Error::Other(format!("Choose a {} model in Settings › AI.", company(provider))));
    }
    if key.is_none() && remote.base_url.trim().is_empty() {
        return Err(Error::Other(format!("Add your {} API key in Settings › AI.", company(provider))));
    }
    let base = base(provider, &remote.base_url);
    let (url, body) = match provider {
        Provider::Anthropic => (
            format!("{base}/v1/messages"),
            json!({
                "model": remote.model.trim(),
                "max_tokens": request.max_tokens,
                "system": request.system,
                "messages": [{"role": "user", "content": request.user}],
                "stream": true,
            }),
        ),
        _ => {
            let mut body = json!({
                "model": remote.model.trim(),
                "instructions": request.system,
                "input": [{"role": "user", "content": request.user}],
                "stream": true,
                "store": false,
            });
            if reasons(remote.model.trim()) {
                body["reasoning"] = json!({"effort": "low"});
            }
            (format!("{base}/responses"), body)
        }
    };
    on(Progress::Phase(Phase::Reading));
    let send = authorise(provider, client(None)?.post(&url), key.as_deref())
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send();
    let mut response = tokio::select! {
        r = send => r.map_err(|e| unreachable(provider, e))?,
        _ = cancel.cancelled() => return Ok(Finish::Cancelled),
    };
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(refusal(provider, status, &body));
    }
    let mut events = Sse::default();
    let mut text = String::new();
    loop {
        let chunk = tokio::select! {
            c = response.chunk() => c.map_err(|e| Error::Other(format!("{} stopped answering: {e}", company(provider))))?,
            _ = cancel.cancelled() => return Ok(Finish::Cancelled),
        };
        let Some(chunk) = chunk else { break };
        for (event, data) in events.push(&chunk) {
            let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
            let kind = v["type"].as_str().unwrap_or(&event).to_string();
            let delta = match kind.as_str() {
                "response.output_text.delta" => v["delta"].as_str(),
                "content_block_delta" if v["delta"]["type"] == "text_delta" => v["delta"]["text"].as_str(),
                "response.completed" | "message_stop" => return Ok(Finish::Done(text)),
                "response.failed" | "response.error" | "error" => {
                    let message = [&v["response"]["error"]["message"], &v["error"]["message"], &v["message"]]
                        .into_iter()
                        .find_map(|m| m.as_str())
                        .unwrap_or("unknown error");
                    return Err(Error::Other(format!("{} stopped: {message}", company(provider))));
                }
                _ => None,
            };
            if let Some(delta) = delta {
                if text.is_empty() {
                    on(Progress::Phase(Phase::Writing));
                }
                text.push_str(delta);
                on(Progress::Text(&text));
            }
        }
    }
    // Some compatible servers close the stream without a final event.
    if text.is_empty() {
        return Err(Error::Other(format!("{} sent an empty answer.", company(provider))));
    }
    Ok(Finish::Done(text))
}

/// Server-sent events: `event:` and `data:` lines, blank-line separated.
#[derive(Default)]
struct Sse {
    buffer: Vec<u8>,
}

impl Sse {
    fn push(&mut self, bytes: &[u8]) -> Vec<(String, String)> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let find = |sep: &[u8]| self.buffer.windows(sep.len()).position(|w| w == sep).map(|i| (i, sep.len()));
            let Some((end, skip)) = [find(b"\r\n\r\n"), find(b"\n\n")].into_iter().flatten().min() else { break };
            let block = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
            self.buffer.drain(..end + skip);
            let (mut event, mut data) = (String::new(), Vec::new());
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
                }
            }
            if !data.is_empty() && data != ["[DONE]"] {
                out.push((event, data.join("\n")));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_across_chunks() {
        let mut sse = Sse::default();
        assert!(sse.push(b"event: content_block_delta\ndata: {\"a\":").is_empty());
        let out = sse.push("1}\n\nevent: ping\r\ndata: {}\r\n\r\ndata: é".as_bytes());
        assert_eq!(out, vec![("content_block_delta".into(), "{\"a\":1}".into()), ("ping".into(), "{}".into())]);
        assert_eq!(sse.push(b"\n\n"), vec![(String::new(), "é".into())]);
    }

    #[test]
    fn openai_listing_keeps_text_models() {
        for id in ["gpt-5-mini", "gpt-4.1", "o3", "o4-mini"] {
            assert!(openai_text_model(id), "{id}");
        }
        for id in ["gpt-image-1", "text-embedding-3-small", "gpt-4o-realtime-preview", "omni-moderation-latest", "dall-e-3", "whisper-1"] {
            assert!(!openai_text_model(id), "{id}");
        }
    }
}
