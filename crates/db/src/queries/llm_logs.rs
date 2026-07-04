use anyhow::Result;
use rusqlite::{params, Connection};
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
