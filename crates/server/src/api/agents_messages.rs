use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;
use crate::process;

#[derive(Debug, Deserialize)]
pub struct SendAgentMessageRequest {
    pub content: String,
    pub user_id: String,
}

pub async fn send_agent_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendAgentMessageRequest>,
) -> Json<serde_json::Value> {
    let session_id = format!("agent-msg-{}-{}", id, req.user_id);

    // 1. Determine caller identity from trusted_users table.
    let caller = {
        let conn = state.db.lock().unwrap();
        match opencrab_db::queries::get_trusted_user(&conn, &req.user_id, &id) {
            Some(u) if u.permission == "co_agent" => {
                opencrab_actions::CallerIdentity::CoAgent {
                    agent_id: req.user_id.clone(),
                }
            }
            Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
            None => {
                let cfg = opencrab_db::queries::get_agent_discord_config(&conn, &id);
                if let Ok(Some(c)) = cfg {
                    if c.owner_discord_id == req.user_id {
                        opencrab_actions::CallerIdentity::Owner
                    } else {
                        opencrab_actions::CallerIdentity::Agent
                    }
                } else {
                    opencrab_actions::CallerIdentity::Agent
                }
            }
        }
    };

    let caller_type = match &caller {
        opencrab_actions::CallerIdentity::CoAgent { .. } => "co_agent",
        opencrab_actions::CallerIdentity::TrustedUser => "trusted_user",
        opencrab_actions::CallerIdentity::Owner => "owner",
        _ => "agent",
    };

    // 2. Ensure session exists (create if not).
    {
        let conn = state.db.lock().unwrap();
        let existing = opencrab_db::queries::get_session(&conn, &session_id)
            .ok()
            .flatten();
        if existing.is_none() {
            let session = opencrab_db::queries::SessionRow {
                id: session_id.clone(),
                mode: "autonomous".to_string(),
                theme: "direct_message".to_string(),
                phase: "divergent".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: serde_json::json!([&id]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            };
            if let Err(e) = opencrab_db::queries::insert_session(&conn, &session) {
                return Json(serde_json::json!({"error": format!("Failed to create session: {}", e)}));
            }
        }
    }

    // 3. Log user message.
    {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: id.clone(),
            session_id: session_id.clone(),
            log_type: "speech".to_string(),
            content: req.content.clone(),
            speaker_id: Some(req.user_id.clone()),
            turn_number: None,
            metadata_json: None,
        };
        let conn = state.db.lock().unwrap();
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            return Json(serde_json::json!({"error": format!("Failed to log message: {}", e)}));
        }
    }

    // 4. Check LLM availability.
    if state.llm_router.provider_names().is_empty() {
        return Json(serde_json::json!({
            "session_id": session_id,
            "caller_type": caller_type,
            "responses": [],
            "error": "No LLM providers available",
        }));
    }

    // 5. Get gateway_actions from discord_manager.
    #[cfg(feature = "discord")]
    let gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> = {
        if let Some(ref dm) = state.discord_manager {
            if let Some(http) = dm.get_http_for_agent(&id).await {
                let tools_cfg = state.tools_config.read().unwrap().clone();
                let workspace_path = state.workspace_base.replace("{agent_id}", &id);
                let workspace_root = std::path::PathBuf::from(workspace_path);
                let subtask_registry: opencrab_discord::SubtaskRegistry = std::sync::Arc::new(dashmap::DashMap::new());
                let completion_registry: opencrab_discord::CompletionRegistry = std::sync::Arc::new(dashmap::DashMap::new());
                let ga = opencrab_discord::DiscordGatewayActions::new(
                    http,
                    state.db.clone(),
                    id.clone(),
                    Arc::new(std::sync::RwLock::new(tools_cfg)),
                    None,
                    state.default_model.clone(),
                    workspace_root,
                    subtask_registry,
                    completion_registry,
                );
                Some(Arc::new(ga) as Arc<dyn opencrab_gateway::GatewayActions>)
            } else {
                None
            }
        } else {
            None
        }
    };
    #[cfg(not(feature = "discord"))]
    let gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> = None;

    // 6. Build agent context.
    let (system_prompt, agent_name) = {
        let conn = state.db.lock().unwrap();
        process::build_agent_context(&conn, &id)
    };

    // 7. Build conversation string.
    let conversation = {
        let conn = state.db.lock().unwrap();
        let budget = process::compute_context_budget(&conn, &state.default_model.split(':').next().unwrap_or(""), state.default_model.split(':').nth(1).unwrap_or(""), state.compaction_ratio);
        let raw = process::build_conversation_string(&conn, &session_id, &id, budget);
        process::prepend_runtime_context(&raw, "direct_message")
    };

    // 8. Run agent response.
    let result = process::run_agent_response(
        &state,
        &id,
        &agent_name,
        &session_id,
        &system_prompt,
        &conversation,
        "rest",
        gateway_actions,
        caller,
        &[],
        0,
        None,   // trigger_message_id
        None,
    )
    .await;

    // 9. Handle result.
    match result {
        Ok(engine_result) => {
            // Log agent response.
            let response_log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: id.clone(),
                session_id: session_id.clone(),
                log_type: "speech".to_string(),
                content: engine_result.response.clone(),
                speaker_id: Some(id.clone()),
                turn_number: None,
                metadata_json: Some(
                    serde_json::json!({
                        "iterations": engine_result.iterations,
                        "tool_calls_made": engine_result.tool_calls_made,
                    })
                    .to_string(),
                ),
            };
            {
                let conn = state.db.lock().unwrap();
                opencrab_db::queries::insert_session_log(&conn, &response_log).ok();
            }

            Json(serde_json::json!({
                "session_id": session_id,
                "caller_type": caller_type,
                "responses": [{
                    "agent_id": id,
                    "content": engine_result.response,
                }],
            }))
        }
        Err(e) => {
            tracing::error!(agent_id = %id, error = %e, "Agent response failed");
            Json(serde_json::json!({
                "session_id": session_id,
                "caller_type": caller_type,
                "responses": [{
                    "agent_id": id,
                    "content": format!("(Error: {})", e),
                }],
            }))
        }
    }
}
