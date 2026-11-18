//! AI settings and draft output through the app bridge.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jolta_ffi::ai::{AiListener, AiOutcome, AiPhase, AiProvider};

use crate::fixture::{App, block_on};

/// API keys live in one process-wide store, so these tests take turns.
static KEYS: Mutex<()> = Mutex::new(());

/// An HTTP server on loopback that keeps each request's headers.
struct Listener {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Listener {
    fn start() -> Listener {
        Self::replying("application/json", r#"{"data":[]}"#)
    }

    fn replying(content_type: &'static str, body: &'static str) -> Listener {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                record.lock().unwrap().push(String::from_utf8_lossy(&request).into_owned());
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Listener { url, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn use_address(app: &App, provider: AiProvider, url: &str) {
    let mut s = app.core.ai_settings().unwrap();
    s.provider = Some(provider);
    match provider {
        AiProvider::OpenAi => (s.openai_url, s.openai_model) = (url.into(), "m".into()),
        _ => (s.anthropic_url, s.anthropic_model) = (url.into(), "m".into()),
    }
    app.core.set_ai_settings(s).unwrap();
}

fn key_saved(app: &App, provider: AiProvider) -> bool {
    let s = app.core.ai_settings().unwrap();
    if provider == AiProvider::OpenAi { s.openai_key_saved } else { s.anthropic_key_saved }
}

#[test]
fn a_key_saved_for_the_default_address_never_reaches_a_later_http_address() {
    let _turn = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    for provider in [AiProvider::OpenAi, AiProvider::Anthropic] {
        let app = App::new();
        app.core.set_ai_key(provider, "sk-DUMMY-saved-for-default".into()).unwrap();
        let server = Listener::start();
        use_address(&app, provider, &server.url);

        // What the settings page does right after the address changes.
        let _ = block_on(app.core.ai_models(provider, server.url.clone(), None));
        // A summary, draft or search request on the new address.
        let _ = block_on(app.core.ai_search_query("mail from last week".into(), 0));

        let requests = server.requests();
        assert!(requests.len() >= 2, "{}: the server got the requests: {requests:?}", name(provider));
        for request in &requests {
            assert!(!request.contains("DUMMY"), "{}: the saved key went over http:\n{request}", name(provider));
        }
        app.core.set_ai_key(provider, String::new()).ok();
    }
}

#[test]
fn a_key_saved_for_the_default_address_is_not_offered_to_another_host() {
    let _turn = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    let app = App::new();
    app.core.set_ai_key(AiProvider::OpenAi, "sk-DUMMY-saved-for-default".into()).unwrap();
    assert!(key_saved(&app, AiProvider::OpenAi));

    use_address(&app, AiProvider::OpenAi, "https://proxy.example.com/v1");
    assert!(!key_saved(&app, AiProvider::OpenAi), "the settings page must ask for a key for the new address");

    // A key saved before keys were tied to an address still works on the default one.
    use_address(&app, AiProvider::OpenAi, "https://api.openai.com/v1");
    assert!(key_saved(&app, AiProvider::OpenAi));
    use_address(&app, AiProvider::OpenAi, "");
    assert!(key_saved(&app, AiProvider::OpenAi));
    app.core.set_ai_key(AiProvider::OpenAi, String::new()).unwrap();
}

#[test]
fn a_key_typed_for_an_http_address_is_neither_sent_nor_saved() {
    let _turn = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    let app = App::new();
    let server = Listener::start();
    use_address(&app, AiProvider::OpenAi, &server.url);

    assert!(block_on(app.core.ai_models(AiProvider::OpenAi, server.url.clone(), Some("sk-DUMMY-typed".into()))).is_err());
    assert!(app.core.set_ai_key(AiProvider::OpenAi, "sk-DUMMY-typed".into()).is_err());
    assert!(!key_saved(&app, AiProvider::OpenAi));
    for request in server.requests() {
        assert!(!request.contains("DUMMY"), "the typed key went over http:\n{request}");
    }
}

fn name(provider: AiProvider) -> &'static str {
    if provider == AiProvider::OpenAi { "OpenAI" } else { "Anthropic" }
}

#[test]
fn a_draft_preview_keeps_plain_text_and_leading_whitespace() {
    struct DraftListener {
        updates: Mutex<Vec<String>>,
        result: std::sync::mpsc::Sender<AiOutcome>,
    }

    impl AiListener for DraftListener {
        fn on_phase(&self, _: AiPhase) {}

        fn on_text(&self, text: String) {
            self.updates.lock().unwrap().push(text);
        }

        fn on_finish(&self, outcome: AiOutcome) {
            self.result.send(outcome).unwrap();
        }
    }

    let server = Listener::replying(
        "text/event-stream",
        concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"  Hi *all*,\\n\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"see you at [time].\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        ),
    );
    let app = App::new();
    use_address(&app, AiProvider::OpenAi, &server.url);
    let (result, response) = std::sync::mpsc::channel();
    let listener = Arc::new(DraftListener { updates: Mutex::new(Vec::new()), result });
    let compose = app.core.new_compose(None).unwrap();
    let _task = app.core.draft_message(compose, "Say hi".into(), listener.clone());
    let outcome = response.recv_timeout(Duration::from_secs(5)).unwrap();
    let AiOutcome::Done { text, .. } = outcome else { panic!("draft generation did not finish") };

    assert_eq!(*listener.updates.lock().unwrap(), [
        "  Hi \\*all\\*,\n",
        "  Hi \\*all\\*,\nsee you at \\[time\\].",
    ]);
    assert_eq!(jolta_core::rich::to_text(&text), "Hi *all*,\nsee you at [time].");
}
