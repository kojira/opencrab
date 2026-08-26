//! 6 operation。V3 §5。

use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};

use crate::error::{ErrorCode, GateError, JSON_CONTENT_TYPE};
use crate::ids::{
    config_digest, decode_config_b64, now_nanos, parse_uuid, session_id_for_binding,
};
use crate::json::parse_object_no_dup;
use crate::listen::enqueue_bind;
use crate::registry::ExtgateState;

pub fn admin_router(state: Arc<ExtgateState>) -> Router {
    Router::new()
        .route(
            "/api/gate-instances/{instance_id}",
            get(get_instance).put(put_instance).delete(delete_instance),
        )
        .route(
            "/api/gate-instances/{instance_id}/revisions",
            post(post_revision),
        )
        .route(
            "/api/gate-bindings/{binding_id}",
            put(put_binding).delete(delete_binding),
        )
        .with_state(state)
}

async fn read_body(req: Request) -> Result<(axum::http::HeaderMap, Vec<u8>), GateError> {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX)
        .await
        .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    Ok((parts.headers, bytes.to_vec()))
}

fn json_ok(status: StatusCode, value: Value) -> Response {
    let mut res = Response::new(axum::body::Body::from(value.to_string()));
    *res.status_mut() = status;
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(JSON_CONTENT_TYPE),
    );
    res
}

fn require_empty_body(body: &[u8]) -> Result<(), GateError> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(GateError::new(ErrorCode::BadRequest))
    }
}

fn require_object(body: &[u8]) -> Result<Value, GateError> {
    parse_object_no_dup(body)
}

