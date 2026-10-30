//! OAuth 2 sign-in for desktop apps: PKCE (S256), the system browser, and a
//! one-shot listener on 127.0.0.1 that catches the redirect. Refresh tokens
//! live in the Keychain; access tokens are cached in memory and refreshed a
//! minute before they expire.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::model::{Account, AccountKind};
use crate::{Error, Result, secrets};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Google,
    Microsoft,
}

/// Client ids come from `oauth-clients.env` in the data folder (written by
/// the setup wizards), or the environment. Neither is in the repository.
#[derive(Clone, Debug)]
pub struct Client {
    pub id: String,
    pub secret: Option<String>,
}

impl Provider {
    fn auth_url(self) -> &'static str {
        match self {
            Provider::Google => "https://accounts.google.com/o/oauth2/v2/auth",
            Provider::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        }
    }

    fn token_url(self) -> &'static str {
        match self {
            Provider::Google => "https://oauth2.googleapis.com/token",
            Provider::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        }
    }

    fn scopes(self) -> &'static str {
        match self {
            Provider::Google => "openid email profile https://mail.google.com/",
            Provider::Microsoft => {
                "openid email profile offline_access https://graph.microsoft.com/Mail.ReadWrite \
                 https://graph.microsoft.com/Mail.Send https://graph.microsoft.com/User.Read"
            }
        }
    }

    fn env_prefix(self) -> &'static str {
        match self {
            Provider::Google => "GOOGLE",
            Provider::Microsoft => "MICROSOFT",
        }
    }

    pub fn for_kind(kind: AccountKind) -> Option<Provider> {
        match kind {
            AccountKind::Gmail => Some(Provider::Google),
            AccountKind::Graph => Some(Provider::Microsoft),
            AccountKind::Imap => None,
        }
    }

    /// The client registered in Google Cloud or Entra for this Mac.
    pub fn client(self, data_dir: &std::path::Path) -> Result<Client> {
        let mut values: HashMap<String, String> = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(data_dir.join("oauth-clients.env")) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    values.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
        let get = |name: &str| {
            let key = format!("{}_{name}", self.env_prefix());
            std::env::var(&key).ok().or_else(|| values.get(&key).cloned()).filter(|v| !v.is_empty())
        };
        let id = get("CLIENT_ID").ok_or_else(|| {
            Error::Auth(format!("No {} OAuth client yet: run the setup wizard in scripts/.", self.env_prefix().to_lowercase()))
        })?;
        Ok(Client { id, secret: get("CLIENT_SECRET") })
    }
}

