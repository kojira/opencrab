use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use opencrab_core::import::{
    check_sync_status, execute_sync_import, get_sync_history, SyncOptions,
};
use opencrab_db::queries::get_identity;

use crate::AppState;

fn agent_not_found() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Agent not found", "code": "AGENT_NOT_FOUND" })),
    )
}

fn bad_request(msg: impl ToString) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

fn internal_error(msg: impl ToString) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg.to_string() })),
    )
}

// ============================================
// GET /api/agents/{id}/import/sync/status
// ============================================

#[derive(Debug, Deserialize)]
pub struct SyncStatusQuery {
    pub source_dir: String,
    pub include_daily_logs: Option<bool>,
    pub daily_log_days: Option<u32>,
}

pub async fn get_sync_status(
    Path(agent_id): Path<String>,
    Query(query): Query<SyncStatusQuery>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let conn = state.db.lock().unwrap();

    let identity = get_identity(&conn, &agent_id).map_err(internal_error)?;
    if identity.is_none() {
        return Err(agent_not_found());
    }

    let options = SyncOptions {
        include_daily_logs: query.include_daily_logs.unwrap_or(true),
        daily_log_days: query.daily_log_days.unwrap_or(30),
        force_resync: false,
    };

    let result =
        check_sync_status(&conn, &agent_id, &query.source_dir, &options).map_err(bad_request)?;

    Ok(Json(serde_json::to_value(result).unwrap()))
}

// ============================================
// POST /api/agents/{id}/import/sync
// ============================================

#[derive(Debug, Deserialize)]
pub struct SyncRequest {
    pub source_dir: String,
    pub options: Option<SyncRequestOptions>,
}

#[derive(Debug, Deserialize)]
pub struct SyncRequestOptions {
    pub include_daily_logs: Option<bool>,
    pub daily_log_days: Option<u32>,
    pub force_resync: Option<bool>,
}

pub async fn execute_import_sync(
    Path(agent_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<SyncRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let conn = state.db.lock().unwrap();

    let identity = get_identity(&conn, &agent_id).map_err(internal_error)?;
    if identity.is_none() {
        return Err(agent_not_found());
    }

    let opts = req.options.as_ref();
    let options = SyncOptions {
        include_daily_logs: opts.and_then(|o| o.include_daily_logs).unwrap_or(true),
        daily_log_days: opts.and_then(|o| o.daily_log_days).unwrap_or(30),
        force_resync: opts.and_then(|o| o.force_resync).unwrap_or(false),
    };

    let result =
        execute_sync_import(&conn, &agent_id, &req.source_dir, &options).map_err(bad_request)?;

    Ok(Json(serde_json::json!({
        "agent_id": result.agent_id,
        "synced_at": result.synced_at,
        "result": {
            "memory_md": {
                "sections_upserted": result.memory_md_upserted,
                "sections_skipped": result.memory_md_skipped,
            },
            "daily_logs": {
                "files_imported": result.daily_logs_imported,
                "files_skipped": result.daily_logs_skipped,
            }
        },
        "warnings": result.warnings,
        "errors": result.errors,
    })))
}

// ============================================
// GET /api/agents/{id}/import/sync/history
// ============================================

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn get_import_sync_history(
    Path(agent_id): Path<String>,
    Query(query): Query<HistoryQuery>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let conn = state.db.lock().unwrap();

    let identity = get_identity(&conn, &agent_id).map_err(internal_error)?;
    if identity.is_none() {
        return Err(agent_not_found());
    }

    let limit = query.limit.unwrap_or(20);
    let offset = query.offset.unwrap_or(0);

    let (items, total) =
        get_sync_history(&conn, &agent_id, limit, offset).map_err(internal_error)?;

    Ok(Json(serde_json::json!({
        "total": total,
        "items": items,
    })))
}
