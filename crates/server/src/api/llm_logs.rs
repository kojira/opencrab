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
            let data: Vec<serde_json::Value> = logs.into_iter().map(|log| {
                serde_json::json!({
                    "id": log.id,
                    "agent_id": log.agent_id,
                    "session_id": log.session_id,
                    "model": log.model,
                    "prompt": log.prompt,
                    "response": log.response,
                    "tool_calls": log.tool_calls,
                    "created_at": log.created_at,
                })
            }).collect();
            Json(serde_json::json!(data))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}
