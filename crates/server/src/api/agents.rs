use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub persona_name: String,
    pub image_url: Option<String>,
    pub status: String,
    pub skill_count: i32,
    pub session_count: i32,
}

pub async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentSummary>> {
    let conn = state.db.lock().unwrap();
    // JOIN soul and identity to get agent summaries
    let mut stmt = conn
        .prepare(
            "SELECT i.agent_id, i.name, COALESCE(s.persona_name, ''), i.image_url,
                    (SELECT COUNT(*) FROM skills WHERE agent_id = i.agent_id) as skill_count,
                    (SELECT COUNT(*) FROM agent_sessions WHERE agent_id = i.agent_id) as session_count
             FROM identity i
             LEFT JOIN soul s ON i.agent_id = s.agent_id
             ORDER BY i.name",
        )
        .unwrap();

    let agents = stmt
        .query_map([], |row| {
            Ok(AgentSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                persona_name: row.get(2)?,
                image_url: row.get(3)?,
                status: "idle".to_string(),
                skill_count: row.get(4)?,
                session_count: row.get(5)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    Json(agents)
}

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub id: Option<String>,
    pub name: String,
    pub persona_name: String,
}

pub async fn create_agent(
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Json<serde_json::Value> {
    let agent_id = req.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let conn = state.db.lock().unwrap();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: agent_id.clone(),
        name: req.name.clone(),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    let soul = opencrab_db::queries::SoulRow {
        agent_id: agent_id.clone(),
        persona_name: req.persona_name,
        personality: None,
        instructions: String::new(),
    };
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();

    Json(serde_json::json!({
        "id": agent_id,
        "name": req.name,
    }))
}

pub async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();

    let identity = opencrab_db::queries::get_identity(&conn, &id).unwrap();
    let soul = opencrab_db::queries::get_soul(&conn, &id).unwrap();

    Json(serde_json::json!({
        "identity": identity,
        "soul": soul,
    }))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // Stop per-agent Discord gateway if running.
    #[cfg(feature = "discord")]
    if let Some(ref manager) = state.discord_manager {
        manager.stop_agent_gateway(&id).await;
    }

    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::delete_agent(&conn, &id).unwrap();

    Json(serde_json::json!({"deleted": deleted}))
}

pub async fn get_soul(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let soul = opencrab_db::queries::get_soul(&conn, &id).unwrap();
    Json(serde_json::to_value(soul).unwrap())
}

pub async fn update_soul(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(soul): Json<opencrab_db::queries::SoulRow>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let mut soul = soul;
    soul.agent_id = id;
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();
    Json(serde_json::json!({"updated": true}))
}

pub async fn get_identity(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let identity = opencrab_db::queries::get_identity(&conn, &id).unwrap();
    Json(serde_json::to_value(identity).unwrap())
}

pub async fn update_identity(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(identity): Json<opencrab_db::queries::IdentityRow>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let mut identity = identity;
    identity.agent_id = id;
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();
    Json(serde_json::json!({"updated": true}))
}

// ============================================
// Soul Presets
// ============================================

pub async fn list_soul_presets(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::SoulPresetRow>> {
    let conn = state.db.lock().unwrap();
    let presets = opencrab_db::queries::list_soul_presets(&conn, &id).unwrap();
    Json(presets)
}

#[derive(Debug, Deserialize)]
pub struct CreateSoulPresetRequest {
    pub preset_name: String,
}

pub async fn create_soul_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CreateSoulPresetRequest>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let soul = opencrab_db::queries::get_soul(&conn, &id).unwrap();
    let Some(soul) = soul else {
        return Json(serde_json::json!({ "ok": false, "error": "Soul not found." }));
    };

    let preset = opencrab_db::queries::SoulPresetRow {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: id,
        preset_name: req.preset_name,
        persona_name: soul.persona_name,
        custom_traits_json: soul.personality,
    };
    opencrab_db::queries::insert_soul_preset(&conn, &preset).unwrap();

    Json(serde_json::json!({ "ok": true, "id": preset.id }))
}

pub async fn delete_soul_preset(
    State(state): State<AppState>,
    Path((_id, preset_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::delete_soul_preset(&conn, &preset_id).unwrap();
    Json(serde_json::json!({ "deleted": deleted }))
}

pub async fn apply_soul_preset(
    State(state): State<AppState>,
    Path((id, preset_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let preset = opencrab_db::queries::get_soul_preset(&conn, &preset_id).unwrap();
    let Some(preset) = preset else {
        return Json(serde_json::json!({ "ok": false, "error": "Preset not found." }));
    };

    let soul = opencrab_db::queries::SoulRow {
        agent_id: id,
        persona_name: preset.persona_name,
        personality: preset.custom_traits_json,
        instructions: String::new(),
    };
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();

    Json(serde_json::json!({ "ok": true }))
}

// ============================================
// Discord per-agent config
// ============================================

pub async fn get_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let cfg = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id).unwrap()
    };

    match cfg {
        Some(cfg) => {
            // Mask the token: show first 10 chars + "..."
            let token_masked = if cfg.bot_token.len() > 10 {
                format!("{}...", &cfg.bot_token[..10])
            } else {
                "***".to_string()
            };

            #[allow(unused_mut)]
            let mut running = false;
            #[cfg(feature = "discord")]
            if let Some(ref manager) = state.discord_manager {
                running = manager.is_running(&id).await;
            }

            Json(serde_json::json!({
                "configured": true,
                "enabled": cfg.enabled,
                "token_masked": token_masked,
                "owner_discord_id": cfg.owner_discord_id,
                "running": running,
            }))
        }
        None => Json(serde_json::json!({
            "configured": false,
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct PatchDiscordConfigRequest {
    pub bot_token: Option<String>,
    pub owner_discord_id: Option<String>,
}

pub async fn patch_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PatchDiscordConfigRequest>,
) -> Json<serde_json::Value> {
    // 既存の設定を取得
    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id).unwrap()
    };

    let Some(_existing) = existing else {
        return Json(serde_json::json!({
            "ok": false,
            "error": "No Discord config found. Use PUT to create one.",
        }));
    };

    // 部分更新
    let updated = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::patch_agent_discord_config(
            &conn,
            &id,
            req.bot_token.as_deref(),
            req.owner_discord_id.as_deref(),
        )
        .unwrap()
    };

    if !updated {
        return Json(serde_json::json!({
            "ok": false,
            "error": "Update failed.",
        }));
    };

    // 更新後の設定を返す
    let cfg = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id).unwrap().unwrap()
    };

    let token_masked = if cfg.bot_token.len() > 10 {
        format!("{}...", &cfg.bot_token[..10])
    } else {
        "***".to_string()
    };

    Json(serde_json::json!({
        "ok": true,
        "configured": true,
        "enabled": cfg.enabled,
        "token_masked": token_masked,
        "owner_discord_id": cfg.owner_discord_id,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateDiscordConfigRequest {
    pub bot_token: String,
    pub owner_discord_id: Option<String>,
}

pub async fn update_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateDiscordConfigRequest>,
) -> Json<serde_json::Value> {
    let owner_discord_id = req.owner_discord_id.unwrap_or_default();

    // Save to DB.
    {
        let conn = state.db.lock().unwrap();
        let cfg = opencrab_db::queries::AgentDiscordConfigRow {
            agent_id: id.clone(),
            bot_token: req.bot_token.clone(),
            owner_discord_id: owner_discord_id.clone(),
            enabled: true,
        };
        opencrab_db::queries::upsert_agent_discord_config(&conn, &cfg).unwrap();
    }

    // Start the gateway (only when discord feature is enabled).
    #[cfg(feature = "discord")]
    if let Some(ref manager) = state.discord_manager {
        match manager
            .start_agent_gateway(&id, &req.bot_token, &owner_discord_id)
            .await
        {
            Ok(()) => {
                return Json(serde_json::json!({
                    "ok": true,
                    "message": "Discord bot started.",
                }));
            }
            Err(e) => {
                tracing::error!(agent_id = %id, error = %e, "Failed to start per-agent Discord gateway");
                return Json(serde_json::json!({
                    "ok": false,
                    "error": e.to_string(),
                }));
            }
        }
    }

    // Config saved but gateway not started (discord feature disabled or manager not initialized).
    Json(serde_json::json!({
        "ok": true,
        "message": "Config saved. Gateway not started (discord feature not active).",
    }))
}

pub async fn start_discord_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let cfg = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_discord_config(&conn, &id).unwrap()
    };

    let Some(_cfg) = cfg else {
        return Json(serde_json::json!({ "ok": false, "error": "No Discord config found." }));
    };

    // Set enabled=1 in DB.
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_discord_config_enabled(&conn, &id, true).unwrap();
    }

    #[cfg(feature = "discord")]
    if let Some(ref manager) = state.discord_manager {
        match manager
            .start_agent_gateway(&id, &_cfg.bot_token, &_cfg.owner_discord_id)
            .await
        {
            Ok(()) => return Json(serde_json::json!({ "ok": true })),
            Err(e) => {
                tracing::error!(agent_id = %id, error = %e, "Failed to start Discord gateway");
                return Json(serde_json::json!({ "ok": false, "error": e.to_string() }));
            }
        }
    }

    Json(serde_json::json!({ "ok": false, "error": "Discord feature not active." }))
}

pub async fn stop_discord_gateway(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // Set enabled=0 in DB.
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_discord_config_enabled(&conn, &id, false).unwrap();
    }

    #[cfg(feature = "discord")]
    if let Some(ref manager) = state.discord_manager {
        manager.stop_agent_gateway(&id).await;
    }

    Json(serde_json::json!({ "ok": true }))
}

pub async fn delete_discord_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    // Stop the gateway.
    #[cfg(feature = "discord")]
    if let Some(ref manager) = state.discord_manager {
        manager.stop_agent_gateway(&id).await;
    }

    // Delete from DB.
    let deleted = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::delete_agent_discord_config(&conn, &id).unwrap()
    };

    Json(serde_json::json!({"deleted": deleted}))
}

