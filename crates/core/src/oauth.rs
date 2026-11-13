//! OAuth 2 sign-in for desktop apps: PKCE (S256), the system browser, and a
//! one-shot listener on 127.0.0.1 that catches the redirect. Refresh tokens
//! live in the Keychain; access tokens are cached in memory and refreshed a
//! minute before they expire.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

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

/// Client ids come from the environment, then `oauth-clients.env` in the data
/// folder (written by the setup wizards), then the ones a release build
/// carries. None of them is in the repository.
#[derive(Clone)]
pub struct Client {
    pub id: String,
    pub secret: Option<String>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("id", &self.id).field("secret", &self.secret.as_ref().map(|_| REDACTED)).finish()
    }
}

const REDACTED: &str = "<redacted>";

impl Provider {
    fn auth_url(self) -> &'static str {
        match self {
            Provider::Google => "https://accounts.google.com/o/oauth2/v2/auth",
            Provider::Microsoft => "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        }
    }

    /// Where removing an account withdraws its grant. Google's revoke ends
    /// the whole grant, so the account is signed out on every Mac. Microsoft
    /// lets a public client revoke no single token.
    pub fn revoke_url(self) -> Option<&'static str> {
        match self {
            Provider::Google => Some("https://oauth2.googleapis.com/revoke"),
            Provider::Microsoft => None,
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

    /// The `scope` of an authorization request. Turning calendar on asks for
    /// mail too: the account may have no mail grant for this client, such as
    /// after a move to a new Google project, and Google shows only the
    /// scopes the account hasn't granted yet.
    fn request_scopes(self, ask: Ask) -> String {
        let extra: &[&[&str]] = match ask {
            Ask::Account => &[self.mail_scopes()],
            Ask::Calendar => &[self.mail_scopes(), self.calendar_scopes()],
            Ask::Rooms => &[ROOMS_SCOPES],
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

    /// The `scope` of the refresh that gets the room lists' token. It is a
    /// refresh of its own: Microsoft refuses a whole refresh when one scope
    /// it asks for lost its consent, so an admin taking room lists back must
    /// not stop mail and calendar.
    fn rooms_refresh_scopes(self) -> Option<String> {
        (self == Provider::Microsoft).then(|| ["offline_access"].iter().chain(ROOMS_SCOPES).copied().collect::<Vec<_>>().join(" "))
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
            (_, Ask::Rooms) => ROOMS_SCOPES.to_vec(),
        }
    }

    /// The id_token claim that names the user for good. Microsoft's `sub`
    /// differs per app; `oid` is the user's object id in their directory.
    fn user_id_claim(self) -> &'static str {
        match self {
            Provider::Google => "sub",
            Provider::Microsoft => "oid",
        }
    }

    fn env_prefix(self) -> &'static str {
        match self {
            Provider::Google => "GOOGLE",
            Provider::Microsoft => "MICROSOFT",
        }
    }

    /// The client a release build carries: `scripts/release.sh` sets these
    /// while it compiles the core, so a downloaded Jolta can sign in without
    /// an `oauth-clients.env`.
    fn built_in(self, name: &str) -> Option<&'static str> {
        match (self, name) {
            (Provider::Google, "CLIENT_ID") => option_env!("JOLTA_GOOGLE_CLIENT_ID"),
            (Provider::Google, "CLIENT_SECRET") => option_env!("JOLTA_GOOGLE_CLIENT_SECRET"),
            (Provider::Microsoft, "CLIENT_ID") => option_env!("JOLTA_MICROSOFT_CLIENT_ID"),
            _ => None,
        }
        .filter(|v| !v.is_empty())
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
            std::env::var(&key)
                .ok()
                .or_else(|| values.get(&key).cloned())
                .filter(|v| !v.is_empty())
                .or_else(|| self.built_in(name).map(str::to_string))
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

/// Microsoft 365's room lists, with each room's size and building. A
/// tenant admin has to consent to it, so Jolta asks only when the room
/// picker first opens.
const ROOMS_SCOPES: &[&str] = &["https://graph.microsoft.com/Place.Read.All"];

/// What a sign-in is for: adding the account (mail), adding calendar
/// access to an account Jolta already has, or reading its organization's
/// room lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    Account,
    Calendar,
    Rooms,
}

/// The access token an API call needs: the account's own, which covers
/// mail and calendar, or the one for Microsoft 365's room lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    Account,
    Rooms,
}

