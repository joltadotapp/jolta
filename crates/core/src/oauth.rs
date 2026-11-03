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

    /// The mail scopes, without the sign-in ones.
    fn mail_scopes(self) -> &'static [&'static str] {
        match self {
            Provider::Google => &["https://mail.google.com/"],
            Provider::Microsoft => &[
                "https://graph.microsoft.com/Mail.ReadWrite",
                "https://graph.microsoft.com/Mail.Send",
                "https://graph.microsoft.com/User.Read",
            ],
        }
    }

    /// Asked for only when the user turns calendar on for the account.
    pub fn calendar_scopes(self) -> &'static [&'static str] {
        match self {
            Provider::Google => &[
                "https://www.googleapis.com/auth/calendar.events",
                "https://www.googleapis.com/auth/calendar.calendarlist.readonly",
                "https://www.googleapis.com/auth/calendar.freebusy",
            ],
            Provider::Microsoft => &[
                "https://graph.microsoft.com/Calendars.ReadWrite",
                "https://graph.microsoft.com/Calendars.ReadWrite.Shared",
                "https://graph.microsoft.com/MailboxSettings.ReadWrite",
            ],
        }
    }

    fn identity_scopes(self) -> &'static [&'static str] {
        match self {
            Provider::Google => &["openid", "email", "profile"],
            Provider::Microsoft => &["openid", "email", "profile", "offline_access"],
        }
    }

    /// The `scope` of an authorization request. Google adds calendar to the
    /// grant it already has (`include_granted_scopes`), so it is asked only
    /// for the new scopes. Microsoft's dynamic consent asks for the whole set.
    fn request_scopes(self, ask: Ask) -> String {
        let extra: &[&[&str]] = match (self, ask) {
            (_, Ask::Account) => &[self.mail_scopes()],
            (Provider::Google, Ask::Calendar) => &[self.calendar_scopes()],
            (Provider::Microsoft, Ask::Calendar) => &[self.mail_scopes(), self.calendar_scopes()],
        };
        [&[self.identity_scopes()], extra].concat().concat().join(" ")
    }

    /// The `scope` of a refresh request. Google's tokens carry everything
    /// granted, so it takes none; Microsoft's carry only what the refresh
    /// asks for, so an account with calendar on asks for calendar too.
    fn refresh_scopes(self, calendar: bool) -> Option<String> {
        match self {
            Provider::Google => None,
            Provider::Microsoft => {
                let calendar: &[&str] = if calendar { self.calendar_scopes() } else { &[] };
                let all = [self.identity_scopes(), self.mail_scopes(), calendar].concat();
                Some(all.join(" "))
            }
        }
    }

    /// Scopes the reply must grant, where the user can refuse one and still
    /// finish signing in: Google's consent screen shows each as a checkbox,
    /// and Gmail answers a login without mail access with AUTHENTICATIONFAILED.
    /// Adding calendar must also leave the mail grant in place.
    fn required_scopes(self, ask: Ask) -> Vec<&'static str> {
        match (self, ask) {
            (Provider::Google, Ask::Account) => self.mail_scopes().to_vec(),
            (Provider::Google, Ask::Calendar) => [self.mail_scopes(), self.calendar_scopes()].concat(),
            (Provider::Microsoft, Ask::Account) => Vec::new(),
            (Provider::Microsoft, Ask::Calendar) => self.calendar_scopes().to_vec(),
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

/// What a sign-in is for: adding the account (mail), or adding calendar
/// access to an account Jolta already has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    Account,
    Calendar,
}

/// A sign-in in progress: the browser URL to open and the listener that
/// will receive the redirect.
pub struct Pending {
    pub url: String,
    provider: Provider,
    ask: Ask,
    /// The address that must sign in, when adding calendar to an account.
    address: Option<String>,
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

/// Starts a sign-in. `port` 0 picks a free port; a fixed port lets the
/// redirect come in through an SSH tunnel from another machine's browser.
pub async fn begin(provider: Provider, client: Client, port: u16) -> Result<Pending> {
    start(provider, client, port, Ask::Account, None).await
}

/// Starts the sign-in that adds calendar access to `account`, which is
/// signed in already: the same account, so mail keeps working on the new
/// refresh token. Save the result with `save_grant`.
pub async fn begin_calendar(account: &Account, client: Client, port: u16) -> Result<Pending> {
    let provider = Provider::for_kind(account.kind)
        .ok_or_else(|| Error::Auth(format!("{} uses its password for calendars, not a sign-in", account.address)))?;
    start(provider, client, port, Ask::Calendar, Some(account.address.clone())).await
}

async fn start(provider: Provider, client: Client, port: u16, ask: Ask, address: Option<String>) -> Result<Pending> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    // Microsoft matches the registered "http://localhost" (any port) and
    // treats 127.0.0.1 as a different host; Google asks for 127.0.0.1.
    let host = match provider {
        Provider::Google => "127.0.0.1",
        Provider::Microsoft => "localhost",
    };
    let redirect = format!("http://{host}:{port}/");
    let verifier = random_url_safe(32)?;
    let state = random_url_safe(16)?;
    let url = authorize_url(provider, &client.id, &redirect, &verifier, &state, ask, address.as_deref());
    Ok(Pending { url, provider, ask, address, client, verifier, state, redirect, listener })
}

