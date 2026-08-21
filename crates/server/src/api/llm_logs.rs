use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct LlmLogsQuery {
    pub limit: Option<i64>,
}

pub async fn list_llm_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LlmLogsQuery>,
) -> Json<serde_json::Value> {
    let limit = query.limit.unwrap_or(20);
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::list_llm_logs(&conn, &id, limit) {
        Ok(logs) => {
            let data: Vec<serde_json::Value> = logs
                .into_iter()
                .map(|log| {
                    serde_json::json!({
                        "id": log.id,
                        "agent_id": log.agent_id,
                        "session_id": log.session_id,
                        "model": log.model,
                        "prompt": log.prompt,
                        "response": log.response,
                        "tool_calls": log.tool_calls,
                        "latency_ms": log.latency_ms,
                        "prompt_tokens": log.prompt_tokens,
                        "completion_tokens": log.completion_tokens,
                        "total_tokens": log.total_tokens,
                        "error_code": log.error_code,
                        "error_body": log.error_body,
                        "requested_at": log.requested_at,
                        "trigger_message_id": log.trigger_message_id,
                        "is_bot_iteration": log.is_bot_iteration,
                        "cache_read_tokens": log.cache_read_tokens,
                        "cache_creation_tokens": log.cache_creation_tokens,
                        "created_at": log.created_at,
                    })
                })
                .collect();
            Json(serde_json::json!(data))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn llm_logs_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::llm_logs_stats(&conn, &id, 30) {
        Ok(stats) => Json(serde_json::json!(stats)),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}