/// How a consent ended: granted, or refused in the browser with
/// Microsoft's reason, such as a tenant that lets only an admin consent.
#[derive(Debug)]
pub enum Consent {
    Granted(SignedIn),
    Refused(String),
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
#[derive(Clone)]
pub struct SignedIn {
    pub email: String,
    pub name: String,
    pub refresh_token: String,
    pub access_token: String,
    pub expires_in: u64,
    /// The provider's permanent id for the user, which an address change
    /// keeps: Google's `sub`, Microsoft's `oid`.
    pub user_id: Option<String>,
}

impl std::fmt::Debug for SignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedIn")
            .field("email", &self.email)
            .field("name", &self.name)
            .field("refresh_token", &REDACTED)
            .field("access_token", &REDACTED)
            .field("expires_in", &self.expires_in)
            .field("user_id", &self.user_id)
            .finish()
    }
}

impl SignedIn {
    /// Reads a token reply to an authorization code.
    fn from_tokens(provider: Provider, tokens: &Value) -> Result<SignedIn> {
        let refresh_token = tokens["refresh_token"]
            .as_str()
            .ok_or_else(|| Error::Auth("the provider sent no refresh token".into()))?
            .to_string();
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
            access_token: tokens["access_token"].as_str().unwrap_or_default().to_string(),
            expires_in: tokens["expires_in"].as_u64().unwrap_or(3600),
            user_id: claims[provider.user_id_claim()].as_str().map(str::to_string),
        })
    }
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

/// Starts the consent to Microsoft 365's room lists for `account`. Finish
/// it with `Pending::finish_consent`; the account keeps its refresh token.
pub async fn begin_rooms(account: &Account, client: Client, port: u16) -> Result<Pending> {
    if account.kind != AccountKind::Graph {
        return Err(Error::Auth(format!("{} has no room lists to ask for", account.address)));
    }
    start(Provider::Microsoft, client, port, Ask::Rooms, Some(account.address.clone())).await
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
        (Provider::Microsoft, Ask::Calendar | Ask::Rooms) => {}
    }
    // Signing in to Google again keeps calendar access granted earlier.
    if provider == Provider::Google {
        params.push(("include_granted_scopes", "true"));
    }
    if let Some(address) = address {
        params.push(("login_hint", address));
    }
    format!("{}?{}", provider.auth_url(), form(&params))
}

/// A plain page for the browser tab the sign-in comes back to.
fn page(status: &str, title: &str, text: &str) -> String {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Jolta</title>\
         <body style=\"font:15px -apple-system,system-ui;margin:20vh auto;max-width:420px;text-align:center\">\
         <h2>{title}</h2><p>{text}</p>"
    );
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// What reached the sign-in listener.
enum Callback {
    /// A favicon or another request without a query.
    Stray,
    /// A reply whose `state` is missing or isn't this sign-in's: an old
    /// tab, or a page trying to slip in its own code.
    Foreign,
    /// This sign-in's reply.
    Reply(Result<Redirect>),
}

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
        match self.finish_consent(pasted).await? {
            Consent::Granted(signed) => Ok(signed),
            Consent::Refused(reason) => Err(Error::Auth(reason)),
        }
    }

    /// Like `finish_or_paste`, but a refusal in the browser, or a reply
    /// without the room lists, is an answer rather than an error.
    pub async fn finish_consent(self, pasted: impl std::future::Future<Output = Option<String>>) -> Result<Consent> {
        let reply = tokio::time::timeout(Duration::from_secs(600), async {
            tokio::select! {
                code = self.receive_code() => code,
                Some(url) = pasted => self.code_from_query(&query_of(&url)),
            }
        })
        .await
        .map_err(|_| Error::Auth("sign-in timed out after ten minutes".into()))??;
        let code = match reply {
            Redirect::Code(code) => code,
            Redirect::Refused(reason) => return Ok(Consent::Refused(reason)),
        };
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
        if tokens["refresh_token"].as_str().is_none() {
            return Err(Error::Auth("the provider sent no refresh token".into()));
        }
        if let Some(reason) = refused_scopes(self.provider, self.ask, tokens["scope"].as_str())? {
            return Ok(Consent::Refused(reason));
        }
        let signed = SignedIn::from_tokens(self.provider, &tokens)?;
        if let Some(address) = &self.address
            && !signed.email.eq_ignore_ascii_case(address)
        {
            let what = if self.ask == Ask::Rooms { "room lists were being asked" } else { "calendar was being turned on" };
            let email = &signed.email;
            return Err(Error::Auth(format!("you signed in as {email}, but {what} for {address}. Sign in as {address}.")));
        }
        Ok(Consent::Granted(signed))
    }

    async fn receive_code(&self) -> Result<Redirect> {
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
            let callback = self.callback(&query_of(path));
            let reply = match &callback {
                Callback::Stray => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                Callback::Foreign => page(
                    "400 Bad Request",
                    "This isn't the sign-in Jolta is waiting for",
                    "Close this tab. To sign in, start again from Jolta.",
                ),
                Callback::Reply(Ok(Redirect::Code(_))) => {
                    page("200 OK", "You're signed in", "You can close this tab and go back to Jolta.")
                }
                Callback::Reply(_) => page("200 OK", "Sign-in didn't finish", "Go back to Jolta to see why, or to try again."),
            };
            stream.write_all(reply.as_bytes()).await.ok();
            if let Callback::Reply(reply) = callback {
                return reply;
            }
        }
    }

    /// A redirect address pasted by hand, which has no browser tab to answer.
    fn code_from_query(&self, query: &HashMap<String, String>) -> Result<Redirect> {
        match self.callback(query) {
            Callback::Reply(reply) => reply,
            Callback::Stray | Callback::Foreign => Err(Error::Auth("the sign-in reply did not match this request".into())),
        }
    }

    fn callback(&self, query: &HashMap<String, String>) -> Callback {
        if query.is_empty() {
            return Callback::Stray;
        }
        if query.get("state") != Some(&self.state) {
            return Callback::Foreign;
        }
        if let Some(err) = query.get("error") {
            let detail = query.get("error_description").cloned().unwrap_or_default();
            return Callback::Reply(Ok(Redirect::Refused(format!("{err} {detail}").trim().to_string())));
        }
        Callback::Reply(query.get("code").cloned().map(Redirect::Code).ok_or_else(|| Error::Auth("the sign-in reply had no code".into())))
    }
}