fn random_url_safe(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| Error::Other(format!("no randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

fn form(pairs: &[(&str, &str)]) -> String {
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

/// A sign-in in progress: the browser URL to open and the listener that
/// will receive the redirect.
pub struct Pending {
    pub url: String,
    provider: Provider,
    client: Client,
    verifier: String,
    state: String,
    redirect: String,
    listener: TcpListener,
}

/// What a finished sign-in yields.
#[derive(Clone, Debug)]
pub struct SignedIn {
    pub email: String,
    pub name: String,
    pub refresh_token: String,
    pub access_token: String,
    pub expires_in: u64,
}

pub async fn begin(provider: Provider, client: Client) -> Result<Pending> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/");
    let verifier = random_url_safe(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_url_safe(16)?;
    let mut params = vec![
        ("client_id", client.id.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("response_type", "code"),
        ("scope", provider.scopes()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
    ];
    if provider == Provider::Google {
        // Ask for a refresh token every time, so re-adding an account works.
        params.extend([("access_type", "offline"), ("prompt", "consent")]);
    } else {
        params.push(("prompt", "select_account"));
    }
    let url = format!("{}?{}", provider.auth_url(), form(&params));
    Ok(Pending { url, provider, client, verifier, state, redirect, listener })
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Kite</title>\
<body style=\"font:15px -apple-system,system-ui;margin:20vh auto;max-width:420px;text-align:center\">\
<h2>You're signed in</h2><p>You can close this tab and go back to Kite.</p>";

impl Pending {
    /// Waits for the browser to come back (up to ten minutes), then trades
    /// the code for tokens.
    pub async fn finish(self) -> Result<SignedIn> {
        self.finish_or_paste(std::future::pending()).await
    }

    /// Like `finish`, but also accepts the redirect address pasted by hand,
    /// for when the browser runs on another machine and 127.0.0.1 cannot
    /// reach this one.
    pub async fn finish_or_paste(self, pasted: impl std::future::Future<Output = Option<String>>) -> Result<SignedIn> {
        let code = tokio::time::timeout(Duration::from_secs(600), async {
            tokio::select! {
                code = self.receive_code() => code,
                Some(url) = pasted => self.code_from_query(&query_of(&url)),
            }
        })
        .await
        .map_err(|_| Error::Auth("sign-in timed out after ten minutes".into()))??;
        let mut params = vec![
            ("client_id", self.client.id.as_str()),
            ("code", code.as_str()),
            ("code_verifier", self.verifier.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", self.redirect.as_str()),
        ];
        if let Some(secret) = &self.client.secret {
            params.push(("client_secret", secret));
        }
        let tokens = post_token(self.provider, &params).await?;
        let refresh_token = tokens["refresh_token"]
            .as_str()
            .ok_or_else(|| Error::Auth("the provider sent no refresh token".into()))?
            .to_string();
        let access_token = tokens["access_token"].as_str().unwrap_or_default().to_string();
        let claims = tokens["id_token"].as_str().and_then(jwt_claims).unwrap_or(Value::Null);
        let email = claims["email"]
            .as_str()
            .or_else(|| claims["preferred_username"].as_str())
            .ok_or_else(|| Error::Auth("the provider did not say which address signed in".into()))?
            .to_string();
        Ok(SignedIn {
            name: claims["name"].as_str().unwrap_or_default().to_string(),
            email,
            refresh_token,
            access_token,
            expires_in: tokens["expires_in"].as_u64().unwrap_or(3600),
        })
    }

    async fn receive_code(&self) -> Result<String> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await?;
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request.split_whitespace().nth(1).unwrap_or("/");
            let query = query_of(path);
            if query.is_empty() {
                // A favicon or other stray request.
                stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await.ok();
                continue;
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{DONE_PAGE}",
                DONE_PAGE.len()
            );
            stream.write_all(reply.as_bytes()).await.ok();
            return self.code_from_query(&query);
        }
    }

    fn code_from_query(&self, query: &HashMap<String, String>) -> Result<String> {
        if query.get("state") != Some(&self.state) {
            return Err(Error::Auth("the sign-in reply did not match this request".into()));
        }
        if let Some(err) = query.get("error") {
            let detail = query.get("error_description").cloned().unwrap_or_default();
            return Err(Error::Auth(format!("{err} {detail}").trim().to_string()));
        }
        query.get("code").cloned().ok_or_else(|| Error::Auth("the sign-in reply had no code".into()))
    }
}

/// The query parameters of a URL or path, decoded.
fn query_of(url: &str) -> HashMap<String, String> {
    url.trim()
        .split_once('?')
        .map(|(_, q)| {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), percent_decode(v)))
                .collect()
        })
        .unwrap_or_default()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The payload of an ID token. It comes straight from the token endpoint
/// over TLS, so its signature needs no separate check here.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

/// HTTPS clients use ring, the one crypto provider in Kite's TLS stack.
pub fn init_tls() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
}

async fn post_token(provider: Provider, params: &[(&str, &str)]) -> Result<Value> {
    init_tls();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Other(format!("HTTP client: {e}")))?;
    let response = client
        .post(provider.token_url())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form(params))
        .send()
        .await
        .map_err(|e| Error::Other(format!("token request failed: {e}")))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| Error::Other(format!("token response: {e}")))?;
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = json["error_description"].as_str().or_else(|| json["error"].as_str()).unwrap_or(&text);
        return Err(Error::Auth(format!("{} said: {reason}", provider.env_prefix().to_lowercase())));
    }
    Ok(json)
}

struct Cached {
    token: String,
    until: Instant,
}

static TOKENS: Mutex<Option<HashMap<String, Cached>>> = Mutex::new(None);

