use std::sync::Arc;

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;
use crate::llm_adapter::LlmRouterAdapter;

pub async fn list_sessions(
    State(state): State<AppState>,
) -> Json<Vec<opencrab_db::queries::SessionRow>> {
    let conn = state.db.lock().unwrap();
    let sessions = opencrab_db::queries::list_sessions(&conn).unwrap_or_default();
    Json(sessions)
}

#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    pub theme: String,
    pub mode: Option<String>,
    pub participant_ids: Vec<String>,
    pub max_turns: Option<i32>,
}

pub async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Json<serde_json::Value> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let session = opencrab_db::queries::SessionRow {
        id: session_id.clone(),
        mode: req.mode.unwrap_or_else(|| "autonomous".to_string()),
        theme: req.theme,
        phase: "divergent".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: serde_json::to_string(&req.participant_ids).unwrap(),
        facilitator_id: None,
        done_count: 0,
        max_turns: req.max_turns,
    };

    let conn = state.db.lock().unwrap();
    opencrab_db::queries::insert_session(&conn, &session).unwrap();

    Json(serde_json::json!({
        "id": session_id,
    }))
}

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let session = opencrab_db::queries::get_session(&conn, &id).unwrap();
    Json(serde_json::to_value(session).unwrap())
}

#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub agent_id: String,
    pub content: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Json<serde_json::Value> {
    // 1. Log the sender's message to DB.
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: req.agent_id.clone(),
        session_id: id.clone(),
        log_type: "speech".to_string(),
        content: req.content.clone(),
        speaker_id: Some(req.agent_id.clone()),
        turn_number: None,
        metadata_json: None,
    };

    let log_id = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::insert_session_log(&conn, &log).unwrap()
    };

    // 2. Check if LLM providers are available. If none, fall back to legacy behavior.
    if state.llm_router.provider_names().is_empty() {
        return Json(serde_json::json!({
            "id": log_id,
            "session_id": id,
        }));
    }

    // 3. Get session and participant IDs.
    let (participant_ids, session_theme) = {
        let conn = state.db.lock().unwrap();
        let session = opencrab_db::queries::get_session(&conn, &id)
            .unwrap()
            .unwrap();
        let ids: Vec<String> =
            serde_json::from_str(&session.participant_ids_json).unwrap_or_default();
        (ids, session.theme)
    };

    // 4. For each participant (except the sender), run SkillEngine.
    let mut responses = Vec::new();

    for agent_id in &participant_ids {
        if agent_id == &req.agent_id {
            continue;
        }

        // Build agent context from DB.
        let (system_prompt, agent_name) = {
            let conn = state.db.lock().unwrap();
            build_agent_context(&conn, agent_id, &session_theme)
        };

        // Build conversation history from session logs.
        let conversation = {
            let conn = state.db.lock().unwrap();
            build_conversation_string(&conn, &id, agent_id)
        };

        // Build workspace path for this agent.
        let ws_path = format!("{}/{}", state.workspace_base, agent_id);
        std::fs::create_dir_all(&ws_path).ok();
        let workspace =
            opencrab_core::workspace::Workspace::from_root(std::path::Path::new(&ws_path))
                .unwrap();

        // Create BridgedExecutor with ActionContext.
        let ctx = opencrab_actions::ActionContext {
            agent_id: agent_id.clone(),
            agent_name: agent_name.clone(),
            session_id: Some(id.clone()),
            db: state.db.clone(),
            workspace: Arc::new(workspace),
        };
        let dispatcher = opencrab_actions::ActionDispatcher::new();
        let executor = opencrab_actions::BridgedExecutor::new(dispatcher, ctx);

        // Create LlmRouterAdapter.
        let llm_client = LlmRouterAdapter::new(state.llm_router.clone());

        // Run SkillEngine.
        let engine = opencrab_core::SkillEngine::new(
            Box::new(llm_client),
            Box::new(executor),
            5, // max iterations
        );

        let default_model = "default";
        let result = engine.run(&system_prompt, &conversation, default_model).await;

        match result {
            Ok(engine_result) => {
                // Log the agent's response to DB.
                let response_log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: agent_id.clone(),
                    session_id: id.clone(),
                    log_type: "speech".to_string(),
                    content: engine_result.response.clone(),
                    speaker_id: Some(agent_id.clone()),
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

                responses.push(serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "content": engine_result.response,
                    "tool_calls_made": engine_result.tool_calls_made,
                }));
            }
            Err(e) => {
                tracing::error!(agent_id = %agent_id, error = %e, "SkillEngine failed");
                responses.push(serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "content": format!("(Error: {})", e),
                    "tool_calls_made": 0,
                }));
            }
        }
    }

    Json(serde_json::json!({
        "id": log_id,
        "session_id": id,
        "responses": responses,
    }))
}

/// Build a system prompt from the agent's identity, soul, and skills.
fn build_agent_context(
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_theme: &str,
) -> (String, String) {
    let identity = opencrab_db::queries::get_identity(conn, agent_id)
        .ok()
        .flatten();
    let soul = opencrab_db::queries::get_soul(conn, agent_id).ok().flatten();
    let skills = opencrab_db::queries::list_skills(conn, agent_id, true).unwrap_or_default();

    let agent_name = identity
        .as_ref()
        .map(|i| i.name.clone())
        .unwrap_or_else(|| agent_id.to_string());

    let role = identity
        .as_ref()
        .map(|i| i.role.clone())
        .unwrap_or_else(|| "discussant".to_string());

    let persona = soul
        .as_ref()
        .map(|s| s.persona_name.clone())
        .unwrap_or_default();

    let personality = soul
        .as_ref()
        .map(|s| s.personality_json.clone())
        .unwrap_or_else(|| "{}".to_string());

    let skills_text = if skills.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect();
        format!("\n\nYour skills:\n{}", list.join("\n"))
    };

    let prompt = format!(
        "You are {agent_name} ({persona}), role: {role}.\n\
         Personality: {personality}\n\
         Current discussion topic: {session_theme}\n\
         \n\
         You are an autonomous agent participating in a discussion. \
         Respond thoughtfully to the conversation. \
         You can use tools to search your history, learn from experience, \
         create new skills, and manage your workspace.{skills_text}"
    );

    (prompt, agent_name)
}

/// Build a conversation string from session logs for the agent to understand context.
fn build_conversation_string(
    conn: &rusqlite::Connection,
    session_id: &str,
    _current_agent_id: &str,
) -> String {
    let logs =
        opencrab_db::queries::list_session_logs_by_session(conn, session_id).unwrap_or_default();

    if logs.is_empty() {
        return "No messages yet.".to_string();
    }

    let mut parts = Vec::new();
    for log in &logs {
        let speaker = log
            .speaker_id
            .as_deref()
            .unwrap_or(&log.agent_id);
        parts.push(format!("[{}]: {}", speaker, log.content));
    }

    parts.join("\n")
}
