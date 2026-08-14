use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::process;
use crate::process::AgentNotFound;
use crate::AppState;

pub async fn list_session_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::SessionLogRow>> {
    let conn = state.db.lock().unwrap();
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, &id).unwrap_or_default();
    Json(logs)
}

#[derive(Debug, Deserialize)]
pub struct MentorInstructionRequest {
    pub content: String,
}

pub async fn send_mentor_instruction(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MentorInstructionRequest>,
) -> Json<serde_json::Value> {
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: "mentor".to_string(),
        session_id: id,
        log_type: "system".to_string(),
        content: req.content,
        speaker_id: Some("mentor".to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    let conn = state.db.lock().unwrap();
    let log_id = match opencrab_db::queries::insert_session_log(&conn, &log) {
        Ok(id) => id,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to record mentor instruction: {e}")
            }));
        }
    };
    Json(serde_json::json!({"id": log_id}))
}

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
        metadata_json: None,
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
) -> Response {
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
        created_at: None,
    };

    let log_id = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return Json(serde_json::json!({"error": "database unavailable"})).into_response();
            }
        };
        match opencrab_db::queries::insert_session_log(&conn, &log) {
            Ok(id) => id,
            Err(e) => {
                return Json(serde_json::json!({"error": format!("failed to log message: {e}")}))
                    .into_response();
            }
        }
    };

    // 2. Check if LLM providers are available. If none, fall back to legacy behavior.
    if state.llm_router.get().provider_names().is_empty() {
        return Json(serde_json::json!({
            "id": log_id,
            "session_id": id,
        }))
        .into_response();
    }

    // 3. Get session and participant IDs.
    let session = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return Json(serde_json::json!({"error": "database unavailable"})).into_response();
            }
        };
        match opencrab_db::queries::get_session(&conn, &id) {
            Ok(Some(session)) => session,
            Ok(None) => {
                return Json(serde_json::json!({"error": format!("session not found: {id}")}))
                    .into_response();
            }
            Err(e) => {
                return Json(serde_json::json!({"error": format!("failed to load session: {e}")}))
                    .into_response();
            }
        }
    };
    // 参加者は agent_sessions テーブルが正（#37）。JSON 列は wire 契約用の投影。
    let participant_ids: Vec<String> = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return Json(serde_json::json!({"error": "database unavailable"})).into_response()
            }
        };
        opencrab_db::queries::list_session_participants(&conn, &id).unwrap_or_default()
    };
    let session_theme = session.theme;

    // 4. For each participant (except the sender), run SkillEngine.
    let mut responses = Vec::new();

    for agent_id in &participant_ids {
        if agent_id == &req.agent_id {
            continue;
        }

        // Build agent context from DB. この経路の run は caller=Owner（下の RunRequest と
        // 一致）。Owner には全 skill を見せる（#352）。
        let (system_prompt, agent_name) = {
            let conn = state.db.lock().unwrap();
            process::build_agent_context(&conn, agent_id, &opencrab_actions::CallerIdentity::Owner)
        };

        // Build conversation history from session logs.
        let conversation = {
            let conn = state.db.lock().unwrap();
            let eff = opencrab_db::queries::effective_model_for_agent(
                &conn,
                agent_id,
                &state.default_model,
            )
            .unwrap_or_else(|_| state.default_model.clone());
            let (prov, mdl) = process::split_llm_model_spec(&eff);
            let budget = process::compute_context_budget(&conn, prov, mdl, state.compaction_ratio);
            let raw = match process::build_conversation_string(&conn, &id, agent_id, budget) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(agent_id = %agent_id, session_id = %id, "build_conversation_string failed: {e}");
                    continue;
                }
            };
            process::prepend_runtime_context(&raw, &session_theme)
        };

        // Run agent through the shared pipeline.
        let result = process::run_agent_response(
            &state,
            opencrab_actions::RunRequest::new(
                agent_id,
                &agent_name,
                &id,
                &system_prompt,
                &conversation,
                "rest",
                opencrab_actions::CallerIdentity::Owner,
            ),
        )
        .await;

        match result {
            Ok(engine_result) => {
                // Log the agent's response to DB.
                {
                    let conn = state.db.lock().unwrap();
                    crate::transcript::record_rest_agent_reply(
                        &conn,
                        agent_id,
                        &id,
                        &engine_result.response,
                        engine_result.iterations,
                        engine_result.tool_calls_made,
                    );
                }

                responses.push(serde_json::json!({
                    "agent_id": agent_id,
                    "agent_name": agent_name,
                    "content": engine_result.response,
                    "tool_calls_made": engine_result.tool_calls_made,
                }));
            }
            Err(e) => {
                // #632: 存在しない participant はチョークポイント（run_agent_response）で
                // 弾かれる。ターンは走らない。他 participant の結果を混ぜず 404 に写像する
                // （でたらめな participant を含むセッションへの send を無効な要求として扱う）。
                if let Some(nf) = e.downcast_ref::<AgentNotFound>() {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({"error": nf.to_string()})),
                    )
                        .into_response();
                }
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
    .into_response()
}
