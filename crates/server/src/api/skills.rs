use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

pub async fn list_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::SkillRow>> {
    let conn = state.db.lock().unwrap();
    let skills = opencrab_db::queries::list_skills(&conn, &id, false).unwrap_or_default();
    Json(skills)
}

#[derive(Debug, Deserialize)]
pub struct AddSkillRequest {
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
}

pub async fn add_skill(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AddSkillRequest>,
) -> Json<serde_json::Value> {
    let skill_id = uuid::Uuid::new_v4().to_string();
    let skill = opencrab_db::queries::SkillRow {
        id: skill_id.clone(),
        agent_id: id,
        name: req.name,
        description: req.description,
        situation_pattern: req.situation_pattern,
        guidance: req.guidance,
        source_type: "manual".to_string(),
        source_context: None,
        file_path: None,
        effectiveness: None,
        usage_count: 0,
        is_active: true,
    };

    let conn = state.db.lock().unwrap();
    opencrab_db::queries::insert_skill(&conn, &skill).unwrap();

    Json(serde_json::json!({
        "id": skill_id,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ToggleSkillRequest {
    pub active: bool,
}

pub async fn toggle_skill(
    State(state): State<AppState>,
    Path((_, skill_id)): Path<(String, String)>,
    Json(req): Json<ToggleSkillRequest>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::set_skill_active(&conn, &skill_id, req.active).unwrap();
    Json(serde_json::json!({"toggled": true}))
}
