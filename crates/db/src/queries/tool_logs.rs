//! ツール 1 実行 = 1 行（`tool_logs` / 載せ替え工程 5-b）。
//!
//! 書くのは core（`BridgedExecutor`）。ゲートは書かない。
//! `outcome` は本設計の閉集合。欠けた値を補完しない（fail-loud）。
//! `memory_sessions` / `llm_logs.tool_calls` は触らない。

use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// `tool_logs.outcome` の閉集合（DDL CHECK と同じ）。
pub const TOOL_LOG_OUTCOMES: &[&str] = &["done", "failed", "refused", "deadline", "stopped"];

/// `tool_logs` の 1 行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLogRow {
    pub id: i64,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub tool_name: String,
    pub args_json: String,
    pub outcome: String,
    pub result_text: String,
    pub started_at: Option<String>,
    pub created_at: String,
    pub latency_ms: Option<i64>,
    pub iteration: Option<i64>,
}

/// `insert_tool_log` の書き込み。`id` / `created_at` は DB が決める。
#[derive(Debug, Clone)]
pub struct ToolLogWrite {
    pub agent_id: String,
    pub session_id: Option<String>,
    pub tool_name: String,
    pub args_json: String,
    pub outcome: String,
    pub result_text: String,
    pub started_at: Option<String>,
    pub latency_ms: Option<i64>,
    pub iteration: Option<i64>,
}

/// `outcome` が CHECK 閉集合か。未知値は拒否する（既定へ落とさない）。
pub fn validate_tool_log_outcome(outcome: &str) -> Result<()> {
    if TOOL_LOG_OUTCOMES.contains(&outcome) {
        Ok(())
    } else {
        bail!("outcome は done|failed|refused|deadline|stopped のいずれか（got {outcome})");
    }
}

/// ツール 1 実行を 1 行書く。`id` を返す。
pub fn insert_tool_log(conn: &Connection, write: &ToolLogWrite) -> Result<i64> {
    validate_tool_log_outcome(&write.outcome)?;
    conn.execute(
        "INSERT INTO tool_logs (
            agent_id, session_id, tool_name, args_json, outcome,
            result_text, started_at, latency_ms, iteration
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            write.agent_id,
            write.session_id,
            write.tool_name,
            write.args_json,
            write.outcome,
            write.result_text,
            write.started_at,
            write.latency_ms,
            write.iteration
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 1 エージェントの tool_logs を新しい順で返す。
pub fn list_tool_logs(conn: &Connection, agent_id: &str, limit: i64) -> Result<Vec<ToolLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, tool_name, args_json, outcome,
                result_text, started_at, created_at, latency_ms, iteration
         FROM tool_logs
         WHERE agent_id = ?1
         ORDER BY created_at DESC, id DESC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![agent_id, limit], row_from_tool_log)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_from_tool_log(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolLogRow> {
    Ok(ToolLogRow {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        session_id: row.get(2)?,
        tool_name: row.get(3)?,
        args_json: row.get(4)?,
        outcome: row.get(5)?,
        result_text: row.get(6)?,
        started_at: row.get(7)?,
        created_at: row.get(8)?,
        latency_ms: row.get(9)?,
        iteration: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        crate::init_memory().expect("init_memory")
    }

    fn write(
        agent_id: &str,
        tool_name: &str,
        outcome: &str,
        result_text: &str,
        session_id: Option<&str>,
    ) -> ToolLogWrite {
        ToolLogWrite {
            agent_id: agent_id.to_string(),
            session_id: session_id.map(str::to_string),
            tool_name: tool_name.to_string(),
            args_json: r#"{"q":"x"}"#.to_string(),
            outcome: outcome.to_string(),
            result_text: result_text.to_string(),
            started_at: Some("2026-08-25T00:00:00Z".to_string()),
            latency_ms: Some(12),
            iteration: None,
        }
    }

    fn insert(
        conn: &Connection,
        agent_id: &str,
        tool_name: &str,
        outcome: &str,
        result_text: &str,
    ) -> i64 {
        insert_tool_log(
            conn,
            &write(agent_id, tool_name, outcome, result_text, Some("session-1")),
        )
        .unwrap()
    }

    #[test]
    fn insert_rejects_unknown_outcome() {
        let conn = setup();
        let err = insert_tool_log(
            &conn,
            &write("a1", "search_my_history", "success", "", None),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("outcome"),
            "未知 outcome は拒否: {err:#}"
        );
        assert!(list_tool_logs(&conn, "a1", 20).unwrap().is_empty());
    }

    #[test]
    fn insert_and_list_three_outcomes() {
        let conn = setup();
        insert(&conn, "a1", "search_my_history", "done", r#"{"hits":1}"#);
        insert(&conn, "a1", "nonexistent", "failed", "Unknown action");
        insert(&conn, "a1", "execute_shell", "refused", "rejected: owner");
        insert(&conn, "a2", "other", "done", "nope");

        let rows = list_tool_logs(&conn, "a1", 20).unwrap();
        assert_eq!(rows.len(), 3);
        let names: Vec<&str> = rows.iter().map(|r| r.tool_name.as_str()).collect();
        assert!(names.contains(&"search_my_history"));
        assert!(names.contains(&"nonexistent"));
        assert!(names.contains(&"execute_shell"));
        let outcomes: Vec<&str> = rows.iter().map(|r| r.outcome.as_str()).collect();
        assert_eq!(outcomes.iter().filter(|o| **o == "done").count(), 1);
        assert_eq!(outcomes.iter().filter(|o| **o == "failed").count(), 1);
        assert_eq!(outcomes.iter().filter(|o| **o == "refused").count(), 1);
        assert!(rows.iter().all(|r| r.agent_id == "a1"));
        assert!(rows
            .iter()
            .all(|r| r.session_id.as_deref() == Some("session-1")));
        assert_eq!(list_tool_logs(&conn, "a1", 1).unwrap().len(), 1);
        assert_eq!(list_tool_logs(&conn, "a2", 20).unwrap().len(), 1);
    }

    #[test]
    fn insert_accepts_deadline_and_stopped() {
        let conn = setup();
        insert(&conn, "a1", "slow", "deadline", "deadline");
        insert(&conn, "a1", "cancel", "stopped", "stopped");
        let rows = list_tool_logs(&conn, "a1", 20).unwrap();
        let outcomes: Vec<&str> = rows.iter().map(|r| r.outcome.as_str()).collect();
        assert!(outcomes.contains(&"deadline"));
        assert!(outcomes.contains(&"stopped"));
    }

    #[test]
    fn session_id_null_is_allowed() {
        let conn = setup();
        insert_tool_log(&conn, &write("a1", "search_my_history", "done", "", None)).unwrap();
        let rows = list_tool_logs(&conn, "a1", 20).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].session_id.is_none());
    }
}
