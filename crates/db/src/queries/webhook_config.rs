use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent Webhook Config
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWebhookConfigRow {
    pub scope: String,     // 'agent' | 'tool' | 'global'
    pub agent_id: String,  // '*' for global
    pub tool_name: String, // '' when unused
    pub kind: String,      // 'subtask' | 'tool' | 'lifecycle'
    pub url: String,
    pub events_json: Option<String>,
    pub enabled: bool,
    pub name: Option<String>,
    pub created_by: Option<String>,
    pub output_mode: String,
    pub max_chars: i64,
    pub updated_at: String,
}

pub fn get_agent_webhook_config(
    conn: &Connection,
    scope: &str,
    agent_id: &str,
    tool_name: &str,
    kind: &str,
) -> Result<Option<AgentWebhookConfigRow>> {
    let result = conn.query_row(
        "SELECT scope, agent_id, tool_name, kind, url, events_json, enabled, name, created_by, output_mode, max_chars, updated_at
         FROM agent_webhook_config
         WHERE scope = ?1 AND agent_id = ?2 AND tool_name = ?3 AND kind = ?4",
        params![scope, agent_id, tool_name, kind],
        |row| {
            Ok(AgentWebhookConfigRow {
                scope: row.get(0)?,
                agent_id: row.get(1)?,
                tool_name: row.get(2)?,
                kind: row.get(3)?,
                url: row.get(4)?,
                events_json: row.get(5)?,
                enabled: row.get(6)?,
                name: row.get(7)?,
                created_by: row.get(8)?,
                output_mode: row.get(9)?,
                max_chars: row.get(10)?,
                updated_at: row.get(11)?,
            })
        },
    );

    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_agent_webhook_config(conn: &Connection, row: &AgentWebhookConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_webhook_config
            (scope, agent_id, tool_name, kind, url, events_json, enabled, name, created_by, output_mode, max_chars, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(scope, agent_id, tool_name, kind) DO UPDATE SET
            url = excluded.url,
            events_json = excluded.events_json,
            enabled = excluded.enabled,
            name = excluded.name,
            created_by = excluded.created_by,
            output_mode = excluded.output_mode,
            max_chars = excluded.max_chars,
            updated_at = excluded.updated_at",
        params![
            row.scope,
            row.agent_id,
            row.tool_name,
            row.kind,
            row.url,
            row.events_json,
            row.enabled,
            row.name,
            row.created_by,
            row.output_mode,
            row.max_chars,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn list_agent_webhook_config(
    conn: &Connection,
    agent_id: Option<&str>,
    include_disabled: bool,
) -> Result<Vec<AgentWebhookConfigRow>> {
    let mut sql = String::from(
        "SELECT scope, agent_id, tool_name, kind, url, events_json, enabled, name, created_by, output_mode, max_chars, updated_at
         FROM agent_webhook_config WHERE 1 = 1",
    );
    if agent_id.is_some() {
        sql.push_str(" AND (agent_id = ?1 OR agent_id = '*')");
    }
    if !include_disabled {
        sql.push_str(" AND enabled = 1");
    }
    sql.push_str(" ORDER BY scope, agent_id, tool_name, kind");

    let mut stmt = conn.prepare(&sql)?;
    let map_row = |row: &rusqlite::Row| {
        Ok(AgentWebhookConfigRow {
            scope: row.get(0)?,
            agent_id: row.get(1)?,
            tool_name: row.get(2)?,
            kind: row.get(3)?,
            url: row.get(4)?,
            events_json: row.get(5)?,
            enabled: row.get(6)?,
            name: row.get(7)?,
            created_by: row.get(8)?,
            output_mode: row.get(9)?,
            max_chars: row.get(10)?,
            updated_at: row.get(11)?,
        })
    };
    let rows = match agent_id {
        Some(a) => stmt
            .query_map(params![a], map_row)?
            .collect::<std::result::Result<_, _>>()?,
        None => stmt
            .query_map([], map_row)?
            .collect::<std::result::Result<_, _>>()?,
    };
    Ok(rows)
}
