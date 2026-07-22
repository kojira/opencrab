//! per-agent Nostr sub-gateway の設定 API。
//!
//! Discord の per-agent 設定 API と同型: DB に設定を保存し、マネージャで
//! 起動/停止する。秘密鍵は応答でマスクする（平文を返さない）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use opencrab_db::queries::AgentNostrConfigRow;
use opencrab_nostr::{config_from_row, NostrFilter};

use crate::AppState;

/// nsec のマスク（末尾4文字だけ見せる）。
fn mask_secret(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() >= 8 {
        format!(
            "••••{}",
            chars[chars.len() - 4..].iter().collect::<String>()
        )
    } else if key.is_empty() {
        String::new()
    } else {
        "••••".to_string()
    }
}

/// GET /api/agents/{id}/nostr — 設定を返す（秘密鍵はマスク）。
pub async fn get_nostr_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let row = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, &id).unwrap_or(None)
    };
    let running = state
        .nostr_manager
        .as_ref()
        .map(|m| m.is_running(&id))
        .unwrap_or(false);

    match row {
        Some(cfg) => {
            let parsed = config_from_row(&cfg);
            Json(json!({
                "configured": true,
                "enabled": cfg.enabled,
                "running": running,
                "has_secret_key": !cfg.secret_key.is_empty(),
                "secret_key_masked": mask_secret(&cfg.secret_key),
                "relays": parsed.effective_relays(),
                "filter": {
                    "authors": parsed.filter.authors,
                    "keywords": parsed.filter.keywords,
                    "kinds": parsed.filter.kinds,
                },
            }))
        }
        None => Json(json!({
            "configured": false,
            "enabled": false,
            "running": running,
            "has_secret_key": false,
            "secret_key_masked": "",
            "relays": opencrab_nostr::DEFAULT_RELAYS,
            "filter": {"authors": [], "keywords": [], "kinds": []},
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct PutNostrBody {
    /// nsec。空/未指定なら既存を保持（更新でクリアしない）。
    #[serde(default)]
    pub secret_key: Option<String>,
    #[serde(default)]
    pub relays: Vec<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<u32>,
    /// 有効化して即起動するか。
    #[serde(default)]
    pub enabled: bool,
}

/// PUT /api/agents/{id}/nostr — 設定を保存し、enabled なら起動する。
pub async fn update_nostr_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PutNostrBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // 既存の秘密鍵を保持（新規指定が無ければ）。
    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, &id).unwrap_or(None)
    };
    let secret_key = body
        .secret_key
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().map(|e| e.secret_key.clone()))
        .unwrap_or_default();

    if secret_key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "secret_key（nsec）が必要です".to_string(),
        ));
    }

    let filter = NostrFilter {
        authors: body.authors.clone(),
        keywords: body.keywords.clone(),
        kinds: body.kinds.clone(),
    };
    // author も keyword も無い購読は全ノート洪水になるため拒否（enabled 時のみ）。
    if body.enabled && filter.authors.is_empty() && filter.keywords.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "author か keyword を少なくとも1つ指定してください（全ノート購読は不可）".to_string(),
        ));
    }

    let row = AgentNostrConfigRow {
        agent_id: id.clone(),
        secret_key,
        relays_json: serde_json::to_string(&body.relays).unwrap_or_else(|_| "[]".to_string()),
        filter_json: serde_json::to_string(&json!({
            "authors": filter.authors,
            "keywords": filter.keywords,
            "kinds": filter.kinds,
        }))
        .unwrap_or_else(|_| "{}".to_string()),
        enabled: body.enabled,
    };
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_nostr_config(&conn, &row)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    // マネージャ反映。
    if let Some(manager) = state.nostr_manager.as_ref() {
        if body.enabled {
            let config = config_from_row(&row);
            manager
                .start_agent_gateway(&id, &row.secret_key, config)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        } else {
            manager.stop_agent_gateway(&id).await;
        }
    }

    Ok(Json(json!({"updated": true, "enabled": body.enabled})))
}

/// POST /api/agents/{id}/nostr/start
pub async fn start_nostr_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let row = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_nostr_config(&conn, &id).unwrap_or(None)
    };
    let Some(row) = row else {
        return Err((StatusCode::NOT_FOUND, "Nostr 設定がありません".to_string()));
    };
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_enabled(&conn, &id, true).ok();
    }
    if let Some(manager) = state.nostr_manager.as_ref() {
        let config = config_from_row(&row);
        manager
            .start_agent_gateway(&id, &row.secret_key, config)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({"started": true})))
}

/// POST /api/agents/{id}/nostr/stop
pub async fn stop_nostr_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_nostr_config_enabled(&conn, &id, false).ok();
    }
    if let Some(manager) = state.nostr_manager.as_ref() {
        manager.stop_agent_gateway(&id).await;
    }
    Json(json!({"stopped": true}))
}

/// DELETE /api/agents/{id}/nostr
pub async fn delete_nostr_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    if let Some(manager) = state.nostr_manager.as_ref() {
        manager.stop_agent_gateway(&id).await;
    }
    let deleted = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::delete_agent_nostr_config(&conn, &id).unwrap_or(false)
    };
    Json(json!({"deleted": deleted}))
}
