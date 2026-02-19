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
    pub role: String,
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
            "SELECT i.agent_id, i.name, COALESCE(s.persona_name, ''), i.role, i.image_url,
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
                role: row.get(3)?,
                image_url: row.get(4)?,
                status: "idle".to_string(),
                skill_count: row.get(5)?,
                session_count: row.get(6)?,
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
    pub role: Option<String>,
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
        role: req.role.unwrap_or_else(|| "discussant".to_string()),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    let soul = opencrab_db::queries::SoulRow {
        agent_id: agent_id.clone(),
        persona_name: req.persona_name,
        social_style_json: serde_json::json!({"assertiveness": 0.0, "responsiveness": 0.0, "style_name": "Analytical"}).to_string(),
        personality_json: serde_json::json!({"openness": 0.5, "conscientiousness": 0.5, "extraversion": 0.5, "agreeableness": 0.5, "neuroticism": 0.0}).to_string(),
        thinking_style_json: serde_json::json!({"primary": "論理的", "secondary": "分析的", "description": ""}).to_string(),
        custom_traits_json: None,
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
        custom_traits_json: soul.custom_traits_json,
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
        social_style_json: "{}".to_string(),
        personality_json: "{}".to_string(),
        thinking_style_json: "{}".to_string(),
        custom_traits_json: preset.custom_traits_json,
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
