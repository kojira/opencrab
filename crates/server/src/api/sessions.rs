use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

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
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: req.agent_id.clone(),
        session_id: id.clone(),
        log_type: "speech".to_string(),
        content: req.content.clone(),
        speaker_id: Some(req.agent_id),
        turn_number: None,
        metadata_json: None,
    };

    let conn = state.db.lock().unwrap();
    let log_id = opencrab_db::queries::insert_session_log(&conn, &log).unwrap();

    Json(serde_json::json!({
        "id": log_id,
        "session_id": id,
    }))
}
