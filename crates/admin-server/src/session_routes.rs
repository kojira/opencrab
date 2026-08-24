//! Sessions create + mentor（DESIGN-DASHBOARD-P2 SLICE 6）。
//! handler は extract → store コマンド 1 回 → 本体封筒。SQL は書かない。

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use opencrab_store::{PlaceCreateLegacy, PlaceCreateLegacyError, PrivateJournalError};

use crate::api::{AdminState, ApiResult};

const CREATE_UNAPPLIED: &[&str] = &[
    "id",
    "phase",
    "turn_number",
    "status",
    "facilitator_id",
    "done_count",
    "metadata_json",
    "participant_ids_json",
];

const MENTOR_UNAPPLIED: &[&str] = &[
    "agent_id",
    "session_id",
    "log_type",
    "speaker_id",
    "turn_number",
    "metadata_json",
];

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos() as i64
}

fn parse_place(id: &str) -> ApiResult<i64> {
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

/// 値つきで明示された未復元キーだけ 501。JSON null は未提供。
fn reject_unapplied_valued_fields(body: &Value, fields: &[&str]) -> ApiResult<()> {
    let present: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|name| body.get(*name).is_some_and(|value| !value.is_null()))
        .collect();
    if present.is_empty() {
        return Ok(());
    }
    Err((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "unimplemented",
            "detail": format!(
                "this route does not yet apply {}: see roadmap #772",
                present.join(", ")
            ),
            "fields": present,
        })),
    ))
}

fn decode_body<T: DeserializeOwned>(body: Value) -> ApiResult<T> {
    serde_json::from_value(body).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "bad_request", "detail": e.to_string() })),
        )
    })
}

fn create_err(error: PlaceCreateLegacyError) -> (StatusCode, Json<Value>) {
    match error {
        PlaceCreateLegacyError::UnresolvedParticipant(id) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "unresolved_participant",
                "detail": format!("unresolved participant: {id}"),
                "participant_id": id,
            })),
        ),
        PlaceCreateLegacyError::DuplicateParticipant(id) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "unresolved_participant",
                "detail": format!("duplicate participant: {id}"),
                "participant_id": id,
            })),
        ),
        PlaceCreateLegacyError::Store(e) => store_err(e),
    }
}

fn mentor_err(error: PrivateJournalError) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "error": format!("Failed to record mentor instruction: {error}")
        })),
    )
}

/// d8f3d7f `sessions.rs:64-67`
#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    pub theme: String,
    pub mode: Option<String>,
    pub participant_ids: Vec<String>,
    pub max_turns: Option<i32>,
}

/// d8f3d7f `sessions.rs:23-25`
#[derive(Debug, Deserialize)]
struct MentorInstructionRequest {
    pub content: String,
}

async fn create_session(
    State(st): State<AdminState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    reject_unapplied_valued_fields(&body, CREATE_UNAPPLIED)?;
    let req: CreateSessionRequest = decode_body(body)?;
    let id = st
        .store
        .place_create_legacy(
            &PlaceCreateLegacy {
                theme: req.theme,
                mode: req.mode,
                participant_ids: req.participant_ids,
                max_turns: req.max_turns.map(i64::from),
            },
            now_ns(),
        )
        .map_err(create_err)?;
    Ok(Json(json!({ "id": id.to_string() })))
}

async fn send_mentor_instruction(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    reject_unapplied_valued_fields(&body, MENTOR_UNAPPLIED)?;
    let req: MentorInstructionRequest = decode_body(body)?;
    let place = parse_place(&id)?;
    let log_id = st
        .store
        .private_journal_append_mentor(place, &req.content, now_ns())
        .map_err(mentor_err)?;
    Ok(Json(json!({ "id": log_id })))
}

async fn send_message_unimpl() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "unimplemented",
            "detail": "POST /api/sessions/{id}/messages: unrestored (conversation send; P2 slice 7 / web-gate)",
        })),
    )
}