fn require_string(obj: &Value, key: &str) -> Result<String, GateError> {
    match obj.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn require_string_any(obj: &Value, key: &str) -> Result<String, GateError> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn require_bool(obj: &Value, key: &str) -> Result<bool, GateError> {
    match obj.get(key) {
        Some(Value::Bool(b)) => Ok(*b),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn require_positive_i64(obj: &Value, key: &str) -> Result<i64, GateError> {
    match obj.get(key) {
        Some(Value::Number(n)) => {
            let v = n
                .as_i64()
                .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
            if v <= 0 {
                return Err(GateError::new(ErrorCode::BadRequest));
            }
            Ok(v)
        }
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn require_positive_u64(obj: &Value, key: &str) -> Result<u64, GateError> {
    match obj.get(key) {
        Some(Value::Number(n)) => {
            let v = n
                .as_u64()
                .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
            if v == 0 {
                return Err(GateError::new(ErrorCode::BadRequest));
            }
            Ok(v)
        }
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn agent_for_subject(conn: &rusqlite::Connection, subject_id: i64) -> Result<String, GateError> {
    let mut stmt = conn
        .prepare("SELECT agent_id FROM agents WHERE subject_id = ?1")
        .map_err(|_| GateError::store())?;
    let mut rows = stmt
        .query_map(params![subject_id], |r| r.get::<_, String>(0))
        .map_err(|_| GateError::store())?;
    let first = rows.next();
    let second = rows.next();
    match (first, second) {
        (None, _) => Err(GateError::new(ErrorCode::SubjectUnknown)),
        (Some(Ok(_)), Some(Ok(_))) => Err(GateError::store()),
        (Some(Ok(id)), _) => Ok(id),
        (Some(Err(_)), _) => Err(GateError::store()),
    }
}

fn instance_json(conn: &rusqlite::Connection, instance_id: &str) -> Result<Value, GateError> {
    conn.query_row(
        "SELECT instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest,
                created_at, updated_at, deleted_at
         FROM gate_instances WHERE instance_id = ?1",
        params![instance_id],
        |r| {
            Ok(json!({
                "instance_id": r.get::<_, String>(0)?,
                "kind_id": r.get::<_, String>(1)?,
                "subject_id": r.get::<_, i64>(2)?,
                "revision": r.get::<_, i64>(3)?,
                "enabled": r.get::<_, i64>(4)? == 1,
                "config_b64": r.get::<_, String>(5)?,
                "config_digest": r.get::<_, String>(6)?,
                "created_at": r.get::<_, i64>(7)?,
                "updated_at": r.get::<_, i64>(8)?,
                "deleted_at": r.get::<_, Option<i64>>(9)?,
            }))
        },
    )
    .map_err(|_| GateError::new(ErrorCode::InstanceUnknown))
}

fn binding_json(conn: &rusqlite::Connection, binding_id: &str) -> Result<Value, GateError> {
    conn.query_row(
        "SELECT binding_id, instance_id, address, created_at, closed_at
         FROM gate_bindings WHERE binding_id = ?1",
        params![binding_id],
        |r| {
            Ok(json!({
                "binding_id": r.get::<_, String>(0)?,
                "instance_id": r.get::<_, String>(1)?,
                "address": r.get::<_, String>(2)?,
                "created_at": r.get::<_, i64>(3)?,
                "closed_at": r.get::<_, Option<i64>>(4)?,
            }))
        },
    )
    .map_err(|_| GateError::new(ErrorCode::BindingUnknown))
}

async fn get_instance(
    State(state): State<Arc<ExtgateState>>,
    Path(instance_id): Path<String>,
    req: Request,
) -> Response {
    match get_instance_inner(&state, &instance_id, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn get_instance_inner(
    state: &ExtgateState,
    instance_id: &str,
    req: Request,
) -> Result<Response, GateError> {
    let (headers, body) = read_body(req).await?;
    state.token.authorize(&headers)?;
    require_empty_body(&body)?;
    let instance_id = parse_uuid(instance_id)?;
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    let deleted: Option<Option<i64>> = conn
        .query_row(
            "SELECT deleted_at FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| GateError::store())?;
    match deleted {
        None | Some(Some(_)) => Err(GateError::new(ErrorCode::InstanceUnknown)),
        Some(None) => Ok(json_ok(StatusCode::OK, instance_json(&conn, &instance_id)?)),
    }
}

async fn put_instance(
    State(state): State<Arc<ExtgateState>>,
    Path(instance_id): Path<String>,
    req: Request,
) -> Response {
    match put_instance_inner(&state, &instance_id, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn put_instance_inner(
    state: &ExtgateState,
    instance_id: &str,
    req: Request,
) -> Result<Response, GateError> {
    let (headers, body) = read_body(req).await?;
    state.token.authorize(&headers)?;
    let obj = require_object(&body)?;
    let instance_id = parse_uuid(instance_id)?;
    let kind_id = require_string(&obj, "kind_id")?;
    let subject_id = require_positive_i64(&obj, "subject_id")?;
    let enabled = require_bool(&obj, "enabled")?;
    let config_b64 = require_string_any(&obj, "config_b64")?;
    let config_bytes = decode_config_b64(&config_b64)?;
    let digest = config_digest(&config_bytes);
    let now = now_nanos();

    let conn = state.db.lock().map_err(|_| GateError::store())?;
    let _agent = agent_for_subject(&conn, subject_id)?;
    let existing = conn
        .query_row(
            "SELECT kind_id, subject_id, enabled, config_b64, deleted_at
             FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| GateError::store())?;
    match existing {
        Some((_, _, _, _, Some(_))) => Err(GateError::new(ErrorCode::InstanceConflict)),
        Some((k, s, e, cfg, None)) => {
            let same_bytes = decode_config_b64(&cfg)? == config_bytes;
            if k == kind_id && s == subject_id && (e == 1) == enabled && same_bytes {
                Ok(json_ok(StatusCode::OK, instance_json(&conn, &instance_id)?))
            } else {
                Err(GateError::new(ErrorCode::InstanceConflict))
            }
        }
        None => {
            conn.execute(
                "INSERT INTO gate_instances (
                    instance_id, kind_id, subject_id, revision, enabled,
                    config_b64, config_digest, created_at, updated_at, deleted_at
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?7, NULL)",
                params![
                    instance_id,
                    kind_id,
                    subject_id,
                    i64::from(enabled),
                    config_b64,
                    digest,
                    now
                ],
            )
            .map_err(|_| GateError::store())?;
            Ok(json_ok(
                StatusCode::CREATED,
                instance_json(&conn, &instance_id)?,
            ))
        }
    }
}

async fn delete_instance(
    State(state): State<Arc<ExtgateState>>,
    Path(instance_id): Path<String>,
    req: Request,
) -> Response {
    match delete_instance_inner(&state, &instance_id, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn delete_instance_inner(
    state: &ExtgateState,
    instance_id: &str,
    req: Request,
) -> Result<Response, GateError> {
    let (headers, body) = read_body(req).await?;
    state.token.authorize(&headers)?;
    require_empty_body(&body)?;
    let instance_id = parse_uuid(instance_id)?;
    let now = now_nanos();
    let reg = state.lock_registry()?;
    if reg.is_live(&instance_id) {
        return Err(GateError::new(ErrorCode::InstanceActive));
    }
    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let deleted: Option<Option<i64>> = tx
        .query_row(
            "SELECT deleted_at FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| GateError::store())?;
    match deleted {
        None | Some(Some(_)) => {
            let _ = tx.rollback();
            Err(GateError::new(ErrorCode::InstanceUnknown))
        }
        Some(None) => {
            tx.execute(
                "UPDATE gate_instances SET deleted_at = ?2, updated_at = ?2 WHERE instance_id = ?1",
                params![instance_id, now],
            )
            .map_err(|_| GateError::store())?;
            tx.execute(
                "UPDATE gate_bindings SET closed_at = ?2
                 WHERE instance_id = ?1 AND closed_at IS NULL",
                params![instance_id, now],
            )
            .map_err(|_| GateError::store())?;
            tx.commit().map_err(|_| GateError::store())?;
            drop(reg);
            Ok(json_ok(
                StatusCode::OK,
                json!({"instance_id": instance_id, "deleted": true}),
            ))
        }
    }
}

async fn post_revision(
    State(state): State<Arc<ExtgateState>>,
    Path(instance_id): Path<String>,
    req: Request,
) -> Response {
    match post_revision_inner(&state, &instance_id, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn post_revision_inner(
    state: &ExtgateState,
    instance_id: &str,
    req: Request,
) -> Result<Response, GateError> {
    let (headers, body) = read_body(req).await?;
    state.token.authorize(&headers)?;
    let obj = require_object(&body)?;
    let instance_id = parse_uuid(instance_id)?;
    let expected = require_positive_u64(&obj, "expected_revision")?;
    let enabled = require_bool(&obj, "enabled")?;
    let config_b64 = require_string_any(&obj, "config_b64")?;
    let config_bytes = decode_config_b64(&config_b64)?;
    let digest = config_digest(&config_bytes);
    let now = now_nanos();

    let reg = state.lock_registry()?;
    if reg.is_live(&instance_id) {
        return Err(GateError::new(ErrorCode::InstanceActive));
    }
    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let row = tx
        .query_row(
            "SELECT revision, deleted_at FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()
        .map_err(|_| GateError::store())?;
    match row {
        None | Some((_, Some(_))) => {
            let _ = tx.rollback();
            Err(GateError::new(ErrorCode::InstanceUnknown))
        }
        Some((rev, None)) if u64::try_from(rev).ok() != Some(expected) => {
            let _ = tx.rollback();
            Err(GateError::new(ErrorCode::RevisionConflict))
        }
        Some((rev, None)) => {
            let new_rev = rev + 1;
            tx.execute(
                "UPDATE gate_instances
                 SET revision = ?2, enabled = ?3, config_b64 = ?4, config_digest = ?5, updated_at = ?6
                 WHERE instance_id = ?1",
                params![
                    instance_id,
                    new_rev,
                    i64::from(enabled),
                    config_b64,
                    digest,
                    now
                ],
            )
            .map_err(|_| GateError::store())?;
            tx.commit().map_err(|_| GateError::store())?;
            drop(reg);
            Ok(json_ok(
                StatusCode::CREATED,
                json!({
                    "instance_id": instance_id,
                    "revision": new_rev,
                    "enabled": enabled,
                    "config_digest": digest,
                }),
            ))
        }
    }
}

async fn put_binding(
    State(state): State<Arc<ExtgateState>>,
    Path(binding_id): Path<String>,
    req: Request,
) -> Response {
    match put_binding_inner(&state, &binding_id, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn put_binding_inner(
    state: &Arc<ExtgateState>,
    binding_id: &str,
    req: Request,
) -> Result<Response, GateError> {
    let (headers, body) = read_body(req).await?;
    state.token.authorize(&headers)?;
    let obj = require_object(&body)?;
    let binding_id = parse_uuid(binding_id)?;
    let instance_id = parse_uuid(&require_string(&obj, "instance_id")?)?;
    let address = require_string(&obj, "address")?;
    let now = now_nanos();
    let now_rfc = chrono::Utc::now().to_rfc3339();

    let body = {
    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let inst = conn
        .query_row(
            "SELECT enabled, deleted_at, subject_id FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| GateError::store())?;
    let (enabled, subject_id) = match inst {
        None | Some((_, Some(_), _)) => return Err(GateError::new(ErrorCode::InstanceUnknown)),
        Some((e, None, s)) => (e == 1, s),
    };
    if !enabled {
        return Err(GateError::new(ErrorCode::InstanceDisabled));
    }
    let agent_id = agent_for_subject(&conn, subject_id)?;

    let existing = conn
        .query_row(
            "SELECT instance_id, address, closed_at FROM gate_bindings WHERE binding_id = ?1",
            params![binding_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| GateError::store())?;
    match existing {
        Some((_, _, Some(_))) => return Err(GateError::new(ErrorCode::BindingClosed)),
        Some((inst, addr, None)) => {
            if inst == instance_id && addr == address {
                return Ok(json_ok(StatusCode::OK, binding_json(&conn, &binding_id)?));
            }
            return Err(GateError::new(ErrorCode::BindingConflict));
        }
        None => {}
    }

    let taken: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM gate_bindings
             WHERE instance_id = ?1 AND address = ?2 AND closed_at IS NULL",
            params![instance_id, address],
            |r| r.get(0),
        )
        .map_err(|_| GateError::store())?;
    if taken > 0 {
        return Err(GateError::new(ErrorCode::AddressInUse));
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let session_id = session_id_for_binding(&binding_id);
    opencrab_db::queries::insert_session_in_tx(&tx, &session_id, &address, &now_rfc)
        .map_err(|_| GateError::store())?;
    opencrab_db::queries::insert_agent_session_in_tx(&tx, &agent_id, &session_id)
        .map_err(|_| GateError::store())?;
    insert_binding_row(&tx, &binding_id, &instance_id, &address, now)?;
    tx.commit().map_err(|_| GateError::store())?;
    binding_json(&conn, &binding_id)?
    };

    enqueue_bind(state, &instance_id, &binding_id, &address).await;
    Ok(json_ok(StatusCode::CREATED, body))
}

fn insert_binding_row(
    tx: &Transaction<'_>,
    binding_id: &str,
    instance_id: &str,
    address: &str,
    now: i64,
) -> Result<(), GateError> {
    tx.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at, closed_at)
         VALUES (?1, ?2, ?3, ?4, NULL)",
        params![binding_id, instance_id, address, now],
    )
    .map_err(|_| GateError::store())?;
    Ok(())
}

async fn delete_binding(
    State(state): State<Arc<ExtgateState>>,
    Path(binding_id): Path<String>,
    req: Request,
) -> Response {
    match delete_binding_inner(&state, &binding_id, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn delete_binding_inner(
    state: &ExtgateState,
    binding_id: &str,
    req: Request,
) -> Result<Response, GateError> {
    let (headers, body) = read_body(req).await?;
    state.token.authorize(&headers)?;
    require_empty_body(&body)?;
    let binding_id = parse_uuid(binding_id)?;
    let now = now_nanos();
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    let row = conn
        .query_row(
            "SELECT instance_id, closed_at FROM gate_bindings WHERE binding_id = ?1",
            params![binding_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()
        .map_err(|_| GateError::store())?;
    let (instance_id, already) = match row {
        None => return Err(GateError::new(ErrorCode::BindingUnknown)),
        Some((inst, closed)) => (inst, closed.is_some()),
    };
    if !already {
        conn.execute(
            "UPDATE gate_bindings SET closed_at = ?2 WHERE binding_id = ?1",
            params![binding_id, now],
        )
        .map_err(|_| GateError::store())?;
    }
    drop(conn);
    if let Ok(mut reg) = state.lock_registry() {
        if let Some(live) = reg.get_mut(&instance_id) {
            live.acknowledged.remove(&binding_id);
            live.pending
                .retain(|_, p| p.binding_id() != Some(binding_id.as_str()));
        }
    }
    Ok(json_ok(
        StatusCode::OK,
        json!({"binding_id": binding_id, "closed": true}),
    ))
}
