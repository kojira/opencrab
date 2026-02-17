use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub persona_name: String,
    pub role: String,
    pub status: String,
}

pub async fn list_agents(State(state): State<AppState>) -> Json<Vec<AgentSummary>> {
    let conn = state.db.lock().unwrap();
    // JOIN soul and identity to get agent summaries
    let mut stmt = conn
        .prepare(
            "SELECT i.agent_id, i.name, COALESCE(s.persona_name, ''), i.role
             FROM identity i
             LEFT JOIN soul s ON i.agent_id = s.agent_id
             ORDER BY i.name",
        )
        .unwrap();

    let agents = stmt
        .query_map([], |row| {
            Ok(AgentSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                persona_name: row.get(2)?,
                role: row.get(3)?,
                status: "idle".to_string(),
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    Json(agents)
}

#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub name: String,
    pub persona_name: String,
    pub role: Option<String>,
}

pub async fn create_agent(
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Json<serde_json::Value> {
    let agent_id = uuid::Uuid::new_v4().to_string();
    let conn = state.db.lock().unwrap();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: agent_id.clone(),
        name: req.name.clone(),
        role: req.role.unwrap_or_else(|| "discussant".to_string()),
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();

    let soul = opencrab_db::queries::SoulRow {
        agent_id: agent_id.clone(),
        persona_name: req.persona_name,
        social_style_json: serde_json::json!({"assertiveness": 0.0, "responsiveness": 0.0, "style_name": "Analytical"}).to_string(),
        personality_json: serde_json::json!({"openness": 0.5, "conscientiousness": 0.5, "extraversion": 0.5, "agreeableness": 0.5, "neuroticism": 0.0}).to_string(),
        thinking_style_json: serde_json::json!({"primary": "論理的", "secondary": "分析的", "description": ""}).to_string(),
        custom_traits_json: None,
    };
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();

    Json(serde_json::json!({
        "id": agent_id,
        "name": req.name,
    }))
}

pub async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();

    let identity = opencrab_db::queries::get_identity(&conn, &id).unwrap();
    let soul = opencrab_db::queries::get_soul(&conn, &id).unwrap();

    Json(serde_json::json!({
        "identity": identity,
        "soul": soul,
    }))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    conn.execute("DELETE FROM identity WHERE agent_id = ?1", [&id])
        .unwrap();
    conn.execute("DELETE FROM soul WHERE agent_id = ?1", [&id])
        .unwrap();
    conn.execute("DELETE FROM skills WHERE agent_id = ?1", [&id])
        .unwrap();

    Json(serde_json::json!({"deleted": true}))
}

pub async fn get_soul(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let soul = opencrab_db::queries::get_soul(&conn, &id).unwrap();
    Json(serde_json::to_value(soul).unwrap())
}

pub async fn update_soul(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(soul): Json<opencrab_db::queries::SoulRow>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let mut soul = soul;
    soul.agent_id = id;
    opencrab_db::queries::upsert_soul(&conn, &soul).unwrap();
    Json(serde_json::json!({"updated": true}))
}

pub async fn get_identity(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let identity = opencrab_db::queries::get_identity(&conn, &id).unwrap();
    Json(serde_json::to_value(identity).unwrap())
}

pub async fn update_identity(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(identity): Json<opencrab_db::queries::IdentityRow>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let mut identity = identity;
    identity.agent_id = id;
    opencrab_db::queries::upsert_identity(&conn, &identity).unwrap();
    Json(serde_json::json!({"updated": true}))
}
