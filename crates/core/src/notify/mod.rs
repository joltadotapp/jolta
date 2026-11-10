//! New-mail notifications and the Dock badge. The core decides which mail
//! is worth a notification and words it; each shell only delivers.

pub mod planner;
mod service;

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

pub use planner::{Candidate, Reminder};
pub use service::Notices;

use crate::calendar::{Bounds, EventData, series, tz};
use crate::model::*;
use crate::search::NOTIFICATION;
use crate::{Result, compose, search};

/// New mail older than this is never announced: mail moved back into the
/// inbox keeps its old arrival time.
const MAX_AGE: i64 = 24 * 3600;
/// An invitation for a meeting starting this soon is urgent.
const MEETING_SOON: i64 = 4 * 3600;

/// SQL: the message `alias` is from one of the user's addresses.
fn from_me(alias: &str) -> String {
    format!("EXISTS (SELECT 1 FROM own_address a WHERE a.address = {alias}.from_addr COLLATE NOCASE)")
}

/// SQL: one of the user's addresses is a whole address in the list
/// `column` holds ("Ann <ann@x>, bob@y"). Brackets, quotes and spaces
/// become commas, so `ann@x` doesn't match `joann@x`.
fn mine_in(column: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM own_address a WHERE instr(
             ',' || lower(replace(replace(replace(replace({column}, '<', ','), '>', ','), ' ', ','), '\"', ',')) || ',',
             ',' || lower(a.address) || ',') > 0)"
    )
}

/// SQL over message `m`: the mail is important. A VIP wrote it; it is a
/// reply in a conversation the user wrote in, not from an automated sender;
/// someone the user has written to sent it To the user, and it is neither
/// a newsletter nor automated; it carries an invitation, change or
/// cancellation; or, with AI sorting on, the model says it needs a reply
/// and isn't a newsletter or a notification. The user's own mail never is.
/// The banners, the badge and `is:important` all use this.
pub fn important(conn: &Connection) -> Result<String> {
    let sorting = crate::ai::settings(conn)?.sorts_new_mail();
    let mut rules = vec![
        "EXISTS (SELECT 1 FROM vip v WHERE v.address = m.from_addr)".to_string(),
        format!(
            "(NOT {NOTIFICATION} AND EXISTS (SELECT 1 FROM message o WHERE o.thread_id = m.thread_id AND o.id != m.id AND {}))",
            from_me("o")
        ),
        format!(
            "(EXISTS (SELECT 1 FROM correspondent c WHERE c.address = m.from_addr) AND {}
              AND m.list_id IS NULL AND m.list_unsubscribe IS NULL AND NOT {NOTIFICATION})",
            mine_in("m.to_addrs")
        ),
        "EXISTS (SELECT 1 FROM invitation i WHERE i.message_id = m.id AND i.method IN ('REQUEST', 'CANCEL'))".to_string(),
    ];
    if sorting {
        rules.push(
            "EXISTS (SELECT 1 FROM ai_triage t WHERE t.message_id = m.id AND t.needs_reply = 1
                     AND t.category NOT IN ('newsletter', 'notification'))"
                .into(),
        );
    }
    Ok(format!("(NOT {} AND ({}))", from_me("m"), rules.join(" OR ")))
}

/// Records the recipients of a message from one of the user's addresses
/// as people the user has written to.
pub(crate) fn note_correspondents(conn: &Connection, from: &str, to: &str, cc: &str) -> Result<()> {
    let mine: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM own_address WHERE address = ?1 COLLATE NOCASE)", [from], |r| r.get(0))?;
    if !mine {
        return Ok(());
    }
    let mut insert = conn.prepare_cached("INSERT OR IGNORE INTO correspondent (address) VALUES (?1)")?;
    for (_, address) in compose::parse_addresses(&format!("{to}, {cc}")) {
        let address = address.trim();
        if address.contains('@') {
            insert.execute(params![address])?;
        }
    }
    Ok(())
}

/// Records everyone the stored mail from the user's addresses went to, for
/// mail stored before the table existed or before an alias was known.
pub fn fill_correspondents(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(&format!("SELECT m.from_addr, m.to_addrs, m.cc_addrs FROM message m WHERE {}", from_me("m")))?;
    let sent: Vec<(String, String, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    for (from, to, cc) in sent {
        note_correspondents(conn, &from, &to, &cc)?;
    }
    Ok(())
}

/// Which new inbox mail gets a notification while Jolta runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Important,
    All,
    Vips,
    Nothing,
}