/// Whether a token reply's `scope` grants what `ask` must have: an error
/// when mail or calendar is missing, the reason when the room lists are.
/// A reply without `scope` grants everything asked, as OAuth 2 says,
/// except the room lists, which Jolta must see granted.
fn refused_scopes(provider: Provider, ask: Ask, granted: Option<&str>) -> Result<Option<String>> {
    let granted = match (granted, ask) {
        (Some(granted), _) => granted,
        (None, Ask::Rooms) => "",
        (None, _) => return Ok(None),
    };
    let missing: Vec<&str> = provider.required_scopes(ask).into_iter().filter(|w| !has_scope(granted, w)).collect();
    if missing.is_empty() {
        return Ok(None);
    }
    if ask == Ask::Rooms {
        return Ok(Some("Microsoft didn't give Jolta your organization's room lists.".into()));
    }
    if missing.iter().any(|m| provider.mail_scopes().contains(m)) {
        return Err(Error::Auth(
            "you didn't give Jolta access to your mail. Sign in again and tick the box that lets Jolta \
             read, compose and send your email."
                .into(),
        ));
    }
    Err(Error::Auth(
        "you didn't give Jolta access to your calendar. Turn calendar on again and tick every \
         calendar box on the consent screen."
            .into(),
    ))
}

/// What the browser came back with.
enum Redirect {
    Code(String),
    /// The provider's `error` and `error_description`.
    Refused(String),
}

/// The reason Microsoft gives for refusing a consent or a token, without
/// its error code and the trace lines after it: "access_denied AADSTS65004:
/// User declined to consent to access the app.\r\nTrace ID: …" becomes
/// "User declined to consent to access the app." A token refused because
/// nobody consented (AADSTS65001) says so in Jolta's words.
pub fn refusal_reason(reply: &str) -> String {
    if reply.contains("AADSTS65001") {
        return "Nobody has allowed Jolta to read your organization's room lists yet.".into();
    }
    let first = reply.lines().next().unwrap_or_default();
    let first = first.split(" Trace ID:").next().unwrap_or_default().trim();
    let first = ["microsoft said:", "access_denied", "consent_required", "interaction_required"]
        .iter()
        .fold(first, |text, prefix| text.strip_prefix(prefix).map_or(text, str::trim));
    let reason = match first.split_once(": ") {
        Some((code, rest)) if code.starts_with("AADSTS") => rest,
        _ => first,
    };
    if reason.is_empty() { "Microsoft refused without saying why.".into() } else { reason.trim().to_string() }
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
        .map_err(|e| {
            let text = format!("token request failed: {e}");
            if e.is_connect() { Error::Offline(text) } else { Error::Unavailable(text) }
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|e| Error::Unavailable(format!("token response: {e}")))?;
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.as_u16() == 429 || status.is_server_error() {
        return Err(Error::Unavailable(format!("{} sign-in server: {status}", provider.env_prefix().to_lowercase())));
    }
    if !status.is_success() {
        let reason = json["error_description"].as_str().or_else(|| json["error"].as_str()).unwrap_or(&text);
        return Err(Error::Auth(format!("{} said: {reason}", provider.env_prefix().to_lowercase())));
    }
    Ok(json)
}

/// `until` is on the wall clock: `Instant` stops while a Mac sleeps.
struct Cached {
    token: String,
    until: SystemTime,
}

static TOKENS: Mutex<Option<HashMap<String, Cached>>> = Mutex::new(None);

/// Keeps an access token from sign-in so the first connection needs no refresh.
pub fn remember(account_ref: &str, access_token: &str, expires_in: u64) {
    let mut cache = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    cache.get_or_insert_with(HashMap::new).insert(
        account_ref.to_string(),
        Cached { token: access_token.to_string(), until: SystemTime::now() + Duration::from_secs(expires_in.saturating_sub(60)) },
    );
}

/// The access token kept under `key`, unless it has expired by `now`.
fn cached(key: &str, now: SystemTime) -> Option<String> {
    let cache = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    cache.as_ref()?.get(key).filter(|c| c.until > now).map(|c| c.token.clone())
}

/// Drops every access token kept for an account, its room lists' too.
pub fn forget(account: &Account) {
    if let Some(cache) = TOKENS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        for grant in [Grant::Account, Grant::Rooms] {
            cache.remove(&cache_key(account, grant));
        }
    }
}

