//! DESIGN-WEBGATE §7: 新規 Web 会話作成。V3 admin 第 7 口にはしない。

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use crate::AppState;

const BIND_WAIT: Duration = Duration::from_secs(60);
const WEB_NS: uuid::Uuid = uuid::Uuid::NAMESPACE_DNS;

type Fail = (StatusCode, Json<Value>);

fn json_err(status: StatusCode, error: &str) -> Fail {
    (status, Json(json!({"error": error})))
}

fn parse_body(bytes: &[u8]) -> Result<Option<String>, Fail> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| json_err(StatusCode::BAD_REQUEST, "invalid json"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| json_err(StatusCode::BAD_REQUEST, "body must be an object"))?;
    for key in obj.keys() {
        if key != "name" {
            return Err(json_err(
                StatusCode::BAD_REQUEST,
                "caller-specified ids are not accepted",
            ));
        }
    }
    match obj.get("name") {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(json_err(StatusCode::BAD_REQUEST, "name must be a string")),
    }
}

fn normalize_name(raw: &str) -> Result<Option<String>, Fail> {
    let trimmed: String = raw.trim_matches(char::is_whitespace).to_string();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(json_err(
            StatusCode::BAD_REQUEST,
            "name must not contain newlines",
        ));
    }
    if trimmed.chars().count() > 100 {
        return Err(json_err(
            StatusCode::BAD_REQUEST,
            "name must be at most 100 unicode scalar values",
        ));
    }
    Ok(Some(trimmed))
}

fn resolve_web_instance(conn: &rusqlite::Connection, agent_id: &str) -> Result<String, Fail> {
    let subject: Option<i64> = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let Some(subject) = subject else {
        // §7.3 手順 1: agent 解決失敗も instance 0 件と同じ 409。agent 専用 404 は置かない。
        return Err(json_err(StatusCode::CONFLICT, "web_instance_unavailable"));
    };
    if subject <= 0 {
        return Err(json_err(StatusCode::CONFLICT, "web_instance_unavailable"));
    }
    let mut stmt = conn
        .prepare(
            "SELECT instance_id FROM gate_instances
             WHERE subject_id = ?1 AND kind_id = 'web' AND enabled = 1 AND deleted_at IS NULL",
        )
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let ids: Vec<String> = stmt
        .query_map([subject], |r| r.get(0))
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    match ids.as_slice() {
        [id] => Ok(id.clone()),
        _ => Err(json_err(StatusCode::CONFLICT, "web_instance_unavailable")),
    }
}

fn conversation_ids(agent_id: &str) -> (String, String, String) {
    let conversation_id = uuid::Uuid::new_v4().to_string();
    let session_id = format!("web-{agent_id}-{conversation_id}");
    let binding_id = uuid::Uuid::new_v5(
        &WEB_NS,
        format!("opencrab:web:binding:{session_id}").as_bytes(),
    )
    .to_string();
    (conversation_id, session_id, binding_id)
}

fn persist_binding(
    conn: &mut rusqlite::Connection,
    binding_id: &str,
    instance_id: &str,
    address: &str,
    session_theme: &str,
) -> Result<(), Fail> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    if let Err(e) = opencrab_db::queries::create_gate_binding_in_tx(
        &tx,
        binding_id,
        instance_id,
        address,
        session_theme,
        opencrab_extgate::now_nanos(),
    ) {
        let _ = tx.rollback();
        return Err(match e {
            opencrab_db::queries::CreateGateBindingError::Conflict => {
                json_err(StatusCode::CONFLICT, "binding_conflict")
            }
            opencrab_db::queries::CreateGateBindingError::Store(err) => {
                json_err(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string())
            }
        });
    }
    if opencrab_db::queries::injected_commit_failure() {
        let _ = tx.rollback();
        return Err(json_err(StatusCode::INTERNAL_SERVER_ERROR, "commit failed"));
    }
    tx.commit()
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    Ok(())
}

pub async fn create_web_conversation(
    State(state): State<AppState>,
    Extension(extgate): Extension<Arc<opencrab_extgate::ExtgateState>>,
    Path(agent_id): Path<String>,
    body: Bytes,
) -> Response {
    let name = match parse_body(&body) {
        Ok(raw) => match raw {
            None => None,
            Some(raw) => match normalize_name(&raw) {
                Ok(n) => n,
                Err(r) => return r.into_response(),
            },
        },
        Err(r) => return r.into_response(),
    };

    let instance_id = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => {
                return json_err(StatusCode::INTERNAL_SERVER_ERROR, "database unavailable")
                    .into_response()
            }
        };
        match resolve_web_instance(&conn, &agent_id) {
            Ok(id) => id,
            Err(r) => return r.into_response(),
        }
    };

    let (conversation_id, session_id, binding_id) = conversation_ids(&agent_id);
    let theme = name.as_deref().unwrap_or(session_id.as_str());

    {
        let mut conn = match state.db.lock() {
            Ok(c) => c,
            Err(_) => {
                return json_err(StatusCode::INTERNAL_SERVER_ERROR, "database unavailable")
                    .into_response()
            }
        };
        if let Err(r) = persist_binding(&mut conn, &binding_id, &instance_id, &session_id, theme) {
            return r.into_response();
        }
    }
    opencrab_extgate::race::park("after_commit").await;

    tracing::info!(instance_id = %instance_id, binding_id = %binding_id, "resolved instance");
    let is_live = extgate
        .lock_registry()
        .map(|reg| reg.is_live(&instance_id))
        .unwrap_or(false);
    tracing::info!(instance_id = %instance_id, is_live, "is_live");
    let outcome =
        opencrab_extgate::enqueue_bind(&extgate, &instance_id, &binding_id, &session_id).await;
    tracing::info!(
        instance_id = %instance_id,
        binding_id = %binding_id,
        ?outcome,
        enqueued = outcome.started_wait(),
        "enqueue_bind"
    );
    tracing::info!(
        instance_id = %instance_id,
        binding_id = %binding_id,
        write_ok = matches!(outcome, opencrab_extgate::EnqueueBindOutcome::Written),
        "bind write"
    );
    if outcome.started_wait() {
        let acked =
            opencrab_extgate::wait_bind_ack(&extgate, &instance_id, &binding_id, BIND_WAIT).await;
        tracing::info!(
            instance_id = %instance_id,
            binding_id = %binding_id,
            acked,
            "wait_bind_ack"
        );
        if acked {
            opencrab_extgate::race::park("before_http_ready").await;
            return success(
                StatusCode::CREATED,
                &conversation_id,
                &session_id,
                &binding_id,
                &name,
                "ready",
            );
        }
    }
    success(
        StatusCode::ACCEPTED,
        &conversation_id,
        &session_id,
        &binding_id,
        &name,
        "provisioning",
    )
}

fn success(
    status: StatusCode,
    conversation_id: &str,
    session_id: &str,
    binding_id: &str,
    name: &Option<String>,
    state: &str,
) -> Response {
    (
        status,
        Json(json!({
            "conversation_id": conversation_id,
            "session_id": session_id,
            "binding_id": binding_id,
            "name": name,
            "state": state,
        })),
    )
        .into_response()
}