/// What the Dock badge counts: unread inbox conversations, all of them or
/// the important ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Badge {
    All,
    #[default]
    Important,
    Nothing,
}

/// Settings › General › Notifications, kept in the `setting` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub scope: Scope,
    pub sound: bool,
    pub badge: Badge,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { scope: Scope::Important, sound: true, badge: Badge::Important }
    }
}

/// How loudly the shell delivers: macOS passive, active and Time Sensitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Quiet,
    Normal,
    Urgent,
}

/// What a click on a notification opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Thread(ThreadId),
    UnifiedInbox,
}

/// A notification's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Archive,
    MarkRead,
    Reply,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// Stable: "mail-<thread>", "mail-summary", "snooze-<thread>",
    /// "noreply-<thread>", "reminders". Showing an id again replaces it.
    pub id: String,
    /// Groups notifications in Notification Center.
    pub group: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub level: Level,
    pub sound: bool,
    pub open: Target,
    pub actions: Vec<Action>,
    /// None for a summary.
    pub account: Option<AccountId>,
    /// When the newest message behind it arrived.
    pub arrived: i64,
}

/// What the shell does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Show(Notification),
    Withdraw(Vec<String>),
    Badge(u32),
}

impl Settings {
    /// The settings in the store; the defaults until the user picks.
    pub fn read(conn: &Connection) -> Result<Settings> {
        let raw: Option<String> = conn.query_row("SELECT value FROM setting WHERE key = 'notify'", [], |r| r.get(0)).optional()?;
        let Some(v) = raw.and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok()) else { return Ok(Settings::default()) };
        let defaults = Settings::default();
        Ok(Settings {
            scope: match v["scope"].as_str() {
                Some("all") => Scope::All,
                Some("vips") => Scope::Vips,
                Some("none") => Scope::Nothing,
                Some("important") => Scope::Important,
                _ => defaults.scope,
            },
            sound: v["sound"].as_bool().unwrap_or(defaults.sound),
            badge: match v["badge"].as_str() {
                Some("all") => Badge::All,
                Some("none") => Badge::Nothing,
                Some("important") => Badge::Important,
                _ => defaults.badge,
            },
        })
    }

    pub fn save(&self, conn: &Connection) -> Result<()> {
        let scope = match self.scope {
            Scope::Important => "important",
            Scope::All => "all",
            Scope::Vips => "vips",
            Scope::Nothing => "none",
        };
        let badge = match self.badge {
            Badge::All => "all",
            Badge::Important => "important",
            Badge::Nothing => "none",
        };
        let value = json!({"scope": scope, "sound": self.sound, "badge": badge}).to_string();
        conn.execute("INSERT INTO setting (key, value) VALUES ('notify', ?1) ON CONFLICT (key) DO UPDATE SET value = ?1", [value])?;
        Ok(())
    }

    /// Moves the Mac app's old "New mail banners" value (`all`, `vips`,
    /// `none`) in, unless settings are stored already. Only VIPs and None
    /// were chosen on purpose and stay; Every new message was the old
    /// default and becomes Important mail, the default now. Returns whether
    /// it moved anything.
    pub fn migrate(conn: &Connection, old: &str) -> Result<bool> {
        let stored: bool = conn.query_row("SELECT EXISTS (SELECT 1 FROM setting WHERE key = 'notify')", [], |r| r.get(0))?;
        if stored {
            return Ok(false);
        }
        let scope = match old {
            "vips" => Scope::Vips,
            "none" => Scope::Nothing,
            _ => Scope::Important,
        };
        Settings { scope, ..Settings::default() }.save(conn)?;
        Ok(true)
    }
}

/// The Dock badge's number: unread inbox conversations, all of them or
/// those `is:important` finds.
pub fn badge_count(conn: &Connection, settings: &Settings) -> Result<u32> {
    let query = match settings.badge {
        Badge::Nothing => return Ok(0),
        Badge::All => "in:inbox is:unread",
        Badge::Important => "in:inbox is:unread is:important",
    };
    search::query_count(conn, &search::parse(query)?)
}

