use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};

use crate::AppState;

pub async fn get_status(
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let conn = state.db.lock().unwrap();
    let watermark = opencrab_db::queries::get_daily_log_watermark(&conn, &agent_id).unwrap_or(None);
    let last_indexed = watermark
        .as_ref()
        .map(|w| w.last_indexed_date.as_str())
        .unwrap_or("0000-00-00");
    let total_indexed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_nodes WHERE agent_id=?1 AND source_type='daily_log' AND node_type='daily'",
            rusqlite::params![&agent_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let unindexed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_curated WHERE agent_id=?1 AND category LIKE 'daily_log/%' AND substr(category, 11) > ?2",
            rusqlite::params![&agent_id, last_indexed],
            |row| row.get(0),
        )
        .unwrap_or(0);
    Ok(Json(serde_json::json!({
        "last_indexed_date": watermark.map(|w| w.last_indexed_date),
        "total_indexed_days": total_indexed,
        "unindexed_days": unindexed,
    })))
}

pub async fn rebuild(
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let db_clone = state.db.clone();
    let llm_clone = state.llm_router.clone();
    let model_clone = state.default_model.clone();
    tokio::spawn(async move {
        let adapter = crate::llm_adapter::LlmRouterAdapter::new(llm_clone);
        let indexer = opencrab_core::memory::DailyLogIndexer::new(
            db_clone,
            Arc::new(adapter),
            model_clone,
        );
        if let Err(e) = indexer.rebuild(&agent_id).await {
            tracing::warn!("daily_log rebuild failed: {}", e);
        }
    });
    Ok(Json(serde_json::json!({"status": "started"})))
}
