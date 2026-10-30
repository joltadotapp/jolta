use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::model::AccountId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Grant {
    Read,
    Organize,
    Draft,
}

impl Grant {
    pub fn as_str(self) -> &'static str {
        match self {
            Grant::Read => "read",
            Grant::Organize => "organize",
            Grant::Draft => "draft",
        }
    }

    pub fn parse(s: &str) -> Option<Grant> {
        match s {
            "read" => Some(Grant::Read),
            "organize" => Some(Grant::Organize),
            "draft" => Some(Grant::Draft),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    Pending,
    Approved,
    Denied,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub id: i64,
    pub name: String,
    pub state: AgentState,
    pub grants: Vec<Grant>,
    pub last_seen: i64,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Finds the agent by its MCP client name, adding it as pending if new.
/// Returns its id and whether it is new.
pub fn agent_seen(conn: &Connection, name: &str) -> Result<(i64, bool)> {
    let existing: Option<i64> = conn.query_row("SELECT id FROM agent WHERE name = ?1", [name], |r| r.get(0)).optional()?;
    if let Some(id) = existing {
        conn.execute("UPDATE agent SET last_seen = ?2 WHERE id = ?1", params![id, now()])?;
        return Ok((id, false));
    }
    conn.execute("INSERT INTO agent (name, first_seen, last_seen) VALUES (?1, ?2, ?2)", params![name, now()])?;
    Ok((conn.last_insert_rowid(), true))
}

pub fn agent_state(conn: &Connection, id: i64) -> Result<AgentState> {
    let state: String = conn.query_row("SELECT state FROM agent WHERE id = ?1", [id], |r| r.get(0))?;
    Ok(match state.as_str() {
        "approved" => AgentState::Approved,
        "denied" => AgentState::Denied,
        _ => AgentState::Pending,
    })
}

pub fn has_grant(conn: &Connection, agent: i64, account: AccountId, grant: Grant) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM agent_grant WHERE agent_id = ?1 AND account_id = ?2 AND class = ?3)",
        params![agent, account, grant.as_str()],
        |r| r.get(0),
    )?)
}

/// Approves (with these grants on every account) or denies an agent.
pub fn decide_agent(conn: &Connection, agent: i64, approve: bool, grants: &[Grant]) -> Result<()> {
    conn.execute("UPDATE agent SET state = ?2 WHERE id = ?1", params![agent, if approve { "approved" } else { "denied" }])?;
    conn.execute("DELETE FROM agent_grant WHERE agent_id = ?1", [agent])?;
    if approve {
        let accounts: Vec<i64> = conn.prepare("SELECT id FROM account")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for account in accounts {
            for g in grants {
                conn.execute(
                    "INSERT INTO agent_grant (agent_id, account_id, class) VALUES (?1, ?2, ?3)",
                    params![agent, account, g.as_str()],
                )?;
            }
        }
    }
    Ok(())
}

pub fn agents(conn: &Connection) -> Result<Vec<Agent>> {
    let mut stmt = conn.prepare("SELECT id, name, state, last_seen FROM agent ORDER BY last_seen DESC")?;
    let rows: Vec<(i64, String, String, i64)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
    rows.into_iter()
        .map(|(id, name, state, last_seen)| {
            let grants = conn
                .prepare("SELECT DISTINCT class FROM agent_grant WHERE agent_id = ?1")?
                .query_map([id], |r| r.get::<_, String>(0))?
                .filter_map(|g| g.ok().and_then(|g| Grant::parse(&g)))
                .collect();
            let state = match state.as_str() {
                "approved" => AgentState::Approved,
                "denied" => AgentState::Denied,
                _ => AgentState::Pending,
            };
            Ok(Agent { id, name, state, grants, last_seen })
        })
        .collect()
}

pub fn record_audit(conn: &Connection, agent: Option<i64>, tool: &str, summary: &str, outcome: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO audit (agent_id, tool, summary, outcome, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![agent, tool, summary, outcome, now()],
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub agent: String,
    pub tool: String,
    pub summary: String,
    pub outcome: String,
    pub at: i64,
}

pub fn audit_log(conn: &Connection, limit: usize) -> Result<Vec<AuditEntry>> {
    let mut stmt = conn.prepare(
        "SELECT coalesce(a.name, '?'), u.tool, u.summary, u.outcome, u.at FROM audit u
         LEFT JOIN agent a ON a.id = u.agent_id ORDER BY u.id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(AuditEntry { agent: r.get(0)?, tool: r.get(1)?, summary: r.get(2)?, outcome: r.get(3)?, at: r.get(4)? })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn agents_paused(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row("SELECT value FROM setting WHERE key = 'agents_paused'", [], |r| r.get::<_, String>(0))
        .optional()?
        .is_some_and(|v| v == "1"))
}

pub fn set_agents_paused(conn: &Connection, paused: bool) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES ('agents_paused', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [if paused { "1" } else { "0" }],
    )?;
    Ok(())
}
