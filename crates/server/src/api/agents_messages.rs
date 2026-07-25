use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::process;
use crate::AppState;

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
    // 呼び出し元 ID は入口で 1 回だけ正規化し、以降すべて（認可・セッションキー・
    // speaker_id）で同じ値を使う。`is_owner_id` が trim して比較する一方でセッション
    // キーだけ生値を使うと、`" <id> "` が owner にはなれるのに別セッション・別
    // speaker_id として記録される非対称が生まれる。
    let user_id = req.user_id.trim();
    let session_id = format!("agent-msg-{}-{}", id, user_id);

    // 1. Determine caller identity from trusted_users table.
    let caller = {
        let conn = state.db.lock().unwrap();
        match opencrab_db::queries::get_trusted_user(&conn, user_id, &id) {
            Some(u) if u.permission == "co_agent" => opencrab_actions::CallerIdentity::CoAgent {
                agent_id: user_id.to_string(),
            },
            Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
            None => {
                let cfg = opencrab_db::queries::get_agent_discord_config(&conn, &id);
                if let Ok(Some(c)) = cfg {
                    if crate::api::is_owner_id(&c.owner_discord_id, user_id) {
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
                return Json(
                    serde_json::json!({"error": format!("Failed to create session: {}", e)}),
                );
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
            speaker_id: Some(user_id.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        let conn = state.db.lock().unwrap();
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            return Json(serde_json::json!({"error": format!("Failed to log message: {}", e)}));
        }
    }

    // 4. Check LLM availability.
    if state.llm_router.get().provider_names().is_empty() {
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
            if let Some(http) = dm.get_http_for_agent(&id) {
                let tools_cfg = state.tools_config.read().unwrap().clone();
                let subtask_registry: opencrab_discord::SubtaskRegistry =
                    std::sync::Arc::new(dashmap::DashMap::new());
                let ga = opencrab_discord::DiscordGatewayActions::new(
                    http,
                    state.db.clone(),
                    Arc::new(std::sync::RwLock::new(tools_cfg)),
                    None,
                    state.default_model.clone(),
                    state.workspace_base.clone(),
                    subtask_registry,
                    None,
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
        let eff = opencrab_db::queries::effective_model_for_agent(&conn, &id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone());
        let (prov, mdl) = process::split_llm_model_spec(&eff);
        let budget = process::compute_context_budget(&conn, prov, mdl, state.compaction_ratio);
        let raw = match process::build_conversation_string(&conn, &session_id, &id, budget) {
            Ok(s) => s,
            Err(e) => {
                return Json(
                    serde_json::json!({"error": format!("Failed to build conversation: {}", e)}),
                );
            }
        };
        process::prepend_runtime_context(&raw, "direct_message")
    };

    // 8. Run agent response.
    let mut run_req = opencrab_actions::RunRequest::new(
        &id,
        &agent_name,
        &session_id,
        &system_prompt,
        &conversation,
        "rest",
        caller,
    );
    if let Some(ga) = gateway_actions {
        run_req = run_req.with_gateway_actions(ga);
    }
    let result = process::run_agent_response(&state, run_req).await;

    // 9. Handle result.
    match result {
        Ok(engine_result) => {
            // Log agent response.
            {
                let conn = state.db.lock().unwrap();
                crate::transcript::record_rest_agent_reply(
                    &conn,
                    &id,
                    &session_id,
                    &engine_result.response,
                    engine_result.iterations,
                    engine_result.tool_calls_made,
                );
            }

            // Mark session as completed after agent responds
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE sessions SET status = 'completed' WHERE id = ?1",
                    [&session_id],
                )
                .ok();
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
            // Mark session as completed after agent responds
            {
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "UPDATE sessions SET status = 'completed' WHERE id = ?1",
                    [&session_id],
                )
                .ok();
            }
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
