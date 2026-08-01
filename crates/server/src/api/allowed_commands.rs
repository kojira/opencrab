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

/// 許可コマンドの一覧（**DB 行のみ**）。
///
/// 線引きは「**LLM に露出する口は実効リスト / HTTP 管理 API は DB 行**」（#300）。
/// エージェント向けツール（`list_allowed_commands` /
/// `manage_allowed_commands(action="list")`）は「自分が実行できる全部」を知る必要があるので
/// `process::effective_allowed_commands` を通すが、この REST は add/remove と対の管理用で、
/// 設定ファイル由来のコマンドを混ぜると **remove できない行**がダッシュボードに並ぶ。
/// 「揃っていない」ように見えても、ここを実効リストへ変えないこと。
pub async fn list_allowed_commands(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Json<Vec<AllowedCommandDto>> {
    let conn = state.db.lock().unwrap();
    let commands =
        opencrab_db::queries::list_agent_allowed_commands(&conn, &agent_id).unwrap_or_default();
    Json(
        commands
            .into_iter()
            .map(|c| AllowedCommandDto { command: c })
            .collect(),
    )
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
    let added =
        opencrab_db::queries::add_agent_allowed_command(&conn, &agent_id, &req.command, "owner")
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // グローバル tools_config には反映しない（他エージェントへ漏れるため）。
    // このエージェントの許可コマンドは DB を信頼できる情報源とし、
    // run_agent_response が実行時に該当エージェント分だけ適用する。

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

    // グローバル tools_config は変更しない（config 由来のコマンドを誤って消さないため）。
    // このエージェントの許可は DB から削除済みで、実行時にはもう適用されない。

    Json(serde_json::json!({ "removed": removed }))
}
