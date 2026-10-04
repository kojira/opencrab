//! Operator issuance of additional sealed principals and subject grants while core runs
//! (Issue #1070). Authentication full-scans the principal table on every request, so a
//! principal sealed here is honored without restarting core. No HTTP route is added.

use std::collections::BTreeSet;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    credential_hash, load_principals, matching_ids, valid_principal_id, CredentialManifest,
    Operation, SecurityError,
};

/// Upper bound for a subject-association grant lifetime: one hour.
pub const MAX_SUBJECT_GRANT_TTL_NANOS: i64 = 3_600 * 1_000_000_000;
/// Upper bound for an operator-issued principal lifetime: 366 days.
const MAX_PRINCIPAL_TTL_NANOS: i64 = 366 * 86_400 * 1_000_000_000;

/// Target scope of a new principal. Neither variant means "all".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrincipalScope {
    Exact(BTreeSet<Uuid>),
    CreationNamespace(Uuid),
}

#[derive(Clone, Debug)]
pub struct PrincipalRequest {
    pub principal_id: String,
    pub operations: BTreeSet<Operation>,
    pub subject_ids: BTreeSet<i64>,
    pub scope: PrincipalScope,
    pub expires_at: i64,
}

/// The only plaintext copy of a freshly generated secret. It is zeroized on drop and never
/// printed by `Debug`.
pub struct IssuedCredential(Zeroizing<String>);

impl IssuedCredential {
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for IssuedCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IssuedCredential(redacted)")
    }
}

/// Creates, scopes, and seals one absent principal inside the caller's immediate
/// transaction. Shared by startup bootstrap and operator issuance so both enforce the same
/// unsealed-row, duplicate-ID, duplicate-bearer, and rotation checks.
pub(super) fn create_sealed_in_tx(
    tx: &Transaction<'_>,
    manifest: &CredentialManifest,
    now: i64,
) -> Result<usize, SecurityError> {
    let rows = load_principals(tx)?;
    if rows.iter().any(|row| row.sealed_at.is_none())
        || rows.iter().any(|row| row.id == manifest.principal_id)
    {
        return Err(SecurityError::Conflict);
    }
    if !matching_ids(&rows, &manifest.token[..]).is_empty() {
        return Err(SecurityError::Conflict);
    }
    if let Some(rotation) = &manifest.rotation {
        let predecessor = rows
            .iter()
            .find(|row| row.id == rotation.predecessor_principal_id)
            .ok_or(SecurityError::Conflict)?;
        if predecessor.revoked_at.is_some()
            || predecessor.expires_at <= now
            || rotation.overlap_deadline <= now
            || rotation.overlap_deadline > predecessor.expires_at
            || rotation.overlap_deadline > manifest.expires_at
        {
            return Err(SecurityError::Conflict);
        }
    }
    let mut salt = Zeroizing::new([0_u8; 32]);
    getrandom::fill(&mut *salt).map_err(|_| SecurityError::Store)?;
    let hash = credential_hash(&salt[..], &manifest.token[..]);
    let scope_mode = if manifest.creation_namespace.is_some() {
        "creation_namespace"
    } else {
        "exact"
    };
    let rotation = manifest.rotation.as_ref();
    tx.execute(
        "INSERT INTO gate_admin_principals
         (principal_id, credential_salt, credential_hash, scope_mode, created_at, expires_at,
          revoked_at, sealed_at, predecessor_principal_id, overlap_deadline)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7, ?8)",
        params![
            manifest.principal_id,
            &salt[..],
            &hash[..],
            scope_mode,
            now,
            manifest.expires_at,
            rotation.map(|value| &value.predecessor_principal_id),
            rotation.map(|value| value.overlap_deadline),
        ],
    )
    .map_err(|_| SecurityError::Conflict)?;
    let insert = |sql: &str, value: &dyn rusqlite::ToSql| {
        tx.execute(sql, params![manifest.principal_id, value])
            .map(|_| ())
            .map_err(|_| SecurityError::Store)
    };
    for operation in &manifest.operations {
        insert(
            "INSERT INTO gate_admin_principal_operations VALUES (?1, ?2)",
            &operation.as_str(),
        )?;
    }
    for subject_id in &manifest.subject_ids {
        insert(
            "INSERT INTO gate_admin_principal_subjects VALUES (?1, ?2)",
            subject_id,
        )?;
    }
    for instance_id in &manifest.instance_ids {
        insert(
            "INSERT INTO gate_admin_principal_instances VALUES (?1, ?2)",
            &instance_id.to_string(),
        )?;
    }
    if let Some(namespace) = manifest.creation_namespace {
        insert(
            "INSERT INTO gate_admin_principal_creation_namespaces VALUES (?1, ?2)",
            &namespace.to_string(),
        )?;
    }
    tx.execute(
        "UPDATE gate_admin_principals SET sealed_at=?2 WHERE principal_id=?1",
        params![manifest.principal_id, now],
    )
    .map_err(|_| SecurityError::Store)?;
    Ok(rows.len())
}