// ============================================
// Memory Index API
// ============================================

/// GET /api/agents/{id}/memory/index — インデックス状態取得
pub async fn get_memory_index_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let watermark = opencrab_db::queries::get_index_watermark(&conn, &id)
        .ok()
        .flatten();
    let unindexed = opencrab_db::queries::get_unindexed_log_count(&conn, &id).unwrap_or(0);
    let tree = opencrab_db::queries::get_index_tree(&conn, &id).unwrap_or_default();
    let config = opencrab_db::queries::get_memory_index_config(&conn, &id)
        .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
            agent_id: id.clone(),
            batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
            threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
            updated_at: String::new(),
        });

    Json(serde_json::json!({
        "agent_id": id,
        "total_nodes": tree.len(),
        "unindexed_logs": unindexed,
        "watermark": watermark,
        "node_type_counts": {
            "root": tree.iter().filter(|n| n.node_type == "root").count(),
            "period": tree.iter().filter(|n| n.node_type == "period").count(),
            "session": tree.iter().filter(|n| n.node_type == "session").count(),
            "topic": tree.iter().filter(|n| n.node_type == "topic").count(),
        },
        "config": {
            "batch_size": config.batch_size,
            "threshold": config.threshold,
            "batch_size_min": opencrab_db::queries::BATCH_SIZE_MIN,
            "threshold_min": opencrab_db::queries::THRESHOLD_MIN,
        },
    }))
}

