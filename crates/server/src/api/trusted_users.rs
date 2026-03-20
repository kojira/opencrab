use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct TrustedUserDto {
    pub id: String,
    pub discord_user_id: String,
    pub agent_id: String,
    pub permission: String,
    pub created_by: String,
    pub created_at: String,
}

fn row_to_dto(r: opencrab_db::queries::TrustedDiscordUserRow) -> TrustedUserDto {
    TrustedUserDto {
        id: r.id,
        discord_user_id: r.discord_user_id,
        agent_id: r.agent_id,
        permission: r.permission,
        created_by: r.created_by,
        created_at: r.created_at,
    }
}

pub async fn list_trusted_users(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Json<Vec<TrustedUserDto>> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_trusted_users(&conn, &agent_id).unwrap_or_default();
    Json(rows.into_iter().map(row_to_dto).collect())
}

#[derive(Debug, Deserialize)]
pub struct AddTrustedUserRequest {
    pub discord_user_id: String,
    pub permission: Option<String>,
}

pub async fn add_trusted_user(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddTrustedUserRequest>,
) -> Result<Json<TrustedUserDto>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let permission = req.permission.unwrap_or_else(|| "user".to_string());

    opencrab_db::queries::add_trusted_user(
        &conn,
        &id,
        &agent_id,
        &req.discord_user_id,
        &permission,
        "owner",
        &now,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(TrustedUserDto {
        id,
        discord_user_id: req.discord_user_id,
        agent_id,
        permission,
        created_by: "owner".to_string(),
        created_at: now,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateTrustedUserRequest {
    pub permission: String,
}

pub async fn update_trusted_user(
    State(state): State<AppState>,
    Path((_agent_id, user_id)): Path<(String, String)>,
    Json(req): Json<UpdateTrustedUserRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let updated = opencrab_db::queries::update_trusted_user_permission(&conn, &user_id, &req.permission)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "updated": updated })))
}

pub async fn delete_trusted_user(
    State(state): State<AppState>,
    Path((_agent_id, user_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::remove_trusted_user(&conn, &user_id).unwrap_or(false);
    Json(serde_json::json!({ "deleted": deleted }))
}
