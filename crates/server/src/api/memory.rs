use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

pub async fn list_curated_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::CuratedMemoryRow>> {
    let conn = state.db.lock().unwrap();
    let memories = opencrab_db::queries::list_curated_memories(&conn, &id).unwrap_or_default();
    Json(memories)
}

#[derive(Debug, Deserialize)]
pub struct SearchMemoryRequest {
    pub query: String,
    pub limit: Option<usize>,
}

pub async fn search_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SearchMemoryRequest>,
) -> Json<serde_json::Value> {
    let limit = req.limit.unwrap_or(10);
    let conn = state.db.lock().unwrap();

    match opencrab_db::queries::search_session_logs(&conn, &id, &req.query, limit) {
        Ok(results) => Json(serde_json::json!({
            "query": req.query,
            "count": results.len(),
            "results": results,
        })),
        Err(e) => Json(serde_json::json!({
            "error": e.to_string(),
        })),
    }
}
