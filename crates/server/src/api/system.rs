use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::{
    agent_log::{get_log_level, set_log_level, LogLevel},
    AppState,
};

#[derive(Serialize)]
pub struct LogLevelResponse {
    pub log_level: String,
}

#[derive(Deserialize)]
pub struct PatchLogLevelRequest {
    pub log_level: String,
}

pub async fn get_log_level_handler(State(_state): State<AppState>) -> Json<LogLevelResponse> {
    Json(LogLevelResponse {
        log_level: get_log_level().as_str().to_string(),
    })
}

pub async fn patch_log_level_handler(
    State(_state): State<AppState>,
    Json(req): Json<PatchLogLevelRequest>,
) -> Result<Json<LogLevelResponse>, (StatusCode, String)> {
    let level = LogLevel::from_str(&req.log_level).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid log level: {}. Must be one of: debug, info, warn, error",
                req.log_level
            ),
        )
    })?;
    set_log_level(level);
    Ok(Json(LogLevelResponse {
        log_level: level.as_str().to_string(),
    }))
}
