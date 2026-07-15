use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// MEMORY: Sessions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogRow {
    pub id: Option<i64>,
    pub agent_id: String,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub speaker_id: Option<String>,
    pub turn_number: Option<i32>,
    pub metadata_json: Option<String>,
    pub created_at: Option<String>,
}

/// `insert_session_log` の best-effort 版: 失敗を握り潰さず warn を残す（#47）。
///
/// 会話履歴のクリティカル経路では挿入失敗が「無言の履歴欠落」になる。伝播すると
/// 応答フロー自体を止めてしまう場所（ログは副作用）で使う想定なので、エラーは
/// 返さずログのみ。戻り値が要る/失敗を伝播すべき場所では `insert_session_log` を使うこと。
pub fn insert_session_log_best_effort(conn: &Connection, log: &SessionLogRow) {
    if let Err(e) = insert_session_log(conn, log) {
        tracing::warn!(
            session_id = %log.session_id,
            log_type = %log.log_type,
            "session log insert failed (best-effort path): {e}"
        );
    }
}

pub fn insert_session_log(conn: &Connection, log: &SessionLogRow) -> Result<i64> {
    // 本体テーブルとFTS影テーブルへの2書き込みをトランザクションで原子化する。
    // 途中失敗で FTS と memory_sessions が恒久的に不整合になるのを防ぐ。
    let tx = conn.unchecked_transaction()?;

    tx.execute(
        "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            log.agent_id,
            log.session_id,
            log.log_type,
            log.content,
            log.speaker_id,
            log.turn_number,
            log.metadata_json,
            Utc::now().to_rfc3339(),
        ],
    )?;

    let row_id = tx.last_insert_rowid();

    // FTSにも追加
    tx.execute(
        "INSERT INTO memory_sessions_fts (rowid, content, agent_id, session_id, log_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            row_id,
            log.content,
            log.agent_id,
            log.session_id,
            log.log_type
        ],
    )?;

    tx.commit()?;

    Ok(row_id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogResult {
    pub id: i64,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub created_at: String,
    pub score: f64,
}

pub fn search_session_logs(
    conn: &Connection,
    agent_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionLogResult>> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    let fts_query = tokens.join(" AND ");

    let mut stmt = conn.prepare(
        "SELECT ms.id, ms.session_id, ms.log_type, ms.content, ms.created_at, bm25(memory_sessions_fts) as score
         FROM memory_sessions_fts fts
         JOIN memory_sessions ms ON fts.rowid = ms.id
         WHERE fts.agent_id = ?1 AND memory_sessions_fts MATCH ?2
         ORDER BY score
         LIMIT ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, fts_query, limit as i64], |row| {
        Ok(SessionLogResult {
            id: row.get(0)?,
            session_id: row.get(1)?,
            log_type: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
            score: row.get(5)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// List all session logs for a given session, ordered by creation time.
/// Used for building conversation history in send_message.
pub fn list_session_logs_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC",
    )?;

    let rows = stmt.query_map(params![session_id], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Count the number of logs in a session.
pub fn count_session_logs(conn: &Connection, session_id: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// List session logs with id > after_id, ordered by id ASC.
pub fn list_session_logs_after_id(
    conn: &Connection,
    session_id: &str,
    after_id: i64,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 AND id > ?2 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![session_id, after_id], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// List the most recent N session logs (returned in id DESC order; caller should reverse).
pub fn list_recent_session_logs(
    conn: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![session_id, limit as i64], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Get topic nodes for a specific session, ordered by start_log_id ASC.
pub fn get_topic_nodes_for_session(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS}
         FROM memory_index_nodes WHERE agent_id = ?1 AND source_session_id = ?2 AND node_type = 'topic' ORDER BY start_log_id ASC"
    ))?;
    let rows = stmt.query_map(params![agent_id, session_id], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// スリープ棚卸しトリガ用: 指定時刻以降にログを持つ distinct セッション数（新規活動量）。
/// `since` が None なら全期間。採点済み件数ではなく「未処理の活動量」を数える。
pub fn count_active_sessions_since(
    conn: &Connection,
    agent_id: &str,
    since: Option<&str>,
) -> Result<i64> {
    let n: i64 = match since {
        Some(ts) => conn.query_row(
            "SELECT COUNT(DISTINCT session_id) FROM memory_sessions
             WHERE agent_id = ?1 AND created_at > ?2",
            params![agent_id, ts],
            |r| r.get(0),
        )?,
        None => conn.query_row(
            "SELECT COUNT(DISTINCT session_id) FROM memory_sessions WHERE agent_id = ?1",
            params![agent_id],
            |r| r.get(0),
        )?,
    };
    Ok(n)
}

/// スリープ棚卸しの結末素材: エージェント単位で直近の verify 評価を新しい順に返す。
/// 戻り値は (session_id, content)。棚卸しではセッション単位の結末として提示する。
pub fn list_recent_evaluations_by_agent(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, content FROM memory_sessions
         WHERE agent_id = ?1 AND log_type = 'evaluation' ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![agent_id, limit], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
