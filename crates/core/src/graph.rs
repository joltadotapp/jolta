//! Microsoft 365 through Microsoft Graph.
//!
//! Folders sync with one delta query each, with immutable ids, so a moved
//! message keeps its id. Actions are PATCH (read, flag) and `move`; mail is
//! sent and drafted as the MIME Jolta builds. Graph files sent mail in Sent
//! Items itself. New mail is found by polling the inbox's delta.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::imap::{Scope, SyncEvent, SyncReport};
use crate::model::*;
use crate::store::{self, OpKind, RemoteMailbox};
use crate::{Error, Jolta, Result, mime, oauth};

const BASE: &str = "https://graph.microsoft.com/v1.0";
const SELECT: &str = "subject,from,toRecipients,ccRecipients,receivedDateTime,isRead,isDraft,flag,hasAttachments,\
internetMessageId,conversationId,bodyPreview";
const WELL_KNOWN: &[(&str, MailboxRole)] = &[
    ("inbox", MailboxRole::Inbox),
    ("sentitems", MailboxRole::Sent),
    ("drafts", MailboxRole::Drafts),
    ("archive", MailboxRole::Archive),
    ("deleteditems", MailboxRole::Trash),
    ("junkemail", MailboxRole::Spam),
    ("outbox", MailboxRole::System),
    ("conversationhistory", MailboxRole::System),
    ("syncissues", MailboxRole::System),
];

pub struct Graph {
    http: reqwest::Client,
    account: Account,
}

fn http_error(e: impl std::fmt::Display) -> Error {
    Error::Other(format!("Microsoft Graph: {e}"))
}

/// A failed sendMail: a refusal that sending again can't change is final.
fn send_failure(status: reqwest::StatusCode, bytes: &[u8]) -> Error {
    let json: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    match json["error"]["code"].as_str() {
        Some("ErrorSendAsDenied") => Error::Refused("the mailbox has no right to send as that address".into()),
        _ => failure(status, bytes),
    }
}

fn failure(status: reqwest::StatusCode, bytes: &[u8]) -> Error {
    let json: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let message = json["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned());
    Error::Other(format!("Microsoft Graph {}: {message}", status.as_u16()))
}

/// One page of a folder's delta, or Graph saying it no longer knows the
/// delta link. Exchange keeps only the last few generations of a link: a
/// second copy of the store that moved the same link on three times makes
/// this one's 410 SyncStateNotFound (seen 2026-11-08), and a link also
/// expires on its own.
enum DeltaPage {
    Page(Value),
    Forgotten,
}

trait DeltaSource {
    async fn delta_page(&self, url: &str) -> Result<DeltaPage>;
}

impl DeltaSource for Graph {
    async fn delta_page(&self, url: &str) -> Result<DeltaPage> {
        let (status, bytes) = self.respond(reqwest::Method::GET, url, None).await?;
        if status.is_success() {
            return Ok(DeltaPage::Page(serde_json::from_slice(&bytes).map_err(http_error)?));
        }
        let code = serde_json::from_slice::<Value>(&bytes).ok().and_then(|j| j["error"]["code"].as_str().map(str::to_ascii_lowercase));
        if status == reqwest::StatusCode::GONE || code.is_some_and(|c| c.starts_with("syncstate")) {
            return Ok(DeltaPage::Forgotten);
        }
        Err(failure(status, &bytes))
    }
}

/// "2026-10-30T12:27:04Z" (or with fractions or an offset) to unix seconds.
pub fn parse_time(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (clock, offset) = match time.find(['Z', '+', '-']) {
        Some(i) => time.split_at(i),
        None => (time, "Z"),
    };
    let mut c = clock.split(':');
    let hh: i64 = c.next()?.parse().ok()?;
    let mm: i64 = c.next()?.parse().ok()?;
    let ss: i64 = c.next().and_then(|s| s.split('.').next()).and_then(|s| s.parse().ok()).unwrap_or(0);
    let off = match offset {
        "Z" | "" => 0,
        o => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let (h, m) = o[1..].split_once(':').unwrap_or((&o[1..3], &o[3..]));
            sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().unwrap_or(0) * 60)
        }
    };
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss - off)
}

