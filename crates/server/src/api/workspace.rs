use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct WorkspaceQuery {
    pub path: Option<String>,
}

/// Open the agent's real workspace (the same directory the agent runs in),
/// validating the agent id to prevent path traversal via the `id` segment.
fn open_agent_workspace(
    state: &AppState,
    id: &str,
) -> Result<opencrab_core::workspace::Workspace, String> {
    opencrab_core::workspace::validate_agent_id(id).map_err(|e| e.to_string())?;
    let ws_path = state.workspace_base.replace("{agent_id}", id);
    opencrab_core::workspace::Workspace::from_root(std::path::Path::new(&ws_path))
        .map_err(|e| e.to_string())
}

pub async fn list_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<WorkspaceQuery>,
) -> Json<serde_json::Value> {
    let workspace = match open_agent_workspace(&state, &id) {
        Ok(ws) => ws,
        Err(e) => {
            return Json(serde_json::json!({"error": e}));
        }
    };

    match workspace.list(query.path.as_deref().unwrap_or("")).await {
        Ok(entries) => {
            let entries_json: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "is_dir": e.is_dir,
                        "size": e.size,
                    })
                })
                .collect();
            Json(serde_json::json!({"entries": entries_json}))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn read_file(
    State(state): State<AppState>,
    Path((id, path)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let workspace = match open_agent_workspace(&state, &id) {
        Ok(ws) => ws,
        Err(e) => {
            return Json(serde_json::json!({"error": e}));
        }
    };

    match workspace.read(&path).await {
        Ok(content) => Json(serde_json::json!({
            "path": path,
            "content": content,
        })),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

#[derive(Debug, Deserialize)]
pub struct WriteFileRequest {
    pub content: String,
}

pub async fn write_file(
    State(state): State<AppState>,
    Path((id, path)): Path<(String, String)>,
    Json(req): Json<WriteFileRequest>,
) -> Json<serde_json::Value> {
    let workspace = match open_agent_workspace(&state, &id) {
        Ok(ws) => ws,
        Err(e) => {
            return Json(serde_json::json!({"error": e}));
        }
    };

    match workspace.write(&path, &req.content).await {
        Ok(_) => Json(serde_json::json!({"written": true})),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}
