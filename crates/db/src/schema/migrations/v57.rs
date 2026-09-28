//! v57 (#612): fold time triggers into `agent_schedules`.
//!
//! Enabled `session_heartbeat_config` rows become `@every {secs}s` schedules whose `message` is the
//! instruction text resolved at migration time. Disabled rows are not moved. The old heartbeat
//! storage (`session_heartbeat_instructions`, `session_heartbeat_config`, `agent_heartbeat_config`,
//! `heartbeat_instructions_audit`, `agents.heartbeat_instructions`) is dropped. Irreversible.

use rusqlite::{params, Connection};

use super::super::helpers::{column_exists, table_exists};
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 57,
    description: "fold session heartbeats into agent_schedules and drop heartbeat storage",
    up: migrate_v57_unified_triggers,
}];

/// Frozen copy of the removed `DEFAULT_HEARTBEAT_INSTRUCTIONS` (v56 semantics).
const V57_DEFAULT_HEARTBEAT_INSTRUCTIONS: &str =
    "今この瞬間、自律的に何をするか判断してください。発言は30分に1回以下が望ましい。";

/// Frozen copy of the removed `MAX_HEARTBEAT_INSTRUCTIONS_LEN` (v56 semantics).
const V57_MAX_HEARTBEAT_INSTRUCTIONS_LEN: usize = 4000;

/// Smallest interval a v56 heartbeat could fire at; smaller stored values were floored at runtime,
/// so moving them verbatim would change the effective interval.
const V57_MIN_INTERVAL_SECS: i64 = 300;

/// Frozen copy of the removed `sanitize_heartbeat_instructions` (v56 semantics).
fn v57_sanitize_heartbeat_instructions(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect();
    cleaned
        .chars()
        .take(V57_MAX_HEARTBEAT_INSTRUCTIONS_LEN)
        .collect()
}

struct V57HeartbeatRow {
    agent_id: String,
    session_id: String,
    interval_secs: Option<i64>,
    anchor_at: Option<String>,
    last_fired_at: Option<String>,
    override_text: Option<String>,
    agent_text: Option<String>,
}

fn v57_failure(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some(message),
    )
}

fn migrate_v57_unified_triggers(conn: &Connection) -> rusqlite::Result<()> {
    // Like v50 / v55, v57 guards on presence so it is idempotent over a schema without the tables.
    if table_exists(conn, "session_heartbeat_config")? {
        migrate_v57_enabled_heartbeats(conn)?;
    }
    conn.execute_batch(
        "DROP TABLE IF EXISTS session_heartbeat_instructions;
         DROP TABLE IF EXISTS session_heartbeat_config;
         DROP TABLE IF EXISTS agent_heartbeat_config;
         DROP TABLE IF EXISTS heartbeat_instructions_audit;",
    )?;
    if column_exists(conn, "agents", "heartbeat_instructions")? {
        conn.execute_batch("ALTER TABLE agents DROP COLUMN heartbeat_instructions;")?;
    }
    Ok(())
}

fn migrate_v57_enabled_heartbeats(conn: &Connection) -> rusqlite::Result<()> {
    let has_agent_text = column_exists(conn, "agents", "heartbeat_instructions")?;
    let agent_text_column = if has_agent_text {
        "agents.heartbeat_instructions"
    } else {
        "NULL"
    };
    let rows: Vec<V57HeartbeatRow> = conn
        .prepare(&format!(
            "SELECT config.agent_id, config.session_id, config.interval_secs,
                    config.anchor_at, config.last_fired_at,
                    instructions.override_text, {agent_text_column}
             FROM session_heartbeat_config AS config
             LEFT JOIN session_heartbeat_instructions AS instructions
               ON instructions.agent_id = config.agent_id
              AND instructions.session_id = config.session_id
             LEFT JOIN agents ON agents.agent_id = config.agent_id
             WHERE config.enabled = 1
             ORDER BY config.agent_id, config.session_id"
        ))?
        .query_map([], |row| {
            Ok(V57HeartbeatRow {
                agent_id: row.get(0)?,
                session_id: row.get(1)?,
                interval_secs: row.get(2)?,
                anchor_at: row.get(3)?,
                last_fired_at: row.get(4)?,
                override_text: row.get(5)?,
                agent_text: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let now = chrono::Utc::now().to_rfc3339();
    for row in rows {
        let interval_secs = match row.interval_secs {
            Some(secs) if secs >= V57_MIN_INTERVAL_SECS => secs,
            other => {
                return Err(v57_failure(format!(
                    "v57: enabled heartbeat for agent {} session {} has interval_secs {other:?}; \
                     set an explicit interval of at least {V57_MIN_INTERVAL_SECS}s or disable it before upgrading",
                    row.agent_id, row.session_id
                )))
            }
        };
        let agent_text = row
            .agent_text
            .as_deref()
            .map(v57_sanitize_heartbeat_instructions)
            .unwrap_or_default();
        let message = match row.override_text {
            Some(text) => text,
            None if !agent_text.is_empty() => agent_text,
            None => V57_DEFAULT_HEARTBEAT_INSTRUCTIONS.to_string(),
        };
        conn.execute(
            "INSERT INTO agent_schedules
                (agent_id, session_id, cron_expr, timezone, message, enabled,
                 anchor_at, last_fired_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'Asia/Tokyo', ?4, 1, ?5, ?6, ?7, ?7)",
            params![
                row.agent_id,
                row.session_id,
                format!("@every {interval_secs}s"),
                message,
                row.anchor_at,
                row.last_fired_at,
                now,
            ],
        )?;
    }

    Ok(())
}