fn address(v: &Value) -> (String, String) {
    let e = &v["emailAddress"];
    (e["name"].as_str().unwrap_or_default().to_string(), e["address"].as_str().unwrap_or_default().to_string())
}

fn recipients(v: &Value) -> String {
    v.as_array()
        .map(|list| {
            list.iter()
                .map(address)
                .map(|(n, a)| if n.is_empty() || n == a { a } else { format!("{n} <{a}>") })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

impl Graph {
    pub fn new(account: &Account) -> Result<Graph> {
        oauth::init_tls();
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().map_err(http_error)?;
        Ok(Graph { http, account: account.clone() })
    }

    /// One request with the account's token, failing on any status but success.
    async fn send(&self, method: reqwest::Method, url: &str, body: Option<(&str, Vec<u8>)>) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let (status, bytes) = self.respond(method, url, body).await?;
        if !status.is_success() {
            return Err(failure(status, &bytes));
        }
        Ok((status, bytes))
    }

    /// One request with the account's token and its answer, whatever the
    /// status. Throttling (429) and brief outages (502, 503, 504) are
    /// retried after the server's Retry-After.
    async fn respond(&self, method: reqwest::Method, url: &str, body: Option<(&str, Vec<u8>)>) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let url = if url.starts_with("https://") { url.to_string() } else { format!("{BASE}{url}") };
        for attempt in 0..5 {
            let token = oauth::access_token(&self.account).await?;
            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&token)
                .header("Prefer", "IdType=\"ImmutableId\", odata.maxpagesize=100");
            if let Some((content_type, bytes)) = &body {
                req = req.header("Content-Type", *content_type).body(bytes.clone());
            }
            let resp = req.send().await.map_err(http_error)?;
            let status = resp.status();
            // Microsoft can revoke a token before it expires; a fresh one gets one more try.
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                oauth::forget(&self.account);
                continue;
            }
            if matches!(status.as_u16(), 429 | 502 | 503 | 504) && attempt < 4 {
                let wait = resp
                    .headers()
                    .get("Retry-After")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(2 << attempt);
                tokio::time::sleep(Duration::from_secs(wait.min(120))).await;
                continue;
            }
            let bytes = resp.bytes().await.map_err(http_error)?.to_vec();
            return Ok((status, bytes));
        }
        unreachable!("the last attempt returns")
    }

    pub async fn get(&self, url: &str) -> Result<Value> {
        let (_, bytes) = self.send(reqwest::Method::GET, url, None).await?;
        serde_json::from_slice(&bytes).map_err(http_error)
    }

    async fn post_json(&self, url: &str, body: Value) -> Result<Value> {
        let (_, bytes) = self.send(reqwest::Method::POST, url, Some(("application/json", body.to_string().into_bytes()))).await?;
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Checks the token and the mailbox: who is signed in.
    pub async fn whoami(&self) -> Result<String> {
        let me = self.get("/me?$select=mail,userPrincipalName").await?;
        Ok(me["mail"].as_str().or_else(|| me["userPrincipalName"].as_str()).unwrap_or_default().to_string())
    }

    async fn folders(&self) -> Result<Vec<RemoteMailbox>> {
        let mut roles: HashMap<String, MailboxRole> = HashMap::new();
        for (name, role) in WELL_KNOWN {
            if let Ok(f) = self.get(&format!("/me/mailFolders/{name}?$select=id")).await {
                if let Some(id) = f["id"].as_str() {
                    roles.insert(id.to_string(), *role);
                }
            }
        }
        let mut out = Vec::new();
        // A system folder's children (Sync Issues/Conflicts) are system folders too.
        let mut queue: Vec<(String, String, bool)> =
            vec![("/me/mailFolders?$top=100&$select=id,displayName,childFolderCount".into(), String::new(), false)];
        while let Some((url, prefix, in_system)) = queue.pop() {
            let mut next = Some(url);
            while let Some(u) = next {
                let page = self.get(&u).await?;
                for f in page["value"].as_array().cloned().unwrap_or_default() {
                    let id = f["id"].as_str().unwrap_or_default().to_string();
                    let name = format!("{prefix}{}", f["displayName"].as_str().unwrap_or_default());
                    let role = if in_system {
                        Some(MailboxRole::System)
                    } else {
                        roles.get(&id).copied().or_else(|| name.eq_ignore_ascii_case("Snoozed").then_some(MailboxRole::Snoozed))
                    };
                    if f["childFolderCount"].as_u64().unwrap_or(0) > 0 {
                        queue.push((
                            format!("/me/mailFolders/{id}/childFolders?$top=100&$select=id,displayName,childFolderCount"),
                            format!("{name}/"),
                            role == Some(MailboxRole::System),
                        ));
                    }
                    out.push(RemoteMailbox { important: false, remote_id: id, name, role, delimiter: Some("/".into()), selectable: true });
                }
                next = page["@odata.nextLink"].as_str().map(str::to_string);
            }
        }
        Ok(out)
    }

    pub async fn sync(&self, jolta: &Jolta, scope: &Scope, progress: &mut impl FnMut(SyncEvent)) -> Result<SyncReport> {
        let account_id = self.account.id;
        let mailboxes = match scope {
            Scope::All => {
                let remote = self.folders().await?;
                jolta.store.write_async(move |tx| store::sync_mailbox_list(tx, account_id, &remote)).await?
            }
            Scope::Mailboxes(ids) => {
                let ids = ids.clone();
                jolta.store
                    .read_async(move |c| Ok(store::mailboxes(c, account_id)?.into_iter().filter(|m| ids.contains(&m.id)).collect()))
                    .await?
            }
        };
        progress(SyncEvent::Mailboxes(mailboxes.len()));
        let mut report = SyncReport { mailboxes: mailboxes.len(), ..Default::default() };
        // Folders Jolta created locally (a first Snoozed) have no Graph id yet.
        let mut queue: Vec<Mailbox> = mailboxes.into_iter().filter(|m| !m.remote_id.eq_ignore_ascii_case("Snoozed")).collect();
        let mut unfiled = Vec::new();
        while let Some(m) = jolta.next_mailbox(&mut queue) {
            let m = &m;
            if m.role == Some(MailboxRole::System) {
                // Outbox and chat history aren't mail to read; drop what an earlier sync fetched.
                let id = m.id;
                jolta.store.write_async(move |tx| store::clear_mailbox(tx, id)).await?;
                continue;
            }
            sync_folder(self, jolta, m, &mut report, &mut unfiled, progress).await?;
        }
        jolta.store.write_async(move |tx| store::delete_unfiled(tx, &unfiled)).await?;
        report.bodies = self.fetch_bodies(jolta).await?;
        Ok(report)
    }

    /// The message as MIME, for reading and the search index.
    pub async fn message_mime(&self, remote_id: &str) -> Result<Vec<u8>> {
        let (_, bytes) = self.send(reqwest::Method::GET, &format!("/me/messages/{remote_id}/$value"), None).await?;
        Ok(bytes)
    }

    /// Downloads bodies the store lacks, newest first, up to 200 per sync.
    async fn fetch_bodies(&self, jolta: &Jolta) -> Result<usize> {
        let account_id = self.account.id;
        let todo: Vec<(MessageId, String)> = jolta
            .store
            .read_async(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, remote_id FROM message WHERE account_id = ?1 AND blob IS NULL ORDER BY id DESC LIMIT 200",
                )?;
                let rows = stmt.query_map([account_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<rusqlite::Result<_>>()?)
            })
            .await?;
        let mut done = 0;
        for (id, remote) in todo {
            let raw = self.message_mime(&remote).await?;
            crate::imap::store_body(jolta, id, raw).await?;
            done += 1;
        }
        Ok(done)
    }

    async fn folder_id(&self, jolta: &Jolta, mailbox: MailboxId) -> Result<String> {
        let m = jolta.store.read_async(move |c| Ok(c.query_row("SELECT remote_id, name FROM mailbox WHERE id = ?1", [mailbox], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?)).await?;
        if m.0.eq_ignore_ascii_case("Snoozed") {
            // Created locally by a first snooze: create it in Graph and record the real id.
            let created = self.post_json("/me/mailFolders", json!({"displayName": m.1})).await?;
            let id = created["id"].as_str().ok_or_else(|| Error::Other("Graph did not return the new folder's id".into()))?.to_string();
            let id2 = id.clone();
            jolta.store.write_async(move |tx| Ok(tx.execute("UPDATE mailbox SET remote_id = ?2 WHERE id = ?1", rusqlite::params![mailbox, id2])?)).await?;
            return Ok(id);
        }
        Ok(m.0)
    }

    pub async fn apply_op(&self, jolta: &Jolta, op: &store::Op) -> Result<Vec<MailboxId>> {
        if let OpKind::Empty { mailbox } = op.kind {
            self.empty_folder(jolta, mailbox).await?;
            return Ok(vec![mailbox]);
        }
        let ids: Vec<MessageId> = op.messages.iter().map(|(m, _)| *m).collect();
        // A message deleted here since the change was queued has nothing left to change.
        let remotes: Vec<String> = jolta
            .store
            .read_async(move |c| {
                let mut out = Vec::new();
                for id in &ids {
                    out.extend(c.query_row("SELECT remote_id FROM message WHERE id = ?1", [id], |r| r.get(0)).optional()?);
                }
                Ok(out)
            })
            .await?;
        match op.kind {
            OpKind::Flags { add, remove } => {
                let mut patch = serde_json::Map::new();
                for (bits, on) in [(add, true), (remove, false)] {
                    if bits & Flags::SEEN != 0 {
                        patch.insert("isRead".into(), json!(on));
                    }
                    if bits & Flags::FLAGGED != 0 {
                        patch.insert("flag".into(), json!({"flagStatus": if on { "flagged" } else { "notFlagged" }}));
                    }
                }
                if !patch.is_empty() {
                    let body = Value::Object(patch).to_string().into_bytes();
                    for r in &remotes {
                        self.send(reqwest::Method::PATCH, &format!("/me/messages/{r}"), Some(("application/json", body.clone()))).await?;
                    }
                }
                Ok(vec![])
            }
            OpKind::Empty { .. } => unreachable!("handled above"),
            OpKind::Move { from, to } => {
                let destination = self.folder_id(jolta, to).await?;
                for r in &remotes {
                    self.post_json(&format!("/me/messages/{r}/move"), json!({"destinationId": destination})).await?;
                }
                Ok(vec![from, to])
            }
        }
    }

    /// Deletes every message in the folder, as Outlook's Empty Folder does:
    /// Exchange keeps them in Recoverable Items for its retention period.
    async fn empty_folder(&self, jolta: &Jolta, mailbox: MailboxId) -> Result<()> {
        let folder = self.folder_id(jolta, mailbox).await?;
        let page = format!("/me/mailFolders/{folder}/messages?$select=id&$top=100");
        // Each pass deletes a page; a folder of 100,000 messages takes 1,000 passes.
        for _ in 0..1000 {
            let listed = self.get(&page).await?;
            let ids: Vec<&str> = listed["value"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str()).collect();
            if ids.is_empty() {
                return Ok(());
            }
            for id in ids {
                match self.send(reqwest::Method::DELETE, &format!("/me/messages/{id}"), None).await {
                    Err(Error::Other(e)) if e.starts_with("Microsoft Graph 404") => {}
                    other => {
                        other?;
                    }
                }
            }
        }
        Err(Error::Other("the folder still had messages after 100,000 deletions".into()))
    }

    /// Sends a message Jolta built; Graph keeps a copy in Sent Items.
    pub async fn send_mime(&self, raw: &[u8]) -> Result<()> {
        let (status, bytes) = self.respond(reqwest::Method::POST, "/me/sendMail", Some(("text/plain", STANDARD.encode(raw).into_bytes()))).await?;
        if !status.is_success() {
            return Err(send_failure(status, &bytes));
        }
        Ok(())
    }

    /// Replaces the server copy of a draft (found by its Message-ID).
    pub async fn save_draft(&self, raw: Option<&[u8]>, old_message_id: Option<&str>) -> Result<()> {
        if let Some(old) = old_message_id {
            let filter = format!("internetMessageId eq '<{}>'", old.replace('\'', "''"));
            let url = format!("/me/mailFolders/drafts/messages?$select=id&$filter={}", urlencode(&filter));
            if let Ok(found) = self.get(&url).await {
                for m in found["value"].as_array().cloned().unwrap_or_default() {
                    if let Some(id) = m["id"].as_str() {
                        self.send(reqwest::Method::DELETE, &format!("/me/messages/{id}"), None).await.ok();
                    }
                }
            }
        }
        if let Some(raw) = raw {
            self.send(reqwest::Method::POST, "/me/messages", Some(("text/plain", STANDARD.encode(raw).into_bytes()))).await?;
        }
        Ok(())
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Makes admin-consent failures readable: Entra's AADSTS codes name the
/// tenant policy that blocked the sign-in.
pub fn explain_sign_in_error(message: &str) -> String {
    if message.contains("AADSTS65001") || message.contains("AADSTS90094") || message.contains("AADSTS90095") || message.contains("admin") && message.contains("consent") {
        return format!("Your organization requires an administrator to approve Jolta before you can sign in. Ask your IT admin to grant consent for Jolta. ({message})");
    }
    message.to_string()
}

async fn sync_folder(
    api: &impl DeltaSource,
    jolta: &Jolta,
    mailbox: &Mailbox,
    report: &mut SyncReport,
    unfiled: &mut Vec<MessageId>,
    progress: &mut impl FnMut(SyncEvent),
) -> Result<()> {
    let (mailbox_id, account_id) = (mailbox.id, mailbox.account_id);
    let cursor: Option<String> = jolta
        .store
        .read_async(move |c| Ok(c.query_row("SELECT delta_link FROM mailbox WHERE id = ?1", [mailbox_id], |r| r.get(0))?))
        .await?;
    let start = format!("/me/mailFolders/{}/messages/delta?$select={SELECT}", mailbox.remote_id);
    // A round from `start` lists every message in the folder, so the ones it
    // leaves out have gone; a round from a delta link lists only changes.
    let mut listed: Option<HashSet<String>> = cursor.is_none().then(HashSet::new);
    let mut next = Some(cursor.unwrap_or_else(|| start.clone()));
    let (mut new, mut removed, mut changed) = (0, 0, 0);
    let mut delta_link = None;
    while let Some(url) = next.take() {
        let page = match api.delta_page(&url).await? {
            DeltaPage::Page(p) => p,
            DeltaPage::Forgotten if listed.is_none() => {
                tracing::info!("Graph forgot the delta link of {}; listing the folder again", mailbox.name);
                listed = Some(HashSet::new());
                delta_link = None;
                next = Some(start.clone());
                continue;
            }
            DeltaPage::Forgotten => return Err(Error::Other(format!("Microsoft Graph lost its place listing {}", mailbox.name))),
        };
        let items = page["value"].as_array().cloned().unwrap_or_default();
        if let Some(listed) = &mut listed {
            listed.extend(items.iter().filter(|i| i.get("@removed").is_none()).filter_map(|i| i["id"].as_str().map(str::to_string)));
        }
        let delta = jolta.store.write_async(move |tx| apply_delta(tx, account_id, mailbox_id, &items)).await?;
        let n = delta.new;
        new += n;
        removed += delta.removed;
        changed += delta.changed;
        report.new_messages.extend(delta.new_ids);
        unfiled.extend(delta.unfiled);
        if n > 0 {
            progress(SyncEvent::Headers { mailbox: mailbox.name.clone(), done: new, total: new });
        }
        next = page["@odata.nextLink"].as_str().map(str::to_string);
        delta_link = page["@odata.deltaLink"].as_str().map(str::to_string).or(delta_link);
    }
    if let Some(listed) = listed {
        let gone = jolta.store.write_async(move |tx| unlink_unlisted(tx, account_id, mailbox_id, &listed)).await?;
        removed += gone.len();
        unfiled.extend(gone);
    }
    if let Some(link) = delta_link {
        jolta.store
            .write_async(move |tx| Ok(tx.execute("UPDATE mailbox SET delta_link = ?2 WHERE id = ?1", rusqlite::params![mailbox_id, link])?))
            .await?;
    }
    report.new += new;
    report.removed += removed;
    report.flag_changes += changed;
    progress(SyncEvent::Mailbox { name: mailbox.name.clone(), new, removed, flag_changes: changed });
    Ok(())
}

/// Takes out of the folder the messages a full listing of it left out,
/// except those with a change still queued. Returns their ids, which `sync`
/// deletes if no other folder has them.
fn unlink_unlisted(tx: &rusqlite::Connection, account_id: AccountId, mailbox_id: MailboxId, listed: &HashSet<String>) -> Result<Vec<MessageId>> {
    let pending = store::pending_messages(tx, account_id)?;
    let here: Vec<(MessageId, String)> = tx
        .prepare("SELECT m.id, m.remote_id FROM message m JOIN message_mailbox mm ON mm.message_id = m.id WHERE mm.mailbox_id = ?1")?
        .query_map([mailbox_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let gone: Vec<MessageId> = here.into_iter().filter(|(id, remote)| !listed.contains(remote) && !pending.contains(id)).map(|(id, _)| id).collect();
    for id in &gone {
        tx.execute("DELETE FROM message_mailbox WHERE mailbox_id = ?1 AND message_id = ?2", rusqlite::params![mailbox_id, id])?;
    }
    Ok(gone)
}

/// Applies one page of a folder's delta to the store. Returns the new,
/// removed and changed counts, the ids of the new messages, and the ids taken
/// out of this folder, which `sync` deletes if no other folder adds them.
fn apply_delta(tx: &rusqlite::Connection, account_id: AccountId, mailbox_id: MailboxId, items: &[Value]) -> Result<Delta> {
    let pending = store::pending_messages(tx, account_id)?;
    let (mut n, mut r, mut c) = (0, 0, 0);
    let (mut ids, mut unfiled) = (Vec::new(), Vec::new());
    for item in items {
        let remote = item["id"].as_str().unwrap_or_default();
        let known: Option<MessageId> = tx
            .query_row("SELECT id FROM message WHERE account_id = ?1 AND remote_id = ?2", rusqlite::params![account_id, remote], |r| r.get(0))
            .optional()?;
        // The page may predate a change still queued for this message, such as
        // an undo made while the sync ran: applying it would put the old state
        // back, or delete the row the queued change points at.
        if known.is_some_and(|id| pending.contains(&id)) {
            continue;
        }
        if item.get("@removed").is_some() {
            if let Some(id) = known {
                tx.execute("DELETE FROM message_mailbox WHERE mailbox_id = ?1 AND message_id = ?2", rusqlite::params![mailbox_id, id])?;
                unfiled.push(id);
                r += 1;
            }
            continue;
        }
        let (from_name, from_addr) = address(&item["from"]);
        let flags = Flags::default()
            .with(Flags::SEEN, item["isRead"].as_bool().unwrap_or(false))
            .with(Flags::FLAGGED, item["flag"]["flagStatus"].as_str() == Some("flagged"))
            .with(Flags::DRAFT, item["isDraft"].as_bool().unwrap_or(false));
        let message = NewMessage {
            account_id,
            remote_id: remote.to_string(),
            mailbox_id,
            uid: None,
            date: item["receivedDateTime"].as_str().and_then(parse_time).unwrap_or(0),
            envelope: Envelope {
                message_id: item["internetMessageId"].as_str().map(|s| s.trim_matches(['<', '>']).to_string()),
                from_name,
                from_addr,
                to: recipients(&item["toRecipients"]),
                cc: recipients(&item["ccRecipients"]),
                subject: item["subject"].as_str().unwrap_or_default().to_string(),
                has_attachments: item["hasAttachments"].as_bool().unwrap_or(false),
                ..Default::default()
            },
            flags,
            size: 0,
            thread_hint: item["conversationId"].as_str().map(|c| format!("ms:{c}")),
            body: None,
        };
        match store::insert_message(tx, &message)? {
            store::Inserted::New(id) => {
                let preview = mime::snippet(item["bodyPreview"].as_str().unwrap_or_default());
                tx.execute("UPDATE message SET snippet = ?2 WHERE id = ?1", rusqlite::params![id, preview])?;
                ids.push(id);
                n += 1;
            }
            store::Inserted::Existing(_) => c += 1,
        }
    }
    Ok(Delta { new: n, removed: r, changed: c, new_ids: ids, unfiled })
}

struct Delta {
    new: usize,
    removed: usize,
    changed: usize,
    new_ids: Vec<MessageId>,
    unfiled: Vec<MessageId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exchange refuses a From the mailbox may not send as with 403
    /// ErrorSendAsDenied; sending it again can only fail again.
    #[test]
    fn a_send_as_refusal_is_final() {
        let body = br#"{"error":{"code":"ErrorSendAsDenied","message":"The user account which was used to submit this request does not have the right to send mail on behalf of the specified sending account., Cannot submit message."}}"#;
        assert!(matches!(send_failure(reqwest::StatusCode::FORBIDDEN, body), Error::Refused(_)));
        let throttled = br#"{"error":{"code":"ErrorServerBusy","message":"busy"}}"#;
        assert!(matches!(send_failure(reqwest::StatusCode::SERVICE_UNAVAILABLE, throttled), Error::Other(_)), "anything else is tried again");
    }

    #[test]
    fn parses_graph_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-10-30T12:27:04Z"), Some(1_793_363_224));
        assert_eq!(parse_time("2026-10-30T13:27:04.1234567+01:00"), Some(1_793_363_224));
    }

    /// A delta page fetched before an undo, applied after it, must not put
    /// the old state back or delete the row the undo points at. Found by
    /// scripts/live-check.sh on 2026-11-02: the undo then failed forever.
    #[test]
    fn delta_pages_leave_queued_changes_alone() {
        let dir = std::env::temp_dir().join(format!("jolta-graph-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = store::Store::open(&dir.join("jolta.sqlite")).unwrap();
        let (inbox, archive) = (MailboxId(1), MailboxId(2));
        let row = |c: &rusqlite::Connection| -> rusqlite::Result<Vec<i64>> {
            c.prepare("SELECT mailbox_id FROM message_mailbox WHERE message_id = 7 ORDER BY 1")?.query_map([], |r| r.get(0))?.collect()
        };
        store
            .write(|tx| {
                tx.execute_batch(
                    "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'graph', 'a@x', 'a@x', 'r', 0);
                     INSERT INTO mailbox (id, account_id, remote_id, name, role) VALUES (1, 1, 'in', 'Inbox', 'inbox'), (2, 1, 'ar', 'Archive', 'archive');
                     INSERT INTO thread (id, account_id) VALUES (1, 1);
                     INSERT INTO message (id, account_id, remote_id, thread_id, date, flags) VALUES (7, 1, 'msg', 1, 0, 0);
                     INSERT INTO message_mailbox (mailbox_id, message_id) VALUES (1, 7);",
                )?;
                // The user undid an archive: the move back to the inbox is queued.
                let changed = store::apply_local(tx, &OpKind::Move { from: archive, to: inbox }, &[MessageId(7)])?;
                assert!(changed.is_empty(), "already in the inbox locally");
                store::insert_op(tx, AccountId(1), &OpKind::Move { from: archive, to: inbox }, &[(MessageId(7), None)])?;
                Ok(())
            })
            .unwrap();

        // Pages from the sync that followed the archive: gone from the inbox, added to the archive.
        let removed = vec![json!({"id": "msg", "@removed": {"reason": "deleted"}})];
        let added = vec![json!({"id": "msg", "subject": "Hi", "receivedDateTime": "2026-11-02T21:00:00Z", "isRead": true})];
        store.write(|tx| apply_delta(tx, AccountId(1), inbox, &removed)).unwrap();
        store.write(|tx| apply_delta(tx, AccountId(1), archive, &added)).unwrap();
        assert_eq!(store.read(|c| Ok(row(c)?)).unwrap(), [1], "the message stays in the inbox, with its row");

        // Once the change has reached the server, the next pages apply as usual.
        store.write(|tx| Ok(tx.execute("UPDATE op SET state = 'done'", [])?)).unwrap();
        store.write(|tx| apply_delta(tx, AccountId(1), inbox, &removed)).unwrap();
        store.write(|tx| apply_delta(tx, AccountId(1), archive, &added)).unwrap();
        assert_eq!(store.read(|c| Ok(row(c)?)).unwrap(), [2]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Graph's answers by URL; a URL it was not given is a link Graph forgot.
    struct Pages(HashMap<String, Value>, std::sync::Mutex<Vec<String>>);

    impl DeltaSource for Pages {
        async fn delta_page(&self, url: &str) -> Result<DeltaPage> {
            self.1.lock().unwrap().push(url.to_string());
            Ok(self.0.get(url).cloned().map_or(DeltaPage::Forgotten, DeltaPage::Page))
        }
    }

    fn mail(id: &str, subject: &str) -> Value {
        json!({"id": id, "subject": subject, "receivedDateTime": "2026-11-08T09:00:00Z", "isRead": false})
    }

    /// A delta link Graph has forgotten (410 SyncStateNotFound, after a copy
    /// of the store moved the same link on) starts the folder over in the
    /// same sync: what Graph lists stays or arrives, what it no longer lists
    /// leaves, and the new link is saved. Found on 2026-11-08: the old code
    /// emptied the folder and kept the dead link, so no mail arrived again.
    #[tokio::test]
    async fn a_forgotten_delta_link_starts_the_folder_over() {
        let dir = std::env::temp_dir().join(format!("jolta-graph-forgotten-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let jolta = Jolta::open(&dir).unwrap();
        let dead = "https://graph/dead";
        jolta
            .store
            .write(|tx| {
                tx.execute_batch(&format!(
                    "INSERT INTO account (id, kind, address, username, secret_ref, created) VALUES (1, 'graph', 'a@x', 'a@x', 'r', 0);
                     INSERT INTO mailbox (id, account_id, remote_id, name, role, delta_link) VALUES (1, 1, 'in', 'Inbox', 'inbox', '{dead}');
                     INSERT INTO thread (id, account_id) VALUES (1, 1);
                     INSERT INTO message (id, account_id, remote_id, thread_id, date, flags, blob) VALUES (7, 1, 'kept', 1, 0, 0, 'body7'), (8, 1, 'gone', 1, 0, 0, NULL);
                     INSERT INTO message_mailbox (mailbox_id, message_id) VALUES (1, 7), (1, 8);"
                ))?;
                Ok(())
            })
            .unwrap();
        let start = format!("/me/mailFolders/in/messages/delta?$select={SELECT}");
        let graph = Pages(
            HashMap::from([
                (start.clone(), json!({"value": [mail("kept", "Old")], "@odata.nextLink": "https://graph/next"})),
                ("https://graph/next".into(), json!({"value": [mail("fresh", "New")], "@odata.deltaLink": "https://graph/live"})),
                ("https://graph/live".into(), json!({"value": [], "@odata.deltaLink": "https://graph/live"})),
            ]),
            Default::default(),
        );
        let inbox = jolta.store.read(|c| store::mailboxes(c, AccountId(1))).unwrap().remove(0);
        let mut report = SyncReport::default();
        let mut unfiled = Vec::new();
        sync_folder(&graph, &jolta, &inbox, &mut report, &mut unfiled, &mut |_| {}).await.unwrap();
        jolta.store.write(|tx| store::delete_unfiled(tx, &unfiled)).unwrap();

        let listed = |c: &rusqlite::Connection| -> rusqlite::Result<Vec<(i64, String, Option<String>)>> {
            c.prepare("SELECT m.id, m.remote_id, m.blob FROM message m JOIN message_mailbox mm ON mm.message_id = m.id WHERE mm.mailbox_id = 1 ORDER BY m.remote_id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        };
        let rows = jolta.store.read(|c| Ok(listed(c)?)).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].1, "fresh");
        assert_eq!(rows[1], (7, "kept".into(), Some("body7".into())), "a message Graph still lists keeps its row and body");
        assert_eq!((report.new, report.removed), (1, 1));
        let saved: Option<String> = jolta.store.read(|c| Ok(c.query_row("SELECT delta_link FROM mailbox WHERE id = 1", [], |r| r.get(0))?)).unwrap();
        assert_eq!(saved.as_deref(), Some("https://graph/live"));

        // The next sync goes on from the new link.
        let mut report = SyncReport::default();
        sync_folder(&graph, &jolta, &inbox, &mut report, &mut Vec::new(), &mut |_| {}).await.unwrap();
        assert_eq!(graph.1.lock().unwrap().last().map(String::as_str), Some("https://graph/live"));
        assert_eq!(jolta.store.read(|c| Ok(listed(c)?)).unwrap().len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explains_consent_errors() {
        assert!(explain_sign_in_error("microsoft said: AADSTS65001: The user or administrator has not consented").starts_with("Your organization"));
        assert_eq!(explain_sign_in_error("network down"), "network down");
    }
}
