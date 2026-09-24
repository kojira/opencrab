use axum::body::to_bytes;
use axum::extract::Request;
use rusqlite::{Transaction, TransactionBehavior};

use crate::error::{ErrorCode, GateError};
use crate::gate_admin_security::{Authenticated, Authorized, Operation};
use crate::ids::now_nanos;
use crate::registry::ExtgateState;

pub(super) async fn begin_request(
    state: &ExtgateState,
    operation: Operation,
    req: Request,
) -> Result<(Authenticated, Vec<u8>), GateError> {
    let (parts, body) = req.into_parts();
    let authenticated = state.authenticate_admin(&parts.headers, operation)?;
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes.to_vec(),
        Err(_) => {
            audit_early_error(state, operation, &authenticated, ErrorCode::BadRequest)?;
            return Err(GateError::new(ErrorCode::BadRequest));
        }
    };
    Ok((authenticated, bytes))
}

fn result_class(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::BadRequest => "bad_request",
        ErrorCode::SubjectUnknown | ErrorCode::InstanceUnknown | ErrorCode::BindingUnknown => {
            "not_found"
        }
        ErrorCode::StoreError => "store_error",
        ErrorCode::Unauthorized => "unauthorized",
        _ => "conflict",
    }
}

fn audit_early_error(
    state: &ExtgateState,
    operation: Operation,
    authenticated: &Authenticated,
    code: ErrorCode,
) -> Result<(), GateError> {
    if state.uses_legacy_admin() {
        return Ok(());
    }
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    crate::gate_admin_security::append_audit_for_attempt(
        &conn,
        uuid::Uuid::new_v4(),
        now_nanos(),
        operation,
        Some(&authenticated.principal_id),
        None,
        result_class(code),
    )
    .map_err(|_| GateError::store())
}

pub(super) fn early<T>(
    state: &ExtgateState,
    operation: Operation,
    authenticated: &Authenticated,
    result: Result<T, GateError>,
) -> Result<T, GateError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            audit_early_error(state, operation, authenticated, error.code)?;
            Err(error)
        }
    }
}

pub(super) fn audited_operation<T, F>(
    state: &ExtgateState,
    operation: Operation,
    authorized: &Authorized,
    action: F,
) -> Result<T, GateError>
where
    F: FnOnce(&Transaction<'_>) -> Result<T, GateError>,
{
    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    tx.execute_batch("SAVEPOINT gate_admin_handler")
        .map_err(|_| GateError::store())?;
    match action(&tx) {
        Ok(value) => {
            if !state.uses_legacy_admin() {
                crate::gate_admin_security::append_audit(
                    &tx,
                    uuid::Uuid::new_v4(),
                    now_nanos(),
                    operation,
                    Some(authorized),
                    "succeeded",
                )
                .map_err(|_| GateError::store())?;
            }
            tx.execute_batch("RELEASE gate_admin_handler")
                .map_err(|_| GateError::store())?;
            tx.commit().map_err(|_| GateError::store())?;
            Ok(value)
        }
        Err(error) => {
            tx.execute_batch("ROLLBACK TO gate_admin_handler; RELEASE gate_admin_handler")
                .map_err(|_| GateError::store())?;
            if !state.uses_legacy_admin() {
                crate::gate_admin_security::append_audit(
                    &tx,
                    uuid::Uuid::new_v4(),
                    now_nanos(),
                    operation,
                    Some(authorized),
                    result_class(error.code),
                )
                .map_err(|_| GateError::store())?;
            }
            tx.commit().map_err(|_| GateError::store())?;
            Err(error)
        }
    }
}