fn subject_is_live(tx: &Transaction<'_>, subject_id: i64) -> Result<bool, SecurityError> {
    tx.query_row(
        "SELECT 1 FROM agents AS agent WHERE agent.subject_id=?1
           AND NOT EXISTS(SELECT 1 FROM subject_tombstones AS t WHERE t.subject_id=agent.subject_id)",
        [subject_id],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(|_| SecurityError::Store)
}

fn validate_request(request: &PrincipalRequest, now: i64) -> Result<(), SecurityError> {
    let scope_ok = match &request.scope {
        PrincipalScope::Exact(instances) => !instances.is_empty(),
        PrincipalScope::CreationNamespace(namespace) => !namespace.is_nil(),
    };
    if !valid_principal_id(&request.principal_id)
        || request.operations.is_empty()
        || request.subject_ids.is_empty()
        || request.subject_ids.iter().any(|id| *id <= 0)
        || !scope_ok
        || request.expires_at <= now
        || request.expires_at - now > MAX_PRINCIPAL_TTL_NANOS
    {
        return Err(SecurityError::InvalidConfig);
    }
    Ok(())
}

/// Issues one new sealed principal with a fresh CSPRNG bearer. Every scoped subject must be
/// a live agent subject: pre-claiming a future or tombstoned ID would silently authorize a
/// different agent later. Existing principals are never rescoped or extended.
pub fn issue_principal(
    conn: &mut Connection,
    request: &PrincipalRequest,
    now: i64,
) -> Result<IssuedCredential, SecurityError> {
    validate_request(request, now)?;
    let mut token = Zeroizing::new([0_u8; 32]);
    getrandom::fill(&mut *token).map_err(|_| SecurityError::Store)?;
    let (instance_ids, creation_namespace) = match &request.scope {
        PrincipalScope::Exact(instances) => (instances.clone(), None),
        PrincipalScope::CreationNamespace(namespace) => (BTreeSet::new(), Some(*namespace)),
    };
    let manifest = CredentialManifest {
        principal_id: request.principal_id.clone(),
        token: token.clone(),
        operations: request.operations.clone(),
        subject_ids: request.subject_ids.clone(),
        instance_ids,
        creation_namespace,
        expires_at: request.expires_at,
        rotation: None,
    };
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| SecurityError::Store)?;
    for subject_id in &request.subject_ids {
        if !subject_is_live(&tx, *subject_id)? {
            return Err(SecurityError::Conflict);
        }
    }
    create_sealed_in_tx(&tx, &manifest, now)?;
    tx.commit().map_err(|_| SecurityError::Store)?;
    Ok(IssuedCredential(Zeroizing::new(
        URL_SAFE_NO_PAD.encode(&token[..]),
    )))
}

/// Issues one single-use association grant for a live `(agent_id, subject_id)` pair with a
/// bounded lifetime. Consumption stays in the existing audited `PUT instance` transaction.
pub fn issue_subject_grant(
    conn: &mut Connection,
    agent_id: &str,
    subject_id: i64,
    ttl_nanos: i64,
    now: i64,
) -> Result<IssuedCredential, SecurityError> {
    if agent_id.is_empty()
        || subject_id <= 0
        || ttl_nanos <= 0
        || ttl_nanos > MAX_SUBJECT_GRANT_TTL_NANOS
    {
        return Err(SecurityError::InvalidConfig);
    }
    let expires_at = now.checked_add(ttl_nanos).ok_or(SecurityError::InvalidConfig)?;
    opencrab_db::queries::issue_subject_association_grant(
        conn, agent_id, subject_id, expires_at, now,
    )
    .map(|grant| IssuedCredential(Zeroizing::new(grant)))
    .map_err(|error| match error {
        opencrab_db::queries::SubjectGrantError::Store(_) => SecurityError::Store,
        _ => SecurityError::Conflict,
    })
}