pub fn session_write_routes() -> Router<AdminState> {
    Router::new()
        .route("/api/sessions", post(create_session))
        .route("/api/sessions/{id}/mentor", post(send_mentor_instruction))
        .route("/api/sessions/{id}/messages", post(send_message_unimpl))
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

    async fn create_ada(state: AdminState) -> (AdminState, String) {
        let (status, created) = call(
            state.clone(),
            "POST",
            "/api/agents",
            Some(json!({"name":"Ada","persona_name":"Helper"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        (state, created["id"].as_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn create_session_returns_id_and_lists() {
        let (state, agent) =
            create_ada(state_from_store(Store::new_in_memory().expect("store"))).await;
        let (status, body) = call(
            state.clone(),
            "POST",
            "/api/sessions",
            Some(json!({
                "theme": "fixture-theme",
                "participant_ids": [agent],
                "max_turns": 4
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["id"].as_str().expect("id string");
        assert!(id.parse::<i64>().is_ok(), "{body}");
        assert_eq!(body, json!({"id": id}));

        let (status, listed) = call(state.clone(), "GET", "/api/sessions", None).await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let row = listed
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap();
        assert_eq!(row["theme"], "fixture-theme");
        assert_eq!(row["participant_ids_json"], json!([agent]).to_string());

        let (status, got) = call(state, "GET", &format!("/api/sessions/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{got}");
        assert_eq!(got["id"], id);
        assert_eq!(got["theme"], "fixture-theme");
    }

    #[tokio::test]
    async fn unresolved_participant_is_400() {
        let (state, agent) =
            create_ada(state_from_store(Store::new_in_memory().expect("store"))).await;
        let (status, body) = call(
            state,
            "POST",
            "/api/sessions",
            Some(json!({
                "theme": "nope",
                "participant_ids": [agent, "99"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "unresolved_participant");
        assert_eq!(body["participant_id"], "99");
    }

    #[tokio::test]
    async fn valued_unrestored_create_field_is_501() {
        let (state, agent) =
            create_ada(state_from_store(Store::new_in_memory().expect("store"))).await;
        let (status, body) = call(
            state,
            "POST",
            "/api/sessions",
            Some(json!({
                "theme": "x",
                "participant_ids": [agent],
                "metadata_json": {"k":"v"}
            })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
        assert_eq!(body["fields"], json!(["metadata_json"]));
    }

    #[tokio::test]
    async fn mentor_returns_id_and_is_not_on_events() {
        let (state, agent) =
            create_ada(state_from_store(Store::new_in_memory().expect("store"))).await;
        let (status, created) = call(
            state.clone(),
            "POST",
            "/api/sessions",
            Some(json!({"theme":"m","participant_ids":[agent]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let id = created["id"].as_str().unwrap();

        let (status, body) = call(
            state.clone(),
            "POST",
            &format!("/api/sessions/{id}/mentor"),
            Some(json!({"content":"do this"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!({"id": 1}));

        let (status, logs) = call(state, "GET", &format!("/api/sessions/{id}/logs"), None).await;
        assert_eq!(status, StatusCode::OK, "{logs}");
        assert_eq!(logs, json!([]));
    }

    #[tokio::test]
    async fn mentor_missing_place_keeps_legacy_error_envelope() {
        let store = Store::new_in_memory().expect("store");
        let state = state_from_store(store);
        let (status, body) = call(
            state,
            "POST",
            "/api/sessions/99/mentor",
            Some(json!({"content":"x"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            json!({"error": "Failed to record mentor instruction: place not found"})
        );
    }

    #[tokio::test]
    async fn session_messages_post_is_501() {
        let store = Store::new_in_memory().expect("store");
        let (status, body) = call(
            state_from_store(store),
            "POST",
            "/api/sessions/1/messages",
            Some(json!({"agent_id":"1","content":"hi"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
    }
}
