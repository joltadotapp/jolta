//! Microsoft 365 through Microsoft Graph.
//!
//! Folders sync with one delta query each, with immutable ids, so a moved
//! message keeps its id. Actions are PATCH (read, flag) and `move`; mail is
//! sent and drafted as the MIME Kite builds. Graph files sent mail in Sent
//! Items itself. New mail is found by polling the inbox's delta.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::imap::{Scope, SyncEvent, SyncReport};
use crate::model::*;
use crate::store::{self, OpKind, RemoteMailbox};
use crate::{Error, Kite, Result, mime, oauth};

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

    /// One request with the account's token. Throttling (429) and brief
    /// outages (503, 504) are retried after the server's Retry-After.
    async fn send(&self, method: reqwest::Method, url: &str, body: Option<(&str, Vec<u8>)>) -> Result<(reqwest::StatusCode, Vec<u8>)> {
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
            if matches!(status.as_u16(), 429 | 503 | 504) && attempt < 4 {
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
            if !status.is_success() {
                let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                let message = json["error"]["message"].as_str().map(str::to_string).unwrap_or_else(|| String::from_utf8_lossy(&bytes).into_owned());
                return Err(Error::Other(format!("Microsoft Graph {}: {message}", status.as_u16())));
            }
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

    pub async fn sync(&self, kite: &Kite, scope: &Scope, progress: &mut impl FnMut(SyncEvent)) -> Result<SyncReport> {
        let account_id = self.account.id;
        let mailboxes = match scope {
            Scope::All => {
                let remote = self.folders().await?;
                kite.store.write_async(move |tx| store::sync_mailbox_list(tx, account_id, &remote)).await?
            }
            Scope::Mailboxes(ids) => {
                let ids = ids.clone();
                kite.store
                    .read_async(move |c| Ok(store::mailboxes(c, account_id)?.into_iter().filter(|m| ids.contains(&m.id)).collect()))
                    .await?
            }
        };
        progress(SyncEvent::Mailboxes(mailboxes.len()));
        let mut report = SyncReport { mailboxes: mailboxes.len(), ..Default::default() };
        // Folders Kite created locally (a first Snoozed) have no Graph id yet.
        for m in mailboxes.iter().filter(|m| !m.remote_id.eq_ignore_ascii_case("Snoozed")) {
            if m.role == Some(MailboxRole::System) {
                // Outbox and chat history aren't mail to read; drop what an earlier sync fetched.
                let id = m.id;
                kite.store.write_async(move |tx| store::clear_mailbox(tx, id)).await?;
                continue;
            }
            self.sync_folder(kite, m, &mut report, progress).await?;
        }
        report.bodies = self.fetch_bodies(kite).await?;
        Ok(report)
    }

    async fn sync_folder(&self, kite: &Kite, mailbox: &Mailbox, report: &mut SyncReport, progress: &mut impl FnMut(SyncEvent)) -> Result<()> {
        let mailbox_id = mailbox.id;
        let cursor: Option<String> = kite
            .store
            .read_async(move |c| Ok(c.query_row("SELECT delta_link FROM mailbox WHERE id = ?1", [mailbox_id], |r| r.get(0))?))
            .await?;
        let mut next = Some(cursor.unwrap_or_else(|| format!("/me/mailFolders/{}/messages/delta?$select={SELECT}", mailbox.remote_id)));
        let (mut new, mut removed, mut changed) = (0, 0, 0);
        let mut delta_link = None;
        while let Some(url) = next {
            let page = match self.get(&url).await {
                Ok(p) => p,
                // An expired delta link (410 / syncStateNotFound) means start over.
                Err(Error::Other(m)) if m.contains(" 410") || m.contains("SyncState") => {
                    kite.store.write_async(move |tx| store::clear_mailbox(tx, mailbox_id)).await?;
                    Value::Object(Default::default())
                }
                Err(e) => return Err(e),
            };
            let items = page["value"].as_array().cloned().unwrap_or_default();
            let account_id = self.account.id;
            let (n, r, c, ids) = kite
                .store
                .write_async(move |tx| {
                    let (mut n, mut r, mut c) = (0, 0, 0);
                    let mut ids = Vec::new();
                    for item in &items {
                        let remote = item["id"].as_str().unwrap_or_default();
                        if item.get("@removed").is_some() {
                            let id: Option<MessageId> = tx
                                .query_row("SELECT id FROM message WHERE account_id = ?1 AND remote_id = ?2", rusqlite::params![account_id, remote], |r| r.get(0))
                                .ok();
                            if let Some(id) = id {
                                store::unlink_messages(tx, mailbox_id, &[id])?;
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
                    Ok((n, r, c, ids))
                })
                .await?;
            new += n;
            removed += r;
            changed += c;
            report.new_messages.extend(ids);
            if n > 0 {
                progress(SyncEvent::Headers { mailbox: mailbox.name.clone(), done: new, total: new });
            }
            next = page["@odata.nextLink"].as_str().map(str::to_string);
            delta_link = page["@odata.deltaLink"].as_str().map(str::to_string).or(delta_link);
        }
        if let Some(link) = delta_link {
            kite.store
                .write_async(move |tx| Ok(tx.execute("UPDATE mailbox SET delta_link = ?2 WHERE id = ?1", rusqlite::params![mailbox_id, link])?))
                .await?;
        }
        report.new += new;
        report.removed += removed;
        report.flag_changes += changed;
        progress(SyncEvent::Mailbox { name: mailbox.name.clone(), new, removed, flag_changes: changed });
        Ok(())
    }

    /// The message as MIME, for reading and the search index.
    pub async fn message_mime(&self, remote_id: &str) -> Result<Vec<u8>> {
        let (_, bytes) = self.send(reqwest::Method::GET, &format!("/me/messages/{remote_id}/$value"), None).await?;
        Ok(bytes)
    }

    /// Downloads bodies the store lacks, newest first, up to 200 per sync.
    async fn fetch_bodies(&self, kite: &Kite) -> Result<usize> {
        let account_id = self.account.id;
        let todo: Vec<(MessageId, String)> = kite
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
            crate::imap::store_body(kite, id, raw).await?;
            done += 1;
        }
        Ok(done)
    }

    async fn folder_id(&self, kite: &Kite, mailbox: MailboxId) -> Result<String> {
        let m = kite.store.read_async(move |c| Ok(c.query_row("SELECT remote_id, name FROM mailbox WHERE id = ?1", [mailbox], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?)).await?;
        if m.0.eq_ignore_ascii_case("Snoozed") {
            // Created locally by a first snooze: create it in Graph and record the real id.
            let created = self.post_json("/me/mailFolders", json!({"displayName": m.1})).await?;
            let id = created["id"].as_str().ok_or_else(|| Error::Other("Graph did not return the new folder's id".into()))?.to_string();
            let id2 = id.clone();
            kite.store.write_async(move |tx| Ok(tx.execute("UPDATE mailbox SET remote_id = ?2 WHERE id = ?1", rusqlite::params![mailbox, id2])?)).await?;
            return Ok(id);
        }
        Ok(m.0)
    }

    pub async fn apply_op(&self, kite: &Kite, op: &store::Op) -> Result<Vec<MailboxId>> {
        let ids: Vec<MessageId> = op.messages.iter().map(|(m, _)| *m).collect();
        let remotes: Vec<String> = kite
            .store
            .read_async(move |c| {
                ids.iter().map(|id| Ok(c.query_row("SELECT remote_id FROM message WHERE id = ?1", [id], |r| r.get(0))?)).collect()
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
            OpKind::Move { from, to } => {
                let destination = self.folder_id(kite, to).await?;
                for r in &remotes {
                    self.post_json(&format!("/me/messages/{r}/move"), json!({"destinationId": destination})).await?;
                }
                Ok(vec![from, to])
            }
        }
    }

    /// Sends a message Kite built; Graph keeps a copy in Sent Items.
    pub async fn send_mime(&self, raw: &[u8]) -> Result<()> {
        self.send(reqwest::Method::POST, "/me/sendMail", Some(("text/plain", STANDARD.encode(raw).into_bytes()))).await?;
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
        return format!("Your organization requires an administrator to approve Kite before you can sign in. Ask your IT admin to grant consent for Kite. ({message})");
    }
    message.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_graph_times() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_time("2026-10-30T12:27:04Z"), Some(1_793_363_224));
        assert_eq!(parse_time("2026-10-30T13:27:04.1234567+01:00"), Some(1_793_363_224));
    }

    #[test]
    fn explains_consent_errors() {
        assert!(explain_sign_in_error("microsoft said: AADSTS65001: The user or administrator has not consented").starts_with("Your organization"));
        assert_eq!(explain_sign_in_error("network down"), "network down");
    }
}
