//! D-1097: `POST /api/gate-subject-grants` issues the single-use subject grant that a new
//! `PUT /api/gate-instances/{id}` association must present.
//!
//! The grant only lets its holder create an instance, so the route is gated on `instance.put`
//! and on the principal's subject scope. The instance-scope check still applies when the grant
//! is consumed. Lifetime and the live-pair check come from the same issuer as the operator CLI.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde_json::json;
use std::sync::Arc;

use super::audit::{begin_request, early};
use super::{json_ok, require_object, require_only_keys, require_string};
use crate::error::{ErrorCode, GateError};
use crate::gate_admin_security::{Operation, MAX_SUBJECT_GRANT_TTL_NANOS};
use crate::ids::now_nanos;
use crate::registry::ExtgateState;

pub(super) async fn post_subject_grant(
    State(state): State<Arc<ExtgateState>>,
    req: Request,
) -> Response {
    match post_subject_grant_inner(&state, req).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn post_subject_grant_inner(
    state: &ExtgateState,
    req: Request,
) -> Result<Response, GateError> {
    let operation = Operation::InstancePut;
    let (authenticated, body) = begin_request(state, operation, req).await?;
    let obj = early(state, operation, &authenticated, require_object(&body))?;
    early(
        state,
        operation,
        &authenticated,
        require_only_keys(&obj, &["agent_id"]),
    )?;
    let agent_id = early(
        state,
        operation,
        &authenticated,
        require_string(&obj, "agent_id"),
    )?;
    let subject_id = early(
        state,
        operation,
        &authenticated,
        subject_for_agent(state, &agent_id),
    )?;
    let authorized = state.authorize_admin_subject(operation, &authenticated, subject_id)?;

    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let now = now_nanos();
    let issued = crate::gate_admin_security::issue_subject_grant_in_tx(
        &tx,
        &agent_id,
        subject_id,
        MAX_SUBJECT_GRANT_TTL_NANOS,
        now,
    );
    let result_class = match &issued {
        Ok(_) => "succeeded",
        Err(crate::gate_admin_security::SecurityError::Store) => "store_error",
        Err(_) => "conflict",
    };
    // A grant row is told apart from an instance PUT by its target: the authorized subject with
    // no instance.
    crate::gate_admin_security::append_subject_audit(
        &tx,
        uuid::Uuid::new_v4(),
        now,
        operation,
        &authorized.principal_id,
        subject_id,
        result_class,
    )
    .map_err(|_| GateError::store())?;
    match issued {
        Ok(grant) => {
            tx.commit().map_err(|_| GateError::store())?;
            Ok(json_ok(
                StatusCode::CREATED,
                json!({"subject_id": subject_id, "grant": grant.expose_secret()}),
            ))
        }
        Err(crate::gate_admin_security::SecurityError::Store) => Err(GateError::store()),
        Err(_) => {
            tx.commit().map_err(|_| GateError::store())?;
            Err(GateError::new(ErrorCode::SubjectUnknown))
        }
    }
}

fn subject_for_agent(state: &ExtgateState, agent_id: &str) -> Result<i64, GateError> {
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    conn.query_row(
        "SELECT subject_id FROM agents WHERE agent_id=?1 AND subject_id IS NOT NULL",
        [agent_id],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|_| GateError::store())?
    .ok_or_else(|| GateError::new(ErrorCode::SubjectUnknown))
}
