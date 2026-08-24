//! Skills CRUD / toggle / archive / seed（DESIGN-DASHBOARD-P2 SLICE 4）。
//! handler は extract → store コマンド 1 回 → 本体封筒。SQL は書かない。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use opencrab_store::{SkillCommandError, SkillCreate, SkillPatch, SkillView};

use crate::api::{AdminState, ApiResult};

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as i64
}

fn parse_agent(id: &str) -> ApiResult<i64> {
    id.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "bad_id",
                "detail": "id は整数（subject/place の内部 ID）である必要があります",
            })),
        )
    })
}

fn store_err(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "store_error", "detail": e.to_string() })),
    )
}

fn skill_err(error: SkillCommandError) -> (StatusCode, Json<Value>) {
    match error {
        SkillCommandError::AgentMissing => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not_found", "detail": error.to_string() })),
        ),
        SkillCommandError::SkillsDir(detail) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "store_error", "detail": detail })),
        ),
        SkillCommandError::Store(error) => store_err(error),
    }
}

fn skills_dir() -> PathBuf {
    std::env::var("OPENCRAB_SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("skills"))
}

fn skill_json(row: SkillView) -> Value {
    json!({
        "id": row.skill_id,
        "agent_id": row.owner_subject_id.to_string(),
        "name": row.name,
        "description": row.description,
        "situation_pattern": row.situation_pattern,
        "guidance": row.guidance,
        "source_type": row.source_type,
        "source_context": row.source_context,
        "file_path": row.source_relative_path,
        "effectiveness": row.effectiveness,
        "usage_count": row.usage_count,
        "is_active": row.active,
        "permission": row.permission,
        "archived": row.archived,
        "created_caller": row.created_by_principal,
        "agent_visible": row.visible_to_agent,
    })
}

#[derive(Debug, Deserialize)]
struct AddSkillRequest {
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
    pub permission: Option<String>,
    pub agent_visible: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ToggleSkillRequest {
    pub active: bool,
}

#[derive(Debug, Deserialize)]
struct UpdateSkillRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub guidance: Option<String>,
    pub situation_pattern: Option<String>,
    pub agent_visible: Option<bool>,
}

async fn list_skills(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    let rows = st
        .store
        .skill_list(agent, false, false)
        .map_err(skill_err)?;
    Ok(Json(Value::Array(
        rows.into_iter().map(skill_json).collect(),
    )))
}

async fn add_skill(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<AddSkillRequest>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    let skill_id = st
        .store
        .skill_create(
            agent,
            &SkillCreate {
                name: req.name,
                description: req.description,
                situation_pattern: req.situation_pattern,
                guidance: req.guidance,
                permission: req.permission,
                visible_to_agent: req.agent_visible,
            },
            now_ns(),
        )
        .map_err(skill_err)?;
    Ok(Json(json!({ "id": skill_id })))
}

async fn update_skill(
    State(st): State<AdminState>,
    Path((agent_id, skill_id)): Path<(String, String)>,
    Json(req): Json<UpdateSkillRequest>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&agent_id)?;
    let updated = st
        .store
        .skill_update(
            agent,
            &skill_id,
            &SkillPatch {
                name: req.name,
                description: req.description,
                guidance: req.guidance,
                situation_pattern: req.situation_pattern,
                visible_to_agent: req.agent_visible,
            },
            now_ns(),
        )
        .map_err(skill_err)?;
    if updated {
        Ok(Json(json!({ "updated": true })))
    } else {
        Ok(Json(
            json!({ "updated": false, "error": "skill not found" }),
        ))
    }
}

async fn toggle_skill(
    State(st): State<AdminState>,
    Path((_, skill_id)): Path<(String, String)>,
    Json(req): Json<ToggleSkillRequest>,
) -> ApiResult<Json<Value>> {
    st.store
        .skill_set_active(&skill_id, req.active, now_ns())
        .map_err(skill_err)?;
    Ok(Json(json!({ "toggled": true })))
}