/// POST /api/agents/{id}/memory/index — 手動インデックス構築トリガー
pub async fn trigger_memory_index_build(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let db = state.db.clone();
    let agent_id = id.clone();
    let llm_router = state.llm_router.clone();
    let model = state.default_model.clone();

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

    let llm_adapter = crate::llm_adapter::LlmRouterAdapter::new(llm_router);

    match opencrab_core::memory_index::IndexBuilder::build_incremental(
        &db,
        &agent_id,
        &llm_adapter,
        &model,
        config.batch_size as usize,
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "nodes_created": result.nodes_created,
            "logs_indexed": result.logs_indexed,
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemoryIndexConfigRequest {
    pub batch_size: Option<i64>,
    pub threshold: Option<i64>,
}

pub async fn update_memory_index_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMemoryIndexConfigRequest>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let current = opencrab_db::queries::get_memory_index_config(&conn, &id)
        .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
            agent_id: id.clone(),
            batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
            threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
            updated_at: String::new(),
        });

    let new_batch_size = req.batch_size.unwrap_or(current.batch_size);
    let new_threshold = req.threshold.unwrap_or(current.threshold);

    match opencrab_db::queries::upsert_memory_index_config(&conn, &id, new_batch_size, new_threshold) {
        Ok(config) => Json(serde_json::json!({
            "ok": true,
            "config": {
                "agent_id": config.agent_id,
                "batch_size": config.batch_size,
                "threshold": config.threshold,
                "updated_at": config.updated_at,
            }
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

/// DELETE /api/agents/{id}/memory/index — インデックス全削除
pub async fn delete_memory_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    match opencrab_core::memory_index::IndexBuilder::delete_index(&state.db, &id) {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "message": "Index deleted",
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

/// POST /api/agents/{id}/memory/index/rebuild — インデックス再構築
pub async fn rebuild_memory_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let db = state.db.clone();
    let agent_id = id.clone();
    let llm_router = state.llm_router.clone();
    let model = state.default_model.clone();

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

    let llm_adapter = crate::llm_adapter::LlmRouterAdapter::new(llm_router);

    match opencrab_core::memory_index::IndexBuilder::rebuild_index(
        &db,
        &agent_id,
        &llm_adapter,
        &model,
        config.batch_size as usize,
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "nodes_created": result.nodes_created,
            "logs_indexed": result.logs_indexed,
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

/// POST /api/agents/{id}/memory/index/merge — トピック再マージ
pub async fn merge_memory_index_topics(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let db = state.db.clone();
    let agent_id = id.clone();
    let llm_router = state.llm_router.clone();
    let model = state.default_model.clone();

    let llm_adapter = crate::llm_adapter::LlmRouterAdapter::new(llm_router);
    // デフォルト: periodあたり最大10topic
    let max_topics_per_period = 10usize;

    match opencrab_core::memory_index::IndexBuilder::merge_topics(
        &db,
        &agent_id,
        &llm_adapter,
        &model,
        max_topics_per_period,
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "periods_processed": result.periods_processed,
            "topics_merged": result.topics_merged,
            "topics_deleted": result.topics_deleted,
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}
