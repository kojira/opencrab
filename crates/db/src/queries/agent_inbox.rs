use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// AGENT INBOX — 外部イベント受信箱（webhook intake / issue #454）
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInboxRow {
    pub id: String,
    pub agent_id: String,
    pub source: String,
    pub event_type: String,
    /// source 内で一意な重複排除キー（例: コメント id）。source アダプタが払い出す。
    pub dedup_key: String,
    pub payload_json: String,
    pub received_at: String,
    /// NULL = 未処理。消化ループが処理したら `datetime('now')` を刻む。
    pub processed_at: Option<String>,
}

/// 受信箱への投入 1 件分（`enqueue_inbox_event` の入力）。
///
/// `received_at` / `processed_at` は DB 側が決めるのでここには持たない。
pub struct InboxInsert {
    pub id: String,
    pub agent_id: String,
    pub source: String,
    pub event_type: String,
    pub dedup_key: String,
    pub payload_json: String,
}

/// 受信箱にイベントを積む。
///
/// UNIQUE(source, dedup_key) に対して `INSERT OR IGNORE` する。**同じ出来事が webhook と
/// catch-up の両方から来ても二重に積まない**（受け入れ基準の dedup）。
///
/// 戻り値は「新規に積まれたか」。`true` = 新規行、`false` = 既存（dedup で弾かれた）。
pub fn enqueue_inbox_event(conn: &Connection, ev: &InboxInsert) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO agent_inbox
            (id, agent_id, source, event_type, dedup_key, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            ev.id,
            ev.agent_id,
            ev.source,
            ev.event_type,
            ev.dedup_key,
            ev.payload_json,
        ],
    )?;
    Ok(n > 0)
}

/// あるエージェントの未処理イベントを受信順（古い順）に返す。
pub fn list_unprocessed_inbox(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<AgentInboxRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, source, event_type, dedup_key, payload_json, received_at, processed_at
         FROM agent_inbox
         WHERE agent_id = ?1 AND processed_at IS NULL
         ORDER BY received_at ASC, id ASC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![agent_id, limit], |row| {
        Ok(AgentInboxRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            source: row.get(2)?,
            event_type: row.get(3)?,
            dedup_key: row.get(4)?,
            payload_json: row.get(5)?,
            received_at: row.get(6)?,
            processed_at: row.get(7)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// あるエージェントの未処理件数。消化ループの「空なら回さない」ゲートに使う。
pub fn count_unprocessed_inbox(conn: &Connection, agent_id: &str) -> Result<i64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agent_inbox WHERE agent_id = ?1 AND processed_at IS NULL",
        params![agent_id],
        |row| row.get(0),
    )?;
    Ok(n)
}

/// 未処理行を持つエージェント id の一覧（重複なし）。消化ループの走査対象。
pub fn agents_with_unprocessed_inbox(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT agent_id FROM agent_inbox WHERE processed_at IS NULL ORDER BY agent_id",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 1 行を処理済みにする（`processed_at = datetime('now')`）。
///
/// `processed_at IS NULL` を条件に付け、二重処理で上書きしない。戻り値は「今回刻んだか」。
pub fn mark_inbox_processed(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute(
        "UPDATE agent_inbox SET processed_at = datetime('now')
         WHERE id = ?1 AND processed_at IS NULL",
        params![id],
    )?;
    Ok(n > 0)
}
