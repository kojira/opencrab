use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub limit: Option<u32>,
    pub before: Option<String>,
}

fn page_limit(raw: Option<u32>) -> u32 {
    raw.unwrap_or(100).min(100)
}

pub async fn list_session_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Response {
    let conn = match state.db.lock() {
        Ok(conn) => conn,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "database unavailable"})),
            )
                .into_response();
        }
    };
    let session_id = match opencrab_db::queries::open_web_physical_session(&conn, &id) {
        Ok(Some(physical)) => physical,
        Ok(None) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let before_id = match q.before.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(n) => Some(n),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid before cursor"})),
                )
                    .into_response();
            }
        },
    };
    match opencrab_db::queries::list_session_logs_page(
        &conn,
        &session_id,
        page_limit(q.limit),
        before_id,
    ) {
        Ok(logs) => Json(logs).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

pub async fn list_sessions(State(state): State<AppState>, Query(q): Query<PageQuery>) -> Response {
    let conn = match state.db.lock() {
        Ok(conn) => conn,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "database unavailable"})),
            )
                .into_response();
        }
    };
    match opencrab_db::queries::list_sessions_page(&conn, page_limit(q.limit), q.before.as_deref())
    {
        Ok(items) => {
            let rows: Vec<serde_json::Value> = items
                .into_iter()
                .map(|i| {
                    let mut value =
                        serde_json::to_value(i.session).expect("SessionRow is serializable");
                    value["gateway_bound"] = serde_json::json!(i.gateway_bound);
                    value["agent_ids"] = serde_json::json!(i.agent_ids);
                    value
                })
                .collect();
            Json(rows).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.starts_with("unknown session cursor") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(serde_json::json!({"error": msg}))).into_response()
        }
    }
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
    Extension(extgate): Extension<Arc<opencrab_extgate::ExtgateState>>,
    Path(id): Path<String>,
) -> Response {
    let conn = match state.db.lock() {
        Ok(conn) => conn,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "database unavailable"})),
            )
                .into_response();
        }
    };
    match opencrab_db::queries::project_session_row(&conn, &id) {
        Ok(Some((session, gateway_bound))) => {
            let agent_ids = match opencrab_db::queries::effective_agent_ids(&conn, &id) {
                Ok(ids) => ids,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            };
            let mut value = serde_json::to_value(session).expect("SessionRow is serializable");
            value["gateway_bound"] = serde_json::json!(gateway_bound);
            value["agent_ids"] = serde_json::json!(agent_ids);
            if gateway_bound {
                match opencrab_db::queries::open_web_binding(&conn, &id) {
                    Ok(Some(b)) => match extgate.lock_registry() {
                        Ok(reg) => {
                            value["binding_address"] = serde_json::json!(b.address);
                            value["web_binding_state"] =
                                serde_json::json!(opencrab_extgate::web_binding_state(
                                    &reg,
                                    &b.instance_id,
                                    &b.binding_id
                                ));
                        }
                        Err(e) => {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({"error": e.code.as_str()})),
                            )
                                .into_response();
                        }
                    },
                    Ok(None) => {}
                    Err(e) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error": e.to_string()})),
                        )
                            .into_response();
                    }
                }
            }
            Json(value).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("session not found: {id}")})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