async fn archive_skill(
    State(st): State<AdminState>,
    Path((_, skill_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    st.store
        .skill_archive(&skill_id, true, now_ns())
        .map_err(skill_err)?;
    Ok(Json(json!({ "archived": true })))
}

async fn restore_skill(
    State(st): State<AdminState>,
    Path((_, skill_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    st.store
        .skill_archive(&skill_id, false, now_ns())
        .map_err(skill_err)?;
    Ok(Json(json!({ "restored": true })))
}

async fn seed_standard_skills(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let agent = parse_agent(&id)?;
    let result = st
        .store
        .skill_seed_standard(agent, &skills_dir(), now_ns())
        .map_err(skill_err)?;
    Ok(Json(json!({
        "seeded": result.seeded,
        "skipped": result.skipped,
        "errors": result.errors,
        "seeded_count": result.seeded.len(),
    })))
}

pub fn skill_write_routes() -> Router<AdminState> {
    Router::new()
        .route("/api/agents/{id}/skills", get(list_skills).post(add_skill))
        .route(
            "/api/agents/{id}/skills/seed-standard",
            axum::routing::post(seed_standard_skills),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}",
            axum::routing::put(update_skill),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}/toggle",
            axum::routing::post(toggle_skill),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}/archive",
            axum::routing::post(archive_skill),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}/restore",
            axum::routing::post(restore_skill),
        )
}

#[cfg(test)]
mod contract {
    use super::*;
    use crate::api::{create_router, AdminState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use opencrab_db::Db;
    use opencrab_store::Store;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn dummy_db() -> Arc<Db> {
        Arc::new(Db::from_connection(
            rusqlite::Connection::open_in_memory().expect("memory db"),
        ))
    }

    fn state_from_store(store: Store) -> AdminState {
        AdminState {
            store: Arc::new(store),
            db: dummy_db(),
            compaction_ratio: 0.5,
        }
    }

    async fn call(
        state: AdminState,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(match body {
                Some(value) => Body::from(serde_json::to_vec(&value).expect("json")),
                None => Body::empty(),
            })
            .expect("request");
        let response = create_router(state)
            .oneshot(request)
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn skills_crud_toggle_archive_body_envelopes() {
        let state = state_from_store(Store::new_in_memory().expect("store"));
        let (_, created) = call(
            state.clone(),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        let id = created["id"].as_str().unwrap().to_string();

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/skills"),
            Some(json!({
                "name":"Deploy",
                "description":"ship",
                "situation_pattern":"when shipping",
                "guidance":"do it",
                "agent_visible": true
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let skill_id = body["id"].as_str().unwrap().to_string();

        let (status, listed) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{id}/skills"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        assert_eq!(listed[0]["name"], "Deploy");
        assert_eq!(listed[0]["is_active"], true);
        assert_eq!(listed[0]["agent_visible"], true);

        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{id}/skills/{skill_id}"),
            Some(json!({"name":"Ship"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"updated": true}));

        let (status, body) = call(
            state.clone(),
            "PUT",
            &format!("/api/agents/{id}/skills/missing"),
            Some(json!({"name":"X"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"updated": false, "error": "skill not found"}));

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/skills/{skill_id}/toggle"),
            Some(json!({"active": false})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"toggled": true}));

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/agents/{id}/skills/{skill_id}/archive"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"archived": true}));
        let (_, listed) = call(
            state.clone(),
            "GET",
            &format!("/api/agents/{id}/skills"),
            None,
        )
        .await;
        assert_eq!(listed, json!([]));

        let (status, body) = call(
            state,
            "POST",
            &format!("/api/agents/{id}/skills/{skill_id}/restore"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"restored": true}));
    }

    #[tokio::test]
    async fn unused_stays_501() {
        let state = state_from_store(Store::new_in_memory().expect("store"));
        let (status, body) = call(state, "GET", "/api/agents/1/skills/unused", None).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
    }
}