/// The request that withdraws the grant behind a refresh token: where to
/// POST and the form to send.
fn revoke_request(provider: Provider, refresh_token: &str) -> Option<(&'static str, String)> {
    provider.revoke_url().map(|url| (url, form(&[("token", refresh_token)])))
}

/// Withdraws the grant behind `refresh_token` at the provider, best effort:
/// an account is removed whether or not the provider could be reached.
pub async fn revoke(provider: Provider, refresh_token: String) -> Result<()> {
    let Some((url, body)) = revoke_request(provider, &refresh_token) else { return Ok(()) };
    init_tls();
    let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|e| Error::Other(format!("HTTP client: {e}")))?;
    let response = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| Error::Other(format!("revoking at {url}: {e}")))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = response.text().await.unwrap_or_default();
    Err(Error::Auth(format!("revoking at {url}: {status} {}", detail.trim())))
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
    token(account, Grant::Account).await
}

/// The token for `grant`. The room lists' token is cached apart from the
/// account's and refreshed with its own scope; it never replaces the
/// stored refresh token.
pub async fn token(account: &Account, grant: Grant) -> Result<String> {
    let data_dir = data_dir();
    let provider = Provider::for_kind(account.kind).ok_or_else(|| Error::Auth("not an OAuth account".into()))?;
    let key = cache_key(account, grant);
    if let Some(token) = cached(&key, SystemTime::now()) {
        return Ok(token);
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
    let scope = match grant {
        Grant::Account => provider.refresh_scopes(account.calendar),
        Grant::Rooms => Some(provider.rooms_refresh_scopes().ok_or_else(|| Error::Auth(format!("{} has no room lists", account.address)))?),
    };
    if let Some(scope) = &scope {
        params.push(("scope", scope));
    }
    let tokens = post_token(provider, &params).await?;
    let token = tokens["access_token"].as_str().ok_or_else(|| Error::Auth("no access token in the refresh reply".into()))?;
    if let Some(rotated) = tokens["refresh_token"].as_str()
        && grant == Grant::Account
        && rotated != refresh
    {
        secrets::set_password(&account.secret_ref, rotated)?;
    }
    remember(&key, token, tokens["expires_in"].as_u64().unwrap_or(3600));
    Ok(token.to_string())
}

fn cache_key(account: &Account, grant: Grant) -> String {
    match grant {
        Grant::Account => account.secret_ref.clone(),
        Grant::Rooms => format!("{}#rooms", account.secret_ref),
    }
}

/// Keeps the token a granted room-lists consent came back with.
pub fn remember_rooms(account: &Account, signed: &SignedIn) {
    remember(&cache_key(account, Grant::Rooms), &signed.access_token, signed.expires_in);
}

/// The SASL XOAUTH2 string for IMAP and SMTP.
pub fn xoauth2(user: &str, token: &str) -> String {
    format!("user={user}\x01auth=Bearer {token}\x01\x01")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_build_signs_in_without_a_clients_file() {
        if std::env::var_os("GOOGLE_CLIENT_ID").is_some() {
            return;
        }
        let empty = std::env::temp_dir().join(format!("jolta-no-clients-{}", std::process::id()));
        match option_env!("JOLTA_GOOGLE_CLIENT_ID") {
            Some(id) => assert_eq!(Provider::Google.client(&empty).unwrap().id, id),
            None => assert!(Provider::Google.client(&empty).is_err()),
        }
    }

    /// A Mac that slept past a token's expiry gets a new token on waking,
    /// so the expiry is kept on the wall clock, which counts the sleep.
    #[test]
    fn a_token_expires_by_the_wall_clock() {
        let key = "test-wall-clock";
        remember(key, "t", 3600);
        assert_eq!(cached(key, SystemTime::now()).as_deref(), Some("t"));
        assert_eq!(cached(key, SystemTime::now() + Duration::from_secs(2 * 3600)), None, "two hours later");
    }

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

    #[test]
    fn room_lists_ask_microsoft_for_places_alone_and_refresh_apart() {
        let p = params(&authorize_url(Provider::Microsoft, "cid", "http://localhost:8765/", "v", "st", Ask::Rooms, Some("me@example.com")));
        assert_eq!(p["scope"], "openid email profile offline_access https://graph.microsoft.com/Place.Read.All");
        assert_eq!(p["login_hint"], "me@example.com");
        assert!(!p.contains_key("prompt"));
        assert_eq!(Provider::Microsoft.required_scopes(Ask::Rooms), ["https://graph.microsoft.com/Place.Read.All"]);
        assert_eq!(Provider::Microsoft.rooms_refresh_scopes().as_deref(), Some("offline_access https://graph.microsoft.com/Place.Read.All"));
        assert_eq!(Provider::Google.rooms_refresh_scopes(), None);
        for calendar in [false, true] {
            assert!(
                !Provider::Microsoft.refresh_scopes(calendar).unwrap().contains("Place"),
                "a granted account's mail and calendar refresh stays as it was, so a withdrawn consent can't stop mail"
            );
        }
    }

    #[test]
    fn a_room_lists_reply_counts_only_with_places_in_its_scope() {
        let granted = "Place.Read.All openid profile email";
        assert_eq!(refused_scopes(Provider::Microsoft, Ask::Rooms, Some(granted)).unwrap(), None);
        assert!(refused_scopes(Provider::Microsoft, Ask::Rooms, Some("openid profile User.Read")).unwrap().is_some());
        assert!(refused_scopes(Provider::Microsoft, Ask::Rooms, None).unwrap().is_some(), "room lists must be seen granted");
        assert_eq!(refused_scopes(Provider::Microsoft, Ask::Calendar, None).unwrap(), None, "a reply without scope grants what was asked");
        assert!(refused_scopes(Provider::Microsoft, Ask::Calendar, Some("Mail.ReadWrite openid")).is_err());
        assert!(refused_scopes(Provider::Google, Ask::Account, Some("openid email")).is_err(), "Google without mail");
    }

    #[test]
    fn a_refused_consent_keeps_microsofts_reason_without_codes_and_traces() {
        assert_eq!(
            refusal_reason("access_denied AADSTS65004: User declined to consent to access the app.\r\nTrace ID: 1f\r\nCorrelation ID: 2a"),
            "User declined to consent to access the app."
        );
        assert_eq!(refusal_reason("access_denied Need admin approval"), "Need admin approval");
        assert_eq!(refusal_reason("access_denied"), "Microsoft refused without saying why.");
        assert_eq!(
            refusal_reason("microsoft said: AADSTS65001: The user or administrator has not consented to use the application. Trace ID: 80 Correlation ID: dd"),
            "Nobody has allowed Jolta to read your organization's room lists yet.",
            "the refresh of a room lists token nobody consented to (Microsoft 365 test account, 2026-11-08)"
        );
        assert_eq!(refusal_reason("microsoft said: AADSTS700082: The refresh token has expired. Trace ID: 1"), "The refresh token has expired.");
    }

    #[tokio::test]
    async fn the_browser_coming_back_refused_is_an_answer_and_a_code_still_a_sign_in() {
        let account = Account {
            id: crate::AccountId(1),
            kind: AccountKind::Graph,
            address: "me@example.com".into(),
            display_name: String::new(),
            username: String::new(),
            imap_host: String::new(),
            imap_port: 0,
            smtp_host: String::new(),
            smtp_port: 0,
            secret_ref: "test".into(),
            imap_security: crate::model::Security::Tls,
            smtp_security: crate::model::Security::Tls,
            smtp_auth: crate::model::SmtpAuth::SameAsIncoming,
            smtp_username: String::new(),
            verify_certs: true,
            signature: String::new(),
            calendar: true,
            oauth_user_id: None,
        };
        let pending = begin_rooms(&account, Client { id: "cid".into(), secret: None }, 0).await.unwrap();
        let state = pending.state.clone();
        let refused = format!("http://localhost/?error=access_denied&error_description=AADSTS65004%3A+User+declined.%0D%0ATrace+ID%3A+1&state={state}");
        let Consent::Refused(reason) = pending.finish_consent(async { Some(refused) }).await.unwrap() else { panic!("refused") };
        assert_eq!(refusal_reason(&reason), "User declined.");
        let pending = begin_rooms(&account, Client { id: "cid".into(), secret: None }, 0).await.unwrap();
        let wrong = "http://localhost/?error=access_denied&state=other".to_string();
        assert!(pending.finish_consent(async { Some(wrong) }).await.is_err(), "a reply to another request is no answer");
        let google = Account { kind: AccountKind::Gmail, ..account };
        assert!(begin_rooms(&google, Client { id: "cid".into(), secret: None }, 0).await.is_err());
    }

    /// Sends one GET to the sign-in listener, as a browser would, and
    /// returns the status line and the page.
    async fn browse(redirect: &str, target: &str) -> (String, String) {
        let port = redirect.trim_end_matches('/').rsplit(':').next().unwrap();
        let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}")).await.unwrap();
        stream.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap_or((&reply, ""));
        (head.lines().next().unwrap_or_default().to_string(), body.to_string())
    }

    #[tokio::test]
    async fn the_listener_turns_away_requests_that_are_not_the_reply_and_keeps_waiting() {
        let pending = begin(Provider::Google, Client { id: "cid".into(), secret: None }, 0).await.unwrap();
        let (state, redirect) = (pending.state.clone(), pending.redirect.clone());
        let flow = tokio::spawn(pending.finish_consent(std::future::pending()));

        assert_eq!(browse(&redirect, "/favicon.ico").await.0, "HTTP/1.1 404 Not Found");
        for forged in ["/?code=stolen&state=other", "/?code=stolen", "/?error=access_denied&state="] {
            let (status, page) = browse(&redirect, forged).await;
            assert_eq!(status, "HTTP/1.1 400 Bad Request", "{forged}");
            assert!(!page.contains("signed in"), "{forged}: {page}");
        }
        assert!(!flow.is_finished(), "a forged reply doesn't end the sign-in");

        let (status, page) = browse(&redirect, &format!("/?error=access_denied&state={state}")).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(!page.contains("signed in"), "a refusal isn't shown as a sign-in: {page}");
        let Consent::Refused(reason) = flow.await.unwrap().unwrap() else { panic!("the real reply ends the sign-in") };
        assert_eq!(reason, "access_denied");
    }

    #[tokio::test]
    async fn the_reply_with_a_code_says_signed_in_and_hands_over_the_code() {
        let pending = begin(Provider::Google, Client { id: "cid".into(), secret: None }, 0).await.unwrap();
        let target = format!("/?code=c0de&state={}", pending.state);
        let (code, (status, page)) = tokio::join!(pending.receive_code(), browse(&pending.redirect, &target));
        assert!(matches!(code.unwrap(), Redirect::Code(c) if c == "c0de"));
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(page.contains("You're signed in"));

        let pending = begin(Provider::Google, Client { id: "cid".into(), secret: None }, 0).await.unwrap();
        let target = format!("/?state={}", pending.state);
        let (code, (_, page)) = tokio::join!(pending.receive_code(), browse(&pending.redirect, &target));
        assert!(code.is_err(), "the reply had no code");
        assert!(!page.contains("signed in"), "{page}");
    }

    #[test]
    fn removal_revokes_a_google_grant_and_has_nothing_to_send_microsoft() {
        assert_eq!(
            revoke_request(Provider::Google, "1//0g-x/y+z"),
            Some(("https://oauth2.googleapis.com/revoke", "token=1%2F%2F0g-x%2Fy%2Bz".to_string()))
        );
        assert_eq!(revoke_request(Provider::Microsoft, "token"), None);
    }

    #[test]
    fn debug_output_never_shows_a_token_or_the_client_secret() {
        let client = Client { id: "client-id".into(), secret: Some("client-secret-value".into()) };
        let signed = SignedIn {
            email: "me@example.com".into(),
            name: "Me".into(),
            refresh_token: "refresh-token-value".into(),
            access_token: "access-token-value".into(),
            expires_in: 3600,
            user_id: Some("user-1".into()),
        };
        let text = format!("{client:?} {client:#?} {signed:?} {signed:#?} {:?}", Consent::Granted(signed.clone()));
        for secret in ["client-secret-value", "refresh-token-value", "access-token-value"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(text.contains("client-id") && text.contains("me@example.com"), "the rest still shows: {text}");
    }

    fn params(url: &str) -> HashMap<String, String> {
        query_of(url)
    }

    #[test]
    fn google_calendar_consent_asks_for_mail_and_calendar() {
        let url = authorize_url(Provider::Google, "cid", "http://127.0.0.1:8765/", "verifier", "st", Ask::Calendar, Some("me@example.com"));
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
        let p = params(&url);
        assert_eq!(
            p["scope"],
            "openid email profile https://mail.google.com/ https://www.googleapis.com/auth/calendar.events \
             https://www.googleapis.com/auth/calendar.calendarlist.readonly https://www.googleapis.com/auth/calendar.freebusy",
            "an account without a mail grant for this client gets both in one sign-in"
        );
        assert_eq!(p["include_granted_scopes"], "true");
        assert_eq!(p["login_hint"], "me@example.com");
        assert_eq!(p["access_type"], "offline");
        assert_eq!(p["code_challenge_method"], "S256");
        assert_eq!(p["code_challenge"], URL_SAFE_NO_PAD.encode(Sha256::digest(b"verifier")));
        assert_eq!(p["redirect_uri"], "http://127.0.0.1:8765/");
        let add = params(&authorize_url(Provider::Google, "cid", "http://127.0.0.1:1/", "v", "st", Ask::Account, None));
        assert_eq!(add["scope"], "openid email profile https://mail.google.com/", "adding an account asks for mail only");
        assert_eq!(add["include_granted_scopes"], "true", "signing in again keeps a calendar grant");
        assert!(!add.contains_key("login_hint"));
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

    fn data_folder(name: &str) -> crate::Jolta {
        crate::secrets::use_memory_store();
        let dir = std::env::temp_dir().join(format!("jolta-oauth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::Jolta::open(&dir).unwrap()
    }

    /// A token reply whose id_token carries `claims`, as JSON.
    fn signed(provider: Provider, claims: &str) -> SignedIn {
        let id_token = format!("x.{}.sig", URL_SAFE_NO_PAD.encode(claims));
        let reply = serde_json::json!({"refresh_token": "r", "access_token": "a", "expires_in": 3600, "id_token": id_token});
        SignedIn::from_tokens(provider, &reply).unwrap()
    }

    fn google(address: &str, sub: &str) -> SignedIn {
        signed(Provider::Google, &format!(r#"{{"email":"{address}","name":"Ann","sub":"{sub}"}}"#))
    }

    fn microsoft(address: &str, oid: &str) -> SignedIn {
        signed(Provider::Microsoft, &format!(r#"{{"preferred_username":"{address}","name":"Ann","oid":"{oid}"}}"#))
    }

    fn inbox() -> crate::store::RemoteMailbox {
        crate::store::RemoteMailbox {
            important: false,
            remote_id: "INBOX".into(),
            name: "Inbox".into(),
            role: Some(crate::model::MailboxRole::Inbox),
            delimiter: None,
            selectable: true,
        }
    }

    /// The provider renamed the user, so the address changed and the
    /// account needed a new sign-in. Signing in again is the same account:
    /// the provider's id for the user says so, not the address.
    #[test]
    fn signing_in_again_after_a_rename_keeps_the_account_and_takes_the_new_address() {
        for (provider, sign_in) in [(Provider::Google, google as fn(&str, &str) -> SignedIn), (Provider::Microsoft, microsoft)] {
            let j = data_folder(&format!("rename-{provider:?}"));
            let added = save_account(&j, provider, &sign_in("old@example.com", "user-1"), None).unwrap();
            j.store.write(|tx| crate::store::set_signature(tx, added.id, "Ann, from old")).unwrap();
            j.store.write(|tx| crate::store::sync_mailbox_list(tx, added.id, &[inbox()])).unwrap();

            let again = save_account(&j, provider, &sign_in("new@example.com", "user-1"), Some(added.id)).unwrap();

            let accounts = j.store.read(crate::store::accounts).unwrap();
            assert_eq!(accounts.len(), 1, "{provider:?}: no second account");
            assert_eq!(again.id, added.id, "{provider:?}: the same account");
            assert_eq!((accounts[0].address.as_str(), accounts[0].username.as_str()), ("new@example.com", "new@example.com"));
            assert_eq!(accounts[0].signature, "Ann, from old", "{provider:?}: settings stay");
            assert_eq!(j.store.read(|c| crate::store::mailboxes(c, added.id)).unwrap().len(), 1, "{provider:?}: mail stays");

            let from_add = save_account(&j, provider, &sign_in("newer@example.com", "user-1"), None).unwrap();
            assert_eq!(from_add.id, added.id, "{provider:?}: adding the account again finds it by the provider's id too");
        }
    }

    /// An account added before Jolta kept the provider's id, then renamed:
    /// Sign In Again in its settings repairs it and keeps the id from then on.
    #[test]
    fn signing_in_again_repairs_a_renamed_account_jolta_had_no_id_for() {
        let j = data_folder("rename-legacy");
        let legacy = crate::store::NewAccount {
            kind: AccountKind::Gmail,
            address: "old@example.com".into(),
            display_name: "Ann".into(),
            username: "old@example.com".into(),
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 465,
        };
        let legacy = j.store.write(|tx| crate::store::insert_account(tx, &legacy)).unwrap();

        let again = save_account(&j, Provider::Google, &google("new@example.com", "user-1"), Some(legacy.id)).unwrap();
        assert_eq!(again.id, legacy.id);
        assert_eq!(again.address, "new@example.com");

        let later = save_account(&j, Provider::Google, &google("newer@example.com", "user-1"), None).unwrap();
        assert_eq!(later.id, legacy.id, "the id is kept from the repair on");
        assert_eq!(j.store.read(crate::store::accounts).unwrap().len(), 1);
    }

    /// Someone else's sign-in never takes over an account: not from its Sign
    /// In Again, and not when the provider gave its old address to a new user.
    #[test]
    fn another_user_gets_an_account_of_their_own() {
        let j = data_folder("rename-other");
        let ann = save_account(&j, Provider::Google, &google("ann@example.com", "user-1"), None).unwrap();

        let bob = save_account(&j, Provider::Google, &google("bob@example.com", "user-2"), Some(ann.id)).unwrap();
        assert_ne!(bob.id, ann.id, "Sign In Again as someone else adds them");

        let new_ann = save_account(&j, Provider::Google, &google("ann@example.com", "user-3"), None).unwrap();
        assert_ne!(new_ann.id, ann.id, "a new user with Ann's old address is someone else");

        let ann = j.store.read(|c| crate::store::account(c, ann.id)).unwrap();
        assert_eq!(ann.address, "ann@example.com", "Ann's account is untouched");
    }
}

/// Stores the refresh token of a calendar sign-in for the account it was
/// started for, in the same Keychain entry, replacing the mail-only one.
pub fn save_grant(account: &Account, signed: &SignedIn) -> Result<()> {
    secrets::set_password(&account.secret_ref, &signed.refresh_token)?;
    remember(&account.secret_ref, &signed.access_token, signed.expires_in);
    Ok(())
}

/// Stores a finished sign-in as an account, or as the account it signed in
/// to again, and returns it. `again` is the account whose Sign In Again
/// started it.
pub fn save_account(jolta: &crate::Jolta, provider: Provider, signed: &SignedIn, again: Option<crate::AccountId>) -> Result<Account> {
    let kind = match provider {
        Provider::Google => AccountKind::Gmail,
        Provider::Microsoft => AccountKind::Graph,
    };
    let account = jolta.store.write(|tx| {
        let accounts = crate::store::accounts(tx)?;
        let id = match signed_in_to(&accounts, kind, signed, again) {
            Some(a) => a.id,
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
                crate::store::insert_account(tx, &new)?.id
            }
        };
        crate::store::set_signed_in_as(tx, id, &signed.email, signed.user_id.as_deref())
    })?;
    secrets::set_password(&account.secret_ref, &signed.refresh_token)?;
    remember(&account.secret_ref, &signed.access_token, signed.expires_in);
    Ok(account)
}

/// The account a sign-in belongs to: the one with the provider's id for
/// the user, which a renamed address keeps. An account that has no id yet,
/// signed in before Jolta kept them, is the one whose Sign In Again this
/// is, else the one at the same address.
fn signed_in_to<'a>(accounts: &'a [Account], kind: AccountKind, signed: &SignedIn, again: Option<crate::AccountId>) -> Option<&'a Account> {
    let mut of_kind = accounts.iter().filter(|a| a.kind == kind);
    if let Some(user) = &signed.user_id
        && let Some(a) = of_kind.clone().find(|a| a.oauth_user_id.as_ref() == Some(user))
    {
        return Some(a);
    }
    let without_id = |a: &&Account| a.oauth_user_id.is_none() || signed.user_id.is_none();
    of_kind
        .clone()
        .filter(without_id)
        .find(|a| Some(a.id) == again)
        .or_else(|| of_kind.find(|a| without_id(a) && a.address.eq_ignore_ascii_case(&signed.email)))
}
