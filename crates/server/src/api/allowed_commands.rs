use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct AllowedCommandDto {
    pub command: String,
}

pub async fn list_allowed_commands(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Json<Vec<AllowedCommandDto>> {
    let conn = state.db.lock().unwrap();
    let commands = opencrab_db::queries::list_agent_allowed_commands(&conn, &agent_id)
        .unwrap_or_default();
    Json(commands.into_iter().map(|c| AllowedCommandDto { command: c }).collect())
}

#[derive(Debug, Deserialize)]
pub struct AddAllowedCommandRequest {
    pub command: String,
}

pub async fn add_allowed_command(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddAllowedCommandRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if req.command.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let conn = state.db.lock().unwrap();
    let added = opencrab_db::queries::add_agent_allowed_command(
        &conn, &agent_id, &req.command, "owner"
    ).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Update in-memory tools_config
    drop(conn);
    if let Ok(mut cfg) = state.tools_config.write() {
        if let Some(ref mut shell) = cfg.shell {
            if !shell.allowed_commands.contains(&req.command) {
                shell.allowed_commands.push(req.command.clone());
            }
        }
    }

    Ok(Json(serde_json::json!({
        "command": req.command,
        "added": added
    })))
}

pub async fn remove_allowed_command(
    State(state): State<AppState>,
    Path((agent_id, command)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let removed = opencrab_db::queries::remove_agent_allowed_command(&conn, &agent_id, &command)
        .unwrap_or(false);

    // Update in-memory tools_config
    drop(conn);
    if let Ok(mut cfg) = state.tools_config.write() {
        if let Some(ref mut shell) = cfg.shell {
            shell.allowed_commands.retain(|c| c != &command);
        }
    }

    Json(serde_json::json!({ "removed": removed }))
}