/// Keeps an access token from sign-in so the first connection needs no refresh.
pub fn remember(account_ref: &str, access_token: &str, expires_in: u64) {
    let mut cache = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    cache.get_or_insert_with(HashMap::new).insert(
        account_ref.to_string(),
        Cached { token: access_token.to_string(), until: Instant::now() + Duration::from_secs(expires_in.saturating_sub(60)) },
    );
}

static DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Where `oauth-clients.env` lives; set when the data folder opens.
pub fn set_data_dir(dir: &std::path::Path) {
    let _ = DATA_DIR.set(dir.to_path_buf());
}

pub fn data_dir() -> std::path::PathBuf {
    DATA_DIR.get().cloned().unwrap_or_else(crate::default_data_dir)
}

/// A valid access token for an OAuth account, refreshed when needed.
/// Providers that rotate refresh tokens get the new one stored.
pub async fn access_token(account: &Account) -> Result<String> {
    let data_dir = data_dir();
    let provider = Provider::for_kind(account.kind).ok_or_else(|| Error::Auth("not an OAuth account".into()))?;
    if let Some(c) = TOKENS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(&account.secret_ref)) {
        if c.until > Instant::now() {
            return Ok(c.token.clone());
        }
    }
    let client = provider.client(&data_dir)?;
    let refresh = secrets::password(&account.secret_ref)?;
    let mut params = vec![
        ("client_id", client.id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
    ];
    if let Some(secret) = &client.secret {
        params.push(("client_secret", secret));
    }
    if provider == Provider::Microsoft {
        params.push(("scope", provider.scopes()));
    }
    let tokens = post_token(provider, &params).await?;
    let token = tokens["access_token"].as_str().ok_or_else(|| Error::Auth("no access token in the refresh reply".into()))?;
    if let Some(rotated) = tokens["refresh_token"].as_str() {
        if rotated != refresh {
            secrets::set_password(&account.secret_ref, rotated)?;
        }
    }
    remember(&account.secret_ref, token, tokens["expires_in"].as_u64().unwrap_or(3600));
    Ok(token.to_string())
}

/// The SASL XOAUTH2 string for IMAP and SMTP.
pub fn xoauth2(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_and_decoding() {
        assert_eq!(form(&[("a b", "c&d=é")]), "a%20b=c%26d%3D%C3%A9");
        assert_eq!(percent_decode("4%2F0Ab+c"), "4/0Ab c");
    }

    #[test]
    fn reads_id_token_claims() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"email":"koen@example.com","name":"Koen"}"#);
        let claims = jwt_claims(&format!("x.{payload}.sig")).unwrap();
        assert_eq!(claims["email"], "koen@example.com");
    }
}

/// Stores a finished sign-in as an account (or refreshes the token of the
/// account with that address) and returns it.
pub fn save_account(kite: &crate::Kite, provider: Provider, signed: &SignedIn) -> Result<Account> {
    let kind = match provider {
        Provider::Google => AccountKind::Gmail,
        Provider::Microsoft => AccountKind::Graph,
    };
    let existing = kite
        .store
        .read(crate::store::accounts)?
        .into_iter()
        .find(|a| a.kind == kind && a.address.eq_ignore_ascii_case(&signed.email));
    let account = match existing {
        Some(a) => a,
        None => {
            let (imap_host, imap_port, smtp_host, smtp_port) = match provider {
                Provider::Google => ("imap.gmail.com", 993, "smtp.gmail.com", 465),
                Provider::Microsoft => ("", 0, "", 0),
            };
            let new = crate::store::NewAccount {
                kind,
                address: signed.email.clone(),
                display_name: signed.name.clone(),
                username: signed.email.clone(),
                imap_host: imap_host.into(),
                imap_port,
                smtp_host: smtp_host.into(),
                smtp_port,
            };
            kite.store.write(|tx| crate::store::insert_account(tx, &new))?
        }
    };
    secrets::set_password(&account.secret_ref, &signed.refresh_token)?;
    remember(&account.secret_ref, &signed.access_token, signed.expires_in);
    Ok(account)
}