/// The messages among `ids` that may still be announced, as they are now:
/// unread, in an inbox, not from the user, and arrived within a day.
/// `sorting` says whether AI sorting is running, so mail it hasn't sorted
/// yet waits for it.
pub fn candidates(conn: &Connection, ids: &[MessageId], now: DateTime<Utc>, sorting: bool) -> Result<Vec<Candidate>> {
    let sql = format!(
        "SELECT m.thread_id, m.account_id, CASE m.from_name WHEN '' THEN m.from_addr ELSE m.from_name END, m.from_addr,
                m.subject, m.snippet, m.date, {important},
                EXISTS (SELECT 1 FROM vip v WHERE v.address = m.from_addr),
                {to_me}, {cc_me}, {NOTIFICATION},
                EXISTS (SELECT 1 FROM message f WHERE f.id = (SELECT min(id) FROM message WHERE thread_id = m.thread_id) AND {first_mine}),
                (SELECT i.timing FROM invitation i WHERE i.message_id = m.id AND i.method IN ('REQUEST', 'CANCEL') AND i.timing != '' LIMIT 1),
                EXISTS (SELECT 1 FROM ai_triage t WHERE t.message_id = m.id)
         FROM message m
         WHERE m.id = ?1 AND (m.flags & ?2) = 0 AND m.date >= ?3 AND NOT {mine}
           AND EXISTS (SELECT 1 FROM message_mailbox mm JOIN mailbox b ON b.id = mm.mailbox_id
                       WHERE mm.message_id = m.id AND b.role = 'inbox')",
        important = important(conn)?,
        to_me = mine_in("m.to_addrs"),
        cc_me = mine_in("m.cc_addrs"),
        first_mine = from_me("f"),
        mine = from_me("m"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut out = Vec::new();
    for &id in ids {
        let row = stmt
            .query_row(params![id, Flags::SEEN, now.timestamp() - MAX_AGE], |r| {
                let vip: bool = r.get(8)?;
                let (to_me, cc_me, automated, started): (bool, bool, bool, bool) = (r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?);
                let invitation: Option<String> = r.get(13)?;
                let important: bool = r.get(7)?;
                let sorted: bool = r.get(14)?;
                Ok(Candidate {
                    message: id,
                    thread: r.get(0)?,
                    account: r.get(1)?,
                    sender: r.get(2)?,
                    sender_address: r.get(3)?,
                    subject: r.get(4)?,
                    first_line: r.get(5)?,
                    arrived: r.get(6)?,
                    important,
                    vip,
                    urgent: (vip && (to_me || cc_me))
                        || (to_me && started && !automated)
                        || invitation.is_some_and(|t| meeting_soon(&t, now)),
                    sorting: sorting && !important && !sorted,
                })
            })
            .optional()?;
        out.extend(row);
    }
    Ok(out)
}

/// The messages among `ids` still unread in an inbox.
pub fn still_waiting(conn: &Connection, ids: &[MessageId]) -> Result<HashSet<MessageId>> {
    let list = serde_json::to_string(&ids.iter().map(|m| m.0).collect::<Vec<_>>()).expect("numbers");
    let mut stmt = conn.prepare_cached(
        "SELECT m.id FROM message m WHERE m.id IN (SELECT value FROM json_each(?1)) AND (m.flags & ?2) = 0
           AND EXISTS (SELECT 1 FROM message_mailbox mm JOIN mailbox b ON b.id = mm.mailbox_id
                       WHERE mm.message_id = m.id AND b.role = 'inbox')",
    )?;
    let ids = stmt.query_map(params![list, Flags::SEEN], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// An invitation's event, or for a series the occurrence it shows next,
/// starts within `MEETING_SOON`.
fn meeting_soon(timing: &str, now: DateTime<Utc>) -> bool {
    let Ok(event) = serde_json::from_str::<EventData>(timing) else { return false };
    let here = tz::here();
    let span = if event.recurrence.is_some() { series::shown_occurrence(&event, &[], now, here).map(|o| o.span) } else { Some(event.span) };
    match span.map(|s| s.bounds(here)) {
        Some(Bounds::Utc { start, .. }) => start >= now && start <= now + Duration::seconds(MEETING_SOON),
        _ => false,
    }
}

/// The conversation a notification's id names: "mail-12", "snooze-12", "noreply-12".
pub fn thread_of(id: &str) -> Option<ThreadId> {
    let (kind, thread) = id.split_once('-')?;
    matches!(kind, "mail" | "snooze" | "noreply").then(|| thread.parse().ok().map(ThreadId)).flatten()
}