fn authorize_url(
    provider: Provider,
    client_id: &str,
    redirect: &str,
    verifier: &str,
    state: &str,
    ask: Ask,
    address: Option<&str>,
) -> String {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let scope = provider.request_scopes(ask);
    let mut params = vec![
        ("client_id", client_id),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("scope", scope.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    match (provider, ask) {
        // Ask for a refresh token every time, so re-adding an account works.
        (Provider::Google, _) => params.extend([("access_type", "offline"), ("prompt", "consent")]),
        (Provider::Microsoft, Ask::Account) => params.push(("prompt", "select_account")),
        (Provider::Microsoft, Ask::Calendar) => {}
    }
    if provider == Provider::Google && ask == Ask::Calendar {
        params.push(("include_granted_scopes", "true"));
    }
    if let Some(address) = address {
        params.push(("login_hint", address));
    }
    format!("{}?{}", provider.auth_url(), form(&params))
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Jolta</title>\
<body style=\"font:15px -apple-system,system-ui;margin:20vh auto;max-width:420px;text-align:center\">\
<h2>You're signed in</h2><p>You can close this tab and go back to Jolta.</p>";

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
        if let Some(granted) = tokens["scope"].as_str() {
            let missing: Vec<&str> =
                self.provider.required_scopes(self.ask).into_iter().filter(|w| !has_scope(granted, w)).collect();
            if missing.iter().any(|m| self.provider.mail_scopes().contains(m)) {
                return Err(Error::Auth(
                    "you didn't give Jolta access to your mail. Sign in again and tick the box that lets Jolta \
                     read, compose and send your email."
                        .into(),
                ));
            }
            if !missing.is_empty() {
                return Err(Error::Auth(
                    "you didn't give Jolta access to your calendar. Turn calendar on again and tick every \
                     calendar box on the consent screen."
                        .into(),
                ));
            }
        }
        let access_token = tokens["access_token"].as_str().unwrap_or_default().to_string();
        let claims = tokens["id_token"].as_str().and_then(jwt_claims).unwrap_or(Value::Null);
        let email = claims["email"]
            .as_str()
            .or_else(|| claims["preferred_username"].as_str())
            .ok_or_else(|| Error::Auth("the provider did not say which address signed in".into()))?
            .to_string();
        if let Some(address) = &self.address
            && !email.eq_ignore_ascii_case(address)
        {
            return Err(Error::Auth(format!(
                "you signed in as {email}, but calendar was being turned on for {address}. Sign in as {address}."
            )));
        }
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
            // The request line can arrive in pieces (through an SSH tunnel, say);
            // read until it is complete.
            let mut buf = Vec::with_capacity(8192);
            let mut chunk = [0u8; 4096];
            while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 64 * 1024 {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let request = String::from_utf8_lossy(&buf);
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

/// Whether a token reply's space-separated `scope` holds `wanted`.
/// Microsoft lists Graph scopes without their resource address.
fn has_scope(granted: &str, wanted: &str) -> bool {
    let short = |s: &str| s.trim_start_matches(GRAPH_RESOURCE).to_ascii_lowercase();
    granted.split_whitespace().any(|g| short(g) == short(wanted))
}

const GRAPH_RESOURCE: &str = "https://graph.microsoft.com/";

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

/// HTTPS clients use ring, the one crypto provider in Jolta's TLS stack.
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
    let scope = provider.refresh_scopes(account.calendar);
    if let Some(scope) = &scope {
        params.push(("scope", scope));
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

    #[test]
    fn microsoft_refresh_asks_for_calendar_only_when_it_is_on() {
        assert_eq!(
            Provider::Microsoft.refresh_scopes(false).as_deref(),
            Some(
                "openid email profile offline_access https://graph.microsoft.com/Mail.ReadWrite \
                 https://graph.microsoft.com/Mail.Send https://graph.microsoft.com/User.Read"
            ),
            "mail-only accounts refresh with exactly the scopes they always had"
        );
        assert_eq!(
            Provider::Microsoft.refresh_scopes(true).as_deref(),
            Some(
                "openid email profile offline_access https://graph.microsoft.com/Mail.ReadWrite \
                 https://graph.microsoft.com/Mail.Send https://graph.microsoft.com/User.Read \
                 https://graph.microsoft.com/Calendars.ReadWrite https://graph.microsoft.com/Calendars.ReadWrite.Shared \
                 https://graph.microsoft.com/MailboxSettings.ReadWrite"
            )
        );
        assert_eq!(Provider::Google.refresh_scopes(true), None, "Google's refresh returns every granted scope");
    }

    fn params(url: &str) -> HashMap<String, String> {
        query_of(url)
    }

    #[test]
    fn google_calendar_consent_adds_to_the_existing_grant() {
        let url = authorize_url(Provider::Google, "cid", "http://127.0.0.1:8765/", "verifier", "st", Ask::Calendar, Some("me@example.com"));
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        let p = params(&url);
        assert_eq!(
            p["scope"],
            "openid email profile https://www.googleapis.com/auth/calendar.events \
             https://www.googleapis.com/auth/calendar.calendarlist.readonly https://www.googleapis.com/auth/calendar.freebusy"
        );
        assert_eq!(p["include_granted_scopes"], "true");
        assert_eq!(p["login_hint"], "me@example.com");
        assert_eq!(p["access_type"], "offline");
        assert_eq!(p["code_challenge_method"], "S256");
        assert_eq!(p["code_challenge"], URL_SAFE_NO_PAD.encode(Sha256::digest(b"verifier")));
        assert_eq!(p["redirect_uri"], "http://127.0.0.1:8765/");
        let add = params(&authorize_url(Provider::Google, "cid", "http://127.0.0.1:1/", "v", "st", Ask::Account, None));
        assert_eq!(add["scope"], "openid email profile https://mail.google.com/", "adding an account asks for mail only");
        assert!(!add.contains_key("include_granted_scopes") && !add.contains_key("login_hint"));
    }

    #[test]
    fn microsoft_calendar_consent_asks_for_mail_and_calendar() {
        let p = params(&authorize_url(Provider::Microsoft, "cid", "http://localhost:8765/", "v", "st", Ask::Calendar, Some("me@example.com")));
        assert_eq!(Some(p["scope"].clone()), Provider::Microsoft.refresh_scopes(true));
        assert_eq!(p["login_hint"], "me@example.com");
        assert!(!p.contains_key("prompt"), "the login hint picks the account; consent shows by itself");
    }

    #[test]
    fn granted_scopes_match_with_or_without_the_graph_address() {
        let granted = "Calendars.ReadWrite Mail.ReadWrite openid profile";
        assert!(has_scope(granted, "https://graph.microsoft.com/Calendars.ReadWrite"));
        assert!(!has_scope(granted, "https://graph.microsoft.com/Calendars.ReadWrite.Shared"));
        assert!(has_scope("https://mail.google.com/ openid", "https://mail.google.com/"));
    }
}

/// Stores the refresh token of a calendar sign-in for the account it was
/// started for, in the same Keychain entry, replacing the mail-only one.
pub fn save_grant(account: &Account, signed: &SignedIn) -> Result<()> {
    secrets::set_password(&account.secret_ref, &signed.refresh_token)?;
    remember(&account.secret_ref, &signed.access_token, signed.expires_in);
    Ok(())
}

/// Stores a finished sign-in as an account (or refreshes the token of the
/// account with that address) and returns it.
pub fn save_account(jolta: &crate::Jolta, provider: Provider, signed: &SignedIn) -> Result<Account> {
    let kind = match provider {
        Provider::Google => AccountKind::Gmail,
        Provider::Microsoft => AccountKind::Graph,
    };
    let existing = jolta
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
            jolta.store.write(|tx| crate::store::insert_account(tx, &new))?
        }
    };
    secrets::set_password(&account.secret_ref, &signed.refresh_token)?;
    remember(&account.secret_ref, &signed.access_token, signed.expires_in);
    Ok(account)
}
