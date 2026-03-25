use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct CoAgentDto {
    pub id: String,
    pub agent_id: String,
    pub co_agent_id: String,
    pub allowed_actions: Option<Vec<String>>,
    pub created_by: String,
    pub created_at: String,
}

fn row_to_dto(r: opencrab_db::queries::TrustedCoAgentRow) -> CoAgentDto {
    let allowed_actions = r
        .allowed_actions
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());
    CoAgentDto {
        id: r.id,
        agent_id: r.agent_id,
        co_agent_id: r.co_agent_id,
        allowed_actions,
        created_by: r.created_by,
        created_at: r.created_at,
    }
}

pub async fn list_co_agents(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Json<Vec<CoAgentDto>> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_trusted_co_agents(&conn, &agent_id).unwrap_or_default();
    Json(rows.into_iter().map(row_to_dto).collect())
}

#[derive(Debug, Deserialize)]
pub struct AddCoAgentRequest {
    pub co_agent_id: String,
    pub allowed_actions: Option<Vec<String>>,
}

pub async fn add_co_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddCoAgentRequest>,
) -> Result<Json<CoAgentDto>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let actions_json = req
        .allowed_actions
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    let row = opencrab_db::queries::TrustedCoAgentRow {
        id: id.clone(),
        agent_id: agent_id.clone(),
        co_agent_id: req.co_agent_id.clone(),
        allowed_actions: actions_json.clone(),
        created_by: "owner".to_string(),
        created_at: now.clone(),
    };

    opencrab_db::queries::insert_trusted_co_agent(&conn, &row)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(CoAgentDto {
        id,
        agent_id,
        co_agent_id: req.co_agent_id,
        allowed_actions: req.allowed_actions,
        created_by: "owner".to_string(),
        created_at: now,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateCoAgentRequest {
    pub allowed_actions: Option<Vec<String>>,
}

pub async fn update_co_agent(
    State(state): State<AppState>,
    Path((agent_id, co_agent_id)): Path<(String, String)>,
    Json(req): Json<UpdateCoAgentRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let actions_json = req
        .allowed_actions
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    let updated = opencrab_db::queries::update_trusted_co_agent_actions(
        &conn,
        &agent_id,
        &co_agent_id,
        actions_json.as_deref(),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({ "updated": updated })))
}

pub async fn delete_co_agent(
    State(state): State<AppState>,
    Path((agent_id, co_agent_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::delete_trusted_co_agent(&conn, &agent_id, &co_agent_id)
        .unwrap_or(false);
    Json(serde_json::json!({ "deleted": deleted }))
}
