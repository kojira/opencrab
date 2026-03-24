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

    let import_result = {
        let conn = state.db.lock().unwrap();
        execute_import(&conn, &agent_id, &scan_result, &import_options).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?
    };

    // Build memory index incrementally
    let config = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_index_config(&conn, &agent_id)
            .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
                agent_id: agent_id.clone(),
                batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                updated_at: String::new(),
            })
    };
    let batch_size = config.batch_size as usize;
    let llm_adapter = crate::llm_adapter::LlmRouterAdapter::new(state.llm_router.clone());
    let model = state.default_model.clone();

    let mut total_indexed: usize = 0;
    loop {
        match opencrab_core::memory_index::IndexBuilder::build_incremental(
            &state.db,
            &agent_id,
            &llm_adapter,
            &model,
            batch_size,
        )
        .await
        {
            Ok(index_result) => {
                if index_result.logs_indexed == 0 {
                    break;
                }
                total_indexed += index_result.logs_indexed;
            }
            Err(_) => {
                break;
            }
        }
    }

    let mut result = import_result;
    result.indexed_logs_count = total_indexed;

    Ok(Json(serde_json::json!({
        "agent_id": agent_id,
        "result": result,
    })))
}
