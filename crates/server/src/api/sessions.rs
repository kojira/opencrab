use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;
use crate::process;

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
            process::build_agent_context(&conn, agent_id, &session_theme)
        };

        // Build conversation history from session logs.
        let conversation = {
            let conn = state.db.lock().unwrap();
            process::build_conversation_string(&conn, &id)
        };

        // Run agent through the shared pipeline.
        let result = process::run_agent_response(
            &state,
            agent_id,
            &agent_name,
            &id,
            &system_prompt,
            &conversation,
            "rest",
            None,
        )
        .await;

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

