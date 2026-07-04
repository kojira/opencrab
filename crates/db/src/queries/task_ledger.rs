use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Task Ledger（前向きワーキング状態: goal/契約/進捗/決定）
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLedgerRow {
    pub id: i64,
    pub agent_id: String,
    pub session_id: String,
    pub goal: String,
    pub contract: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProgressRow {
    pub id: i64,
    pub task_id: i64,
    pub kind: String,
    pub content: String,
    pub created_at: String,
}

const TASK_LEDGER_COLUMNS: &str =
    "id, agent_id, session_id, goal, contract, status, created_at, updated_at";

fn task_ledger_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskLedgerRow> {
    Ok(TaskLedgerRow {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        session_id: row.get(2)?,
        goal: row.get(3)?,
        contract: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

/// タスクを作成し、採番された id を返す（status は 'active'）。
pub fn insert_task_ledger(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    goal: &str,
    contract: Option<&str>,
) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO task_ledger (agent_id, session_id, goal, contract, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?5)",
        params![agent_id, session_id, goal, contract, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// id 指定で取得（agent_id スコープ: 他エージェントのタスクは見えない）。
pub fn get_task_ledger(
    conn: &Connection,
    agent_id: &str,
    task_id: i64,
) -> Result<Option<TaskLedgerRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TASK_LEDGER_COLUMNS} FROM task_ledger WHERE agent_id = ?1 AND id = ?2"
    ))?;
    let mut rows = stmt.query_map(params![agent_id, task_id], task_ledger_from_row)?;
    Ok(rows.next().transpose()?)
}

/// セッションの active タスク（最新1件）。
pub fn get_active_task_for_session(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<TaskLedgerRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TASK_LEDGER_COLUMNS} FROM task_ledger
         WHERE agent_id = ?1 AND session_id = ?2 AND status = 'active'
         ORDER BY id DESC LIMIT 1"
    ))?;
    let mut rows = stmt.query_map(params![agent_id, session_id], task_ledger_from_row)?;
    Ok(rows.next().transpose()?)
}

/// status を更新する。該当行が無ければ Ok(false)。
pub fn update_task_status(
    conn: &Connection,
    agent_id: &str,
    task_id: i64,
    status: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE task_ledger SET status = ?1, updated_at = ?2 WHERE agent_id = ?3 AND id = ?4",
        params![status, Utc::now().to_rfc3339(), agent_id, task_id],
    )?;
    Ok(n > 0)
}

/// goal / contract を再交渉する（None のフィールドは据え置き）。該当行が無ければ Ok(false)。
pub fn update_task_goal_contract(
    conn: &Connection,
    agent_id: &str,
    task_id: i64,
    goal: Option<&str>,
    contract: Option<&str>,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE task_ledger SET
            goal = COALESCE(?1, goal),
            contract = COALESCE(?2, contract),
            updated_at = ?3
         WHERE agent_id = ?4 AND id = ?5",
        params![goal, contract, Utc::now().to_rfc3339(), agent_id, task_id],
    )?;
    Ok(n > 0)
}

/// 進捗エントリ（progress / decision / blocker）を追記し、採番された id を返す。
/// 親タスクの updated_at も更新する。
pub fn insert_task_progress(
    conn: &Connection,
    task_id: i64,
    kind: &str,
    content: &str,
) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO task_progress (task_id, kind, content, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![task_id, kind, content, now],
    )?;
    let progress_id = conn.last_insert_rowid();
    conn.execute(
        "UPDATE task_ledger SET updated_at = ?1 WHERE id = ?2",
        params![now, task_id],
    )?;
    Ok(progress_id)
}

/// 直近 limit 件の進捗を**時系列順**で返す。
pub fn list_recent_task_progress(
    conn: &Connection,
    task_id: i64,
    limit: usize,
) -> Result<Vec<TaskProgressRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, kind, content, created_at FROM task_progress
         WHERE task_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![task_id, limit as i64], |row| {
        Ok(TaskProgressRow {
            id: row.get(0)?,
            task_id: row.get(1)?,
            kind: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    let mut list: Vec<TaskProgressRow> = rows.collect::<std::result::Result<_, _>>()?;
    list.reverse();
    Ok(list)
}

/// タスクの進捗エントリ総数。
pub fn count_task_progress(conn: &Connection, task_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM task_progress WHERE task_id = ?1",
        params![task_id],
        |r| r.get(0),
    )?)
}
