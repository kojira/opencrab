use anyhow::Result;
use rusqlite::{params, params_from_iter, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// LLM Logs
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmLogRow {
    pub id: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub prompt: String,
    pub response: String,
    pub tool_calls: Option<String>,
    pub latency_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub error_code: Option<String>,
    pub error_body: Option<String>,
    pub requested_at: Option<String>,
    pub trigger_message_id: Option<String>,
    pub is_bot_iteration: bool,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub created_at: String,
}

pub fn insert_llm_log(conn: &Connection, row: &LlmLogRow) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_logs (id, agent_id, session_id, model, prompt, response, tool_calls, latency_ms, prompt_tokens, completion_tokens, total_tokens, error_code, error_body, requested_at, trigger_message_id, is_bot_iteration, cache_read_tokens, cache_creation_tokens, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            row.id,
            row.agent_id,
            row.session_id,
            row.model,
            row.prompt,
            row.response,
            row.tool_calls,
            row.latency_ms,
            row.prompt_tokens,
            row.completion_tokens,
            row.total_tokens,
            row.error_code,
            row.error_body,
            row.requested_at,
            row.trigger_message_id,
            row.is_bot_iteration,
            row.cache_read_tokens,
            row.cache_creation_tokens,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn list_llm_logs(conn: &Connection, agent_id: &str, limit: i64) -> Result<Vec<LlmLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, model, prompt, response, tool_calls,
                latency_ms, prompt_tokens, completion_tokens, total_tokens,
                error_code, error_body, requested_at, trigger_message_id,
                is_bot_iteration, cache_read_tokens, cache_creation_tokens, created_at
         FROM llm_logs
         WHERE agent_id = ?1
         ORDER BY created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![agent_id, limit], |row| {
        Ok(LlmLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            model: row.get(3)?,
            prompt: row.get(4)?,
            response: row.get(5)?,
            tool_calls: row.get(6)?,
            latency_ms: row.get(7)?,
            prompt_tokens: row.get(8)?,
            completion_tokens: row.get(9)?,
            total_tokens: row.get(10)?,
            error_code: row.get(11)?,
            error_body: row.get(12)?,
            requested_at: row.get(13)?,
            trigger_message_id: row.get(14)?,
            is_bot_iteration: row.get::<_, i64>(15).map(|v| v != 0).unwrap_or(false),
            cache_read_tokens: row.get(16)?,
            cache_creation_tokens: row.get(17)?,
            created_at: row.get(18)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmLogStatRow {
    pub date: String,
    pub count: i64,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub avg_latency_ms: f64,
    pub error_count: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

pub fn llm_logs_stats(conn: &Connection, agent_id: &str, days: i64) -> Result<Vec<LlmLogStatRow>> {
    let sql = "SELECT date(COALESCE(requested_at, created_at)) as date,
               COUNT(*) as count,
               COALESCE(SUM(total_tokens),0) as total_tokens,
               COALESCE(SUM(prompt_tokens),0) as prompt_tokens,
               COALESCE(SUM(completion_tokens),0) as completion_tokens,
               COALESCE(AVG(latency_ms),0) as avg_latency_ms,
               COUNT(CASE WHEN error_code IS NOT NULL THEN 1 END) as error_count,
               COALESCE(SUM(cache_read_tokens),0) as cache_read_tokens,
               COALESCE(SUM(cache_creation_tokens),0) as cache_creation_tokens
        FROM llm_logs
        WHERE agent_id = ?1
          AND COALESCE(requested_at, created_at) >= datetime('now', ?2)
        GROUP BY date(COALESCE(requested_at, created_at))
        ORDER BY date ASC";
    let days_param = format!("-{} days", days);
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![agent_id, days_param], |row| {
        Ok(LlmLogStatRow {
            date: row.get(0)?,
            count: row.get(1)?,
            total_tokens: row.get(2)?,
            prompt_tokens: row.get(3)?,
            completion_tokens: row.get(4)?,
            avg_latency_ms: row.get(5)?,
            error_count: row.get(6)?,
            cache_read_tokens: row.get(7)?,
            cache_creation_tokens: row.get(8)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.into())
}

// ============================================
// LLM Logs アーカイブ（#337: 古いログを zip へ書き出して DB から外す）
// ============================================

/// `llm_logs` に存在する月（`YYYY-MM`）の一覧と各月の行数を、古い順に返す。
///
/// 月はタイムスタンプ `COALESCE(requested_at, created_at)` の先頭 7 文字で決める。
/// `substr` を使うのは RFC3339（`...T...+00:00`）と `datetime('now')` 形式
/// （`... ...`）が混在しても先頭 7 文字は常に `YYYY-MM` で安定なため
/// （`strftime` はタイムゾーン付き文字列で揺れうる）。両方 NULL の行は除外する。
pub fn list_llm_log_months(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT substr(COALESCE(requested_at, created_at), 1, 7) AS ym, COUNT(*) AS cnt
         FROM llm_logs
         WHERE COALESCE(requested_at, created_at) IS NOT NULL
         GROUP BY ym
         ORDER BY ym ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.into())
}

/// 指定月（`YYYY-MM`）に属する `llm_logs` 行を、時刻昇順（同時刻は id 昇順）で全件返す。
///
/// アーカイブ時に「書き出す行の実体」を確定させるための読み出し。ここで得た行を
/// そのまま zip に書き、**検証後にこの行の id だけ**を削除する（月の述語で消さない）。
pub fn list_llm_logs_for_month(conn: &Connection, month: &str) -> Result<Vec<LlmLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, model, prompt, response, tool_calls,
                latency_ms, prompt_tokens, completion_tokens, total_tokens,
                error_code, error_body, requested_at, trigger_message_id,
                is_bot_iteration, cache_read_tokens, cache_creation_tokens, created_at
         FROM llm_logs
         WHERE substr(COALESCE(requested_at, created_at), 1, 7) = ?1
         ORDER BY COALESCE(requested_at, created_at) ASC, id ASC",
    )?;
    let rows = stmt.query_map(params![month], |row| {
        Ok(LlmLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            model: row.get(3)?,
            prompt: row.get(4)?,
            response: row.get(5)?,
            tool_calls: row.get(6)?,
            latency_ms: row.get(7)?,
            prompt_tokens: row.get(8)?,
            completion_tokens: row.get(9)?,
            total_tokens: row.get(10)?,
            error_code: row.get(11)?,
            error_body: row.get(12)?,
            requested_at: row.get(13)?,
            trigger_message_id: row.get(14)?,
            is_bot_iteration: row.get::<_, i64>(15).map(|v| v != 0).unwrap_or(false),
            cache_read_tokens: row.get(16)?,
            cache_creation_tokens: row.get(17)?,
            created_at: row.get(18)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

/// 指定 id 群の `llm_logs` 行を**単一トランザクション**で削除し、削除件数を返す。
///
/// - 途中で失敗したらトランザクションごと巻き戻る（部分削除しない = DB が壊れない）。
/// - SQLite の変数上限を避けるためチャンク分割するが、**全チャンクを 1 つの
///   トランザクションに包む**ので原子性は保たれる。
/// - 月の述語ではなく**渡された id だけ**を消すので、書き出して検証済みでない行を
///   巻き込まない（アーカイブ後に挿入された同月の新しい行は残る）。
pub fn delete_llm_logs_by_ids(conn: &mut Connection, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut deleted = 0usize;
    for chunk in ids.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("DELETE FROM llm_logs WHERE id IN ({placeholders})");
        deleted += tx.execute(&sql, params_from_iter(chunk.iter()))?;
    }
    tx.commit()?;
    Ok(deleted)
}

#[cfg(test)]
mod archive_query_tests {
    use super::*;

    fn insert(conn: &Connection, id: &str, ts: &str) {
        let row = LlmLogRow {
            id: id.to_string(),
            agent_id: "a1".to_string(),
            session_id: None,
            model: Some("m".to_string()),
            prompt: format!("prompt for {id}"),
            response: format!("response for {id}"),
            tool_calls: None,
            latency_ms: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            error_code: None,
            error_body: None,
            requested_at: Some(ts.to_string()),
            trigger_message_id: None,
            is_bot_iteration: false,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            created_at: ts.to_string(),
        };
        insert_llm_log(conn, &row).unwrap();
    }

    #[test]
    fn months_are_grouped_and_ordered() {
        let conn = crate::init_memory().unwrap();
        insert(&conn, "a", "2026-03-21T10:00:00+00:00");
        insert(&conn, "b", "2026-03-25T10:00:00+00:00");
        insert(&conn, "c", "2026-05-01T00:00:00+00:00");
        let months = list_llm_log_months(&conn).unwrap();
        assert_eq!(
            months,
            vec![("2026-03".to_string(), 2), ("2026-05".to_string(), 1)]
        );
    }

    #[test]
    fn rows_for_month_are_scoped() {
        let conn = crate::init_memory().unwrap();
        insert(&conn, "a", "2026-03-21T10:00:00+00:00");
        insert(&conn, "b", "2026-04-02T10:00:00+00:00");
        let rows = list_llm_logs_for_month(&conn, "2026-03").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "a");
        assert_eq!(rows[0].prompt, "prompt for a");
    }

    #[test]
    fn delete_by_ids_removes_only_listed_and_is_atomic() {
        let mut conn = crate::init_memory().unwrap();
        insert(&conn, "a", "2026-03-21T10:00:00+00:00");
        insert(&conn, "b", "2026-03-22T10:00:00+00:00");
        insert(&conn, "c", "2026-04-01T10:00:00+00:00");
        let deleted =
            delete_llm_logs_by_ids(&mut conn, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(deleted, 2);
        let remaining = list_llm_log_months(&conn).unwrap();
        assert_eq!(remaining, vec![("2026-04".to_string(), 1)]);
    }
}
