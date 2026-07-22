//! per-agent の MCP サーバ設定 API。
//!
//! 1 エージェント × 複数サーバ。env（トークンを含みうる）は**値を返さない**（キーのみ）。
//! 設定変更後は接続をバックグラウンドで貼り直す（`reload_agent`）。

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use opencrab_db::queries::AgentMcpServerRow;
use opencrab_mcp::is_valid_server_name;

use crate::AppState;

fn default_true() -> bool {
    true
}

/// 設定変更後、該当エージェントの MCP 接続をバックグラウンドで貼り直す。
/// connect は subprocess 起動 + initialize を伴い時間がかかりうるため、HTTP は待たせない。
///
/// 世代は**同期的（リクエスト順）**に採番してから spawn する。連続編集は run_reload 側で
/// コアレッシングされ、最新世代の1回だけが実際に再接続する（古い設定が勝つ競合と
/// subprocess の同時多発を防ぐ）。
pub(crate) fn spawn_reload(state: &AppState, agent_id: String) {
    if let Some(manager) = state.mcp_manager.clone() {
        let gen = manager.mark_reload_requested(&agent_id);
        tokio::spawn(async move {
            manager.run_reload(&agent_id, gen).await;
        });
    }
}

/// GET /api/agents/{id}/mcp — サーバ一覧（接続状態つき、env は値を伏せる）。
pub async fn list_mcp_servers(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let rows = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_agent_mcp_servers(&conn, &id).unwrap_or_default()
    };
    // 接続済みサーバ名 → tools 数。
    let connected: HashMap<String, usize> = state
        .mcp_manager
        .as_ref()
        .map(|m| m.connected_status(&id).into_iter().collect())
        .unwrap_or_default();

    let servers: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let args: Vec<String> = serde_json::from_str(&r.args_json).unwrap_or_default();
            let env: BTreeMap<String, String> =
                serde_json::from_str(&r.env_json).unwrap_or_default();
            let env_keys: Vec<&String> = env.keys().collect();
            json!({
                "name": r.name,
                "command": r.command,
                "args": args,
                // env は値を返さない（トークン等）。設定済みキーのみ示す。
                "env_keys": env_keys,
                "trusted_only": r.trusted_only,
                "enabled": r.enabled,
                "connected": connected.contains_key(&r.name),
                "tools": connected.get(&r.name).copied(),
            })
        })
        .collect();

    Json(json!({ "servers": servers }))
}

#[derive(Debug, Deserialize)]
pub struct PutMcpBody {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 追加環境変数。空/未指定なら既存を保持する（値を伏せて返すため、無変更の更新で
    /// 消えないように）。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub trusted_only: bool,
    #[serde(default)]
    pub enabled: bool,
}

/// PUT /api/agents/{id}/mcp — サーバを1つ追加/更新する（キーは name）。
pub async fn put_mcp_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PutMcpBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let name = body.name.trim().to_string();
    if !is_valid_server_name(&name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "サーバ名は英数字・_・-（1〜64文字、__ を含まない）にしてください".to_string(),
        ));
    }
    if body.command.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "command が必要です".to_string()));
    }

    // env が空なら既存を保持（値を伏せているため、無変更更新で消さない）。
    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_mcp_server(&conn, &id, &name).unwrap_or(None)
    };
    let env_json = if body.env.is_empty() {
        existing
            .as_ref()
            .map(|e| e.env_json.clone())
            .unwrap_or_else(|| "{}".to_string())
    } else {
        serde_json::to_string(&body.env).unwrap_or_else(|_| "{}".to_string())
    };

    let row = AgentMcpServerRow {
        agent_id: id.clone(),
        name: name.clone(),
        command: body.command.trim().to_string(),
        args_json: serde_json::to_string(&body.args).unwrap_or_else(|_| "[]".to_string()),
        env_json,
        trusted_only: body.trusted_only,
        enabled: body.enabled,
    };
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_mcp_server(&conn, &row)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    spawn_reload(&state, id);
    Ok(Json(json!({ "updated": true, "name": name })))
}

/// POST /api/agents/{id}/mcp/{name}/enabled — 有効/無効を切り替える。
#[derive(Debug, Deserialize)]
pub struct SetEnabledBody {
    pub enabled: bool,
}

pub async fn set_mcp_enabled(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Json(body): Json<SetEnabledBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let updated = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_mcp_server_enabled(&conn, &id, &name, body.enabled)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    if !updated {
        return Err((StatusCode::NOT_FOUND, "サーバが見つかりません".to_string()));
    }
    spawn_reload(&state, id);
    Ok(Json(json!({ "updated": true, "enabled": body.enabled })))
}

/// DELETE /api/agents/{id}/mcp/{name}
pub async fn delete_mcp_server(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let deleted = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::delete_agent_mcp_server(&conn, &id, &name)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    if !deleted {
        return Err((StatusCode::NOT_FOUND, "サーバが見つかりません".to_string()));
    }
    // 実削除時のみ再接続（削除したサーバは reload で落ちる）。
    spawn_reload(&state, id);
    Ok(Json(json!({ "deleted": true })))
}
