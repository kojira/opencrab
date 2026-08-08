use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Agent Schedules（per-agent 定時実行 / #455）
// ============================================
//
// cron / `@every` をセッション時刻源へ載せる。既定は無効（fail-closed / #240）。
// **この PR（PR1）では発火（scheduler 配線）はまだ行わない**（PR4）。ここではスキーマと
// CRUD クエリまでを用意する。cron 式の検証・発火経路・認可は PR4 で足す。

/// 定時実行スケジュール 1 行（`agent_schedules`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentScheduleRow {
    /// `None` = 未挿入（INSERT で採番）。
    pub id: Option<i64>,
    pub agent_id: String,
    /// 注入先の一本化されたセッション（Nostr agent は `nostr-{agent}`）。
    pub session_id: String,
    /// 標準 5 フィールド cron、または `@every 3h`。
    pub cron_expr: String,
    /// cron 評価に使う tz（既定 `Asia/Tokyo`）。
    pub timezone: String,
    pub message: String,
    pub enabled: bool,
    /// `@every` 用アンカー（rfc3339）。
    pub anchor_at: Option<String>,
    pub last_run_at: Option<String>,
    /// 計算結果キャッシュ（真実は再計算・rfc3339）。
    pub next_run_at: Option<String>,
}

fn row_from(row: &rusqlite::Row) -> rusqlite::Result<AgentScheduleRow> {
    Ok(AgentScheduleRow {
        id: Some(row.get(0)?),
        agent_id: row.get(1)?,
        session_id: row.get(2)?,
        cron_expr: row.get(3)?,
        timezone: row.get(4)?,
        message: row.get(5)?,
        enabled: row.get(6)?,
        anchor_at: row.get(7)?,
        last_run_at: row.get(8)?,
        next_run_at: row.get(9)?,
    })
}

const SELECT_COLS: &str =
    "id, agent_id, session_id, cron_expr, timezone, message, enabled, anchor_at, last_run_at, next_run_at";

/// スケジュールを挿入し、採番された id を返す（`created_at`/`updated_at` は現在時刻）。
pub fn insert_agent_schedule(conn: &Connection, row: &AgentScheduleRow) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO agent_schedules
            (agent_id, session_id, cron_expr, timezone, message, enabled,
             anchor_at, last_run_at, next_run_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            row.agent_id,
            row.session_id,
            row.cron_expr,
            row.timezone,
            row.message,
            row.enabled,
            row.anchor_at,
            row.last_run_at,
            row.next_run_at,
            now,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// id で 1 件取得する。無ければ `None`。
pub fn get_agent_schedule(conn: &Connection, id: i64) -> Result<Option<AgentScheduleRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM agent_schedules WHERE id = ?1");
    let result = conn.query_row(&sql, params![id], row_from);
    match result {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// あるエージェントの全スケジュールを取得する（作成順）。
pub fn list_agent_schedules(conn: &Connection, agent_id: &str) -> Result<Vec<AgentScheduleRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM agent_schedules WHERE agent_id = ?1 ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent_id], row_from)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// **enabled = 1** のスケジュールを全件列挙する（中央スケジューラ用 / PR4）。
pub fn list_enabled_agent_schedules(conn: &Connection) -> Result<Vec<AgentScheduleRow>> {
    let sql = format!("SELECT {SELECT_COLS} FROM agent_schedules WHERE enabled = 1 ORDER BY id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_from)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 可変フィールド（cron/tz/message/enabled/anchor）を更新する。`id` 必須。
pub fn update_agent_schedule(conn: &Connection, row: &AgentScheduleRow) -> Result<()> {
    let Some(id) = row.id else {
        anyhow::bail!("update_agent_schedule: id is required");
    };
    conn.execute(
        "UPDATE agent_schedules SET
            session_id = ?2, cron_expr = ?3, timezone = ?4, message = ?5,
            enabled = ?6, anchor_at = ?7, updated_at = ?8
         WHERE id = ?1",
        params![
            id,
            row.session_id,
            row.cron_expr,
            row.timezone,
            row.message,
            row.enabled,
            row.anchor_at,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// 発火後に実行時刻キャッシュを更新する（スケジューラ用 / PR4）。
pub fn set_agent_schedule_run(
    conn: &Connection,
    id: i64,
    last_run_at: &str,
    next_run_at: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE agent_schedules SET last_run_at = ?2, next_run_at = ?3, updated_at = ?4 WHERE id = ?1",
        params![id, last_run_at, next_run_at, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// スケジュールを削除する。
pub fn delete_agent_schedule(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM agent_schedules WHERE id = ?1", params![id])?;
    Ok(())
}
