use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListSkillsQuery {
    pub include_archived: Option<bool>,
}

pub async fn list_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::SkillRow>> {
    let conn = state.db.lock().unwrap();
    let skills = opencrab_db::queries::list_skills(&conn, &id, false).unwrap_or_default();
    Json(skills)
}

pub async fn list_skills_all(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ListSkillsQuery>,
) -> Json<Vec<opencrab_db::queries::SkillRow>> {
    let conn = state.db.lock().unwrap();
    let include_archived = q.include_archived.unwrap_or(false);
    let skills = opencrab_db::queries::list_skills_filtered(&conn, &id, false, include_archived)
        .unwrap_or_default();
    Json(skills)
}

#[derive(Debug, Deserialize)]
pub struct AddSkillRequest {
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
    pub permission: Option<String>,
    /// caller=Agent のターンへ露出してよいか（#352）。省略時は false（fail-closed）。
    pub agent_visible: Option<bool>,
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
        permission: req.permission.unwrap_or_else(|| "\"agent\"".to_string()),
        archived: false,
        // #335: REST の手動追加はオーナーのダッシュボード由来。None = legacy grandfather
        // （Owner 相当）。
        created_caller: None,
        // #352: 新規追加は Agent 非露出が既定（fail-closed）。露出は update_skill で切り替える。
        agent_visible: req.agent_visible.unwrap_or(false),
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

#[derive(Debug, Deserialize)]
pub struct UpdateSkillRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub guidance: Option<String>,
    pub situation_pattern: Option<String>,
    /// caller=Agent のターンへ露出してよいか（#352）。
    ///
    /// この REST 経路はダッシュボード（オーナー操作）専用。エージェントのアクションは
    /// HTTP ではなく action dispatch を通るため、ここには到達できない
    /// （＝caller=Agent から自分でこのフラグを立てられない）。
    pub agent_visible: Option<bool>,
}

pub async fn update_skill(
    State(state): State<AppState>,
    Path((agent_id, skill_id)): Path<(String, String)>,
    Json(req): Json<UpdateSkillRequest>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let skills = opencrab_db::queries::list_skills_filtered(&conn, &agent_id, false, true)
        .unwrap_or_default();
    let existing = skills.into_iter().find(|s| s.id == skill_id);

    if let Some(mut skill) = existing {
        if let Some(name) = req.name {
            skill.name = name;
        }
        if let Some(desc) = req.description {
            skill.description = desc;
        }
        if let Some(guidance) = req.guidance {
            skill.guidance = guidance;
        }
        if let Some(pattern) = req.situation_pattern {
            skill.situation_pattern = pattern;
        }
        if let Some(agent_visible) = req.agent_visible {
            skill.agent_visible = agent_visible;
        }

        opencrab_db::queries::update_skill(&conn, &skill).unwrap();
        Json(serde_json::json!({"updated": true}))
    } else {
        Json(serde_json::json!({"updated": false, "error": "skill not found"}))
    }
}

pub async fn archive_skill(
    State(state): State<AppState>,
    Path((_, skill_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::archive_skill(&conn, &skill_id, true).unwrap();
    Json(serde_json::json!({"archived": true}))
}

pub async fn restore_skill(
    State(state): State<AppState>,
    Path((_, skill_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::archive_skill(&conn, &skill_id, false).unwrap();
    Json(serde_json::json!({"restored": true}))
}

pub async fn list_unused(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Vec<opencrab_db::queries::SkillRow>> {
    let conn = state.db.lock().unwrap();
    let skills = opencrab_db::queries::find_unused_skills(&conn, &id, 7).unwrap_or_default();
    Json(skills)
}
