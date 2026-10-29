use rusqlite::{params, Connection, TransactionBehavior};
use uuid::Uuid;

use super::{Authorized, Operation, SecurityError};

/// Executes an authorized mutation and audit as one outer transaction. A rejected
/// mutation rolls back its savepoint and commits only the sanitized failure audit.
pub fn audited_mutation<F>(
    conn: &mut Connection,
    request_id: Uuid,
    attempted_at: i64,
    operation: Operation,
    authorized: &Authorized,
    mutation: F,
) -> Result<(), SecurityError>
where
    F: FnOnce(&Connection) -> Result<(), SecurityError>,
{
    let mut tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| SecurityError::Store)?;
    let mut savepoint = tx.savepoint().map_err(|_| SecurityError::Store)?;
    match mutation(&savepoint) {
        Ok(()) => {
            append_audit(
                &savepoint,
                request_id,
                attempted_at,
                operation,
                Some(authorized),
                "succeeded",
            )?;
            savepoint.commit().map_err(|_| SecurityError::Store)?;
            tx.commit().map_err(|_| SecurityError::Store)
        }
        Err(_) => {
            savepoint.rollback().map_err(|_| SecurityError::Store)?;
            drop(savepoint);
            append_audit(
                &tx,
                request_id,
                attempted_at,
                operation,
                Some(authorized),
                "conflict",
            )?;
            tx.commit().map_err(|_| SecurityError::Store)?;
            Err(SecurityError::Conflict)
        }
    }
}

/// Audits a request authorized for a subject with no instance target (a subject grant).
pub fn append_subject_audit(
    conn: &Connection,
    request_id: Uuid,
    attempted_at: i64,
    operation: Operation,
    principal_id: &str,
    subject_id: i64,
    result_class: &str,
) -> Result<(), SecurityError> {
    insert_audit(
        conn,
        request_id,
        attempted_at,
        operation,
        Some(principal_id),
        Some(subject_id),
        None,
        result_class,
    )
}

pub fn append_audit_for_attempt(
    conn: &Connection,
    request_id: Uuid,
    attempted_at: i64,
    operation: Operation,
    principal_id: Option<&str>,
    authorized_target: Option<(i64, Uuid)>,
    result_class: &str,
) -> Result<(), SecurityError> {
    insert_audit(
        conn,
        request_id,
        attempted_at,
        operation,
        principal_id,
        authorized_target.map(|value| value.0),
        authorized_target.map(|value| value.1),
        result_class,
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_audit(
    conn: &Connection,
    request_id: Uuid,
    attempted_at: i64,
    operation: Operation,
    principal_id: Option<&str>,
    subject_id: Option<i64>,
    instance_id: Option<Uuid>,
    result_class: &str,
) -> Result<(), SecurityError> {
    conn.execute(
        "INSERT INTO gate_admin_request_audit
         (audit_id, request_id, attempted_at, principal_id, operation,
          authorized_subject_id, authorized_instance_id, result_class)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            Uuid::new_v4().to_string(),
            request_id.to_string(),
            attempted_at,
            principal_id,
            operation.as_str(),
            subject_id,
            instance_id.map(|value| value.to_string()),
            result_class,
        ],
    )
    .map_err(|_| SecurityError::Store)?;
    Ok(())
}

pub fn append_audit(
    conn: &Connection,
    request_id: Uuid,
    attempted_at: i64,
    operation: Operation,
    authorized: Option<&Authorized>,
    result_class: &str,
) -> Result<(), SecurityError> {
    append_audit_for_attempt(
        conn,
        request_id,
        attempted_at,
        operation,
        authorized.map(|value| value.principal_id.as_str()),
        authorized.map(|value| (value.subject_id, value.instance_id)),
        result_class,
    )
}
