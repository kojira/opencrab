use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use opencrab_core::import::{
    ScanOptions, ScanResult,
    ImportOptions,
    scan_workspace, execute_import,
};

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ScanRequest {
    pub source_dir: String,
    pub options: ScanOptions,
}

#[derive(Debug, Deserialize)]
pub struct ExecuteRequest {
    pub source_dir: String,
    pub agent_name: String,
    pub options: ScanOptions,
    pub confirmed: bool,
}

pub async fn scan_workspace_handler(
    State(_state): State<AppState>,
    Json(req): Json<ScanRequest>,
) -> Result<Json<ScanResult>, (StatusCode, Json<serde_json::Value>)> {
    match scan_workspace(&req.source_dir, &req.options) {
        Ok(result) => Ok(Json(result)),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

pub async fn execute_import_handler(
    State(state): State<AppState>,
    Json(req): Json<ExecuteRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !req.confirmed {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "confirmed must be true" })),
        ));
    }

    let scan_result = scan_workspace(&req.source_dir, &req.options).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let agent_id = uuid::Uuid::new_v4().to_string();
    let import_options = ImportOptions {
        overwrite_if_exists: req.options.overwrite_if_exists,
        agent_name: Some(req.agent_name),
    };

    let conn = state.db.lock().unwrap();
    match execute_import(&conn, &agent_id, &scan_result, &import_options) {
        Ok(result) => Ok(Json(serde_json::json!({
            "agent_id": agent_id,
            "result": result,
        }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}
