//! Platform-neutral gate-admin credential, scope, and audit authority (Issue #1006 S1).

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, SecondsFormat};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const HASH_DOMAIN: &[u8] = b"opencrab/gate-admin/bearer/v1\0";

#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("invalid gate-admin configuration")]
    InvalidConfig,
    #[error("invalid gate-admin credential manifest")]
    InvalidManifest,
    #[error("gate-admin credential conflict")]
    Conflict,
    #[error("unauthorized")]
    Unauthorized,
    #[error("gate-admin store error")]
    Store,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Operation {
    InstanceRead,
    InstancePut,
    InstanceDelete,
    InstanceRevise,
    BindingPut,
    BindingDelete,
}

impl Operation {
    pub const ALL: [Self; 6] = [
        Self::InstanceRead,
        Self::InstancePut,
        Self::InstanceDelete,
        Self::InstanceRevise,
        Self::BindingPut,
        Self::BindingDelete,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InstanceRead => "instance.read",
            Self::InstancePut => "instance.put",
            Self::InstanceDelete => "instance.delete",
            Self::InstanceRevise => "instance.revise",
            Self::BindingPut => "binding.put",
            Self::BindingDelete => "binding.delete",
        }
    }

    fn parse(value: &str) -> Result<Self, SecurityError> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.as_str() == value)
            .ok_or(SecurityError::InvalidManifest)
    }
}

pub struct CredentialManifest {
    pub principal_id: String,
    token: Zeroizing<[u8; 32]>,
    pub operations: BTreeSet<Operation>,
    pub subject_ids: BTreeSet<i64>,
    pub instance_ids: BTreeSet<Uuid>,
    pub creation_namespace: Option<Uuid>,
    pub expires_at: i64,
    pub rotation: Option<Rotation>,
}

impl std::fmt::Debug for CredentialManifest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialManifest")
            .field("principal_id", &self.principal_id)
            .field("bearer_token", &"redacted")
            .field("operations", &self.operations)
            .field("subject_ids", &self.subject_ids)
            .field("instance_ids", &self.instance_ids)
            .field("creation_namespace", &self.creation_namespace)
            .field("expires_at", &self.expires_at)
            .field("rotation", &self.rotation)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rotation {
    pub predecessor_principal_id: String,
    pub overlap_deadline: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: u64,
    principal_id: String,
    bearer_token: String,
    operations: Vec<String>,
    scope: RawScope,
    expires_at: String,
    rotation: Option<RawRotation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScope {
    subject_ids: Vec<i64>,
    instance_ids: Vec<String>,
    creation_namespace: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRotation {
    predecessor_principal_id: String,
    overlap_deadline: String,
}

fn valid_principal_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn canonical_timestamp(value: &str) -> Result<i64, SecurityError> {
    let parsed = DateTime::parse_from_rfc3339(value).map_err(|_| SecurityError::InvalidManifest)?;
    if parsed.offset().local_minus_utc() != 0
        || parsed.to_rfc3339_opts(SecondsFormat::AutoSi, true) != value
    {
        return Err(SecurityError::InvalidManifest);
    }
    parsed
        .timestamp_nanos_opt()
        .ok_or(SecurityError::InvalidManifest)
}

fn canonical_uuid(value: &str) -> Result<Uuid, SecurityError> {
    let uuid = Uuid::parse_str(value).map_err(|_| SecurityError::InvalidManifest)?;
    if uuid.hyphenated().to_string() != value {
        return Err(SecurityError::InvalidManifest);
    }
    Ok(uuid)
}

fn collect_unique<T: Ord>(
    values: impl IntoIterator<Item = T>,
) -> Result<BTreeSet<T>, SecurityError> {
    let mut result = BTreeSet::new();
    for value in values {
        if !result.insert(value) {
            return Err(SecurityError::InvalidManifest);
        }
    }
    Ok(result)
}

fn parse_manifest(bytes: &[u8]) -> Result<CredentialManifest, SecurityError> {
    let mut raw: RawManifest =
        serde_json::from_slice(bytes).map_err(|_| SecurityError::InvalidManifest)?;
    if raw.version != 1 || !valid_principal_id(&raw.principal_id) {
        return Err(SecurityError::InvalidManifest);
    }
    let decoded_result = URL_SAFE_NO_PAD.decode(raw.bearer_token.as_bytes());
    let canonical_token = Zeroizing::new(raw.bearer_token.clone());
    raw.bearer_token.zeroize();
    let decoded = Zeroizing::new(decoded_result.map_err(|_| SecurityError::InvalidManifest)?);
    if decoded.len() != 32 || URL_SAFE_NO_PAD.encode(&*decoded) != canonical_token.as_str() {
        return Err(SecurityError::InvalidManifest);
    }
    let mut token = Zeroizing::new([0_u8; 32]);
    token.copy_from_slice(&decoded);
    let operations = collect_unique(
        raw.operations
            .iter()
            .map(|value| Operation::parse(value))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    let subject_ids = collect_unique(raw.scope.subject_ids)?;
    if operations.is_empty() || subject_ids.is_empty() || subject_ids.iter().any(|id| *id <= 0) {
        return Err(SecurityError::InvalidManifest);
    }
    let instance_ids = collect_unique(
        raw.scope
            .instance_ids
            .iter()
            .map(|value| canonical_uuid(value))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    let creation_namespace = raw
        .scope
        .creation_namespace
        .as_deref()
        .map(canonical_uuid)
        .transpose()?;
    if (instance_ids.is_empty()) == creation_namespace.is_none() {
        return Err(SecurityError::InvalidManifest);
    }
    let rotation = raw
        .rotation
        .map(|rotation| {
            if !valid_principal_id(&rotation.predecessor_principal_id) {
                return Err(SecurityError::InvalidManifest);
            }
            Ok(Rotation {
                predecessor_principal_id: rotation.predecessor_principal_id,
                overlap_deadline: canonical_timestamp(&rotation.overlap_deadline)?,
            })
        })
        .transpose()?;
    Ok(CredentialManifest {
        principal_id: raw.principal_id,
        token,
        operations,
        subject_ids,
        instance_ids,
        creation_namespace,
        expires_at: canonical_timestamp(&raw.expires_at)?,
        rotation,
    })
}

/// Opens an absolute manifest path once, rejecting every symlink component and insecure inode.
pub fn read_manifest(path: &Path, service_euid: u32) -> Result<CredentialManifest, SecurityError> {
    if !path.is_absolute() {
        return Err(SecurityError::InvalidConfig);
    }
    let mut current = std::path::PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(part) => current.push(part),
            _ => return Err(SecurityError::InvalidConfig),
        }
        let metadata =
            std::fs::symlink_metadata(&current).map_err(|_| SecurityError::InvalidManifest)?;
        if metadata.file_type().is_symlink() {
            return Err(SecurityError::InvalidManifest);
        }
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| SecurityError::InvalidManifest)?;
    validate_manifest_metadata(&file, service_euid)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.read_to_end(&mut bytes)
        .map_err(|_| SecurityError::InvalidManifest)?;
    parse_manifest(&bytes)
}

fn validate_manifest_metadata(file: &File, service_euid: u32) -> Result<(), SecurityError> {
    let metadata = file
        .metadata()
        .map_err(|_| SecurityError::InvalidManifest)?;
    if !metadata.file_type().is_file()
        || metadata.uid() != service_euid
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(SecurityError::InvalidManifest);
    }
    Ok(())
}

fn credential_hash(salt: &[u8], token: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut digest = Sha256::new();
    digest.update(HASH_DOMAIN);
    digest.update(salt);
    digest.update(token);
    let mut output = Zeroizing::new([0_u8; 32]);
    output.copy_from_slice(&digest.finalize());
    output
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    let max = left.len().max(right.len());
    let mut difference = u8::from(left.len() != right.len());
    for index in 0..max {
        difference |=
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0);
    }
    difference == 0
}

#[derive(Debug, Eq, PartialEq)]
pub struct BootstrapOutcome {
    pub created: bool,
    pub scanned_principals: usize,
}

struct PrincipalRow {
    id: String,
    salt: Vec<u8>,
    hash: Vec<u8>,
    scope_mode: String,
    expires_at: i64,
    revoked_at: Option<i64>,
    sealed_at: Option<i64>,
    predecessor: Option<String>,
    overlap_deadline: Option<i64>,
}

fn load_principals(tx: &Transaction<'_>) -> Result<Vec<PrincipalRow>, SecurityError> {
    let mut statement = tx
        .prepare(
            "SELECT principal_id, credential_salt, credential_hash, scope_mode, expires_at,
                    revoked_at, sealed_at, predecessor_principal_id, overlap_deadline
             FROM gate_admin_principals ORDER BY principal_id",
        )
        .map_err(|_| SecurityError::Store)?;
    let rows = statement
        .query_map([], |row| {
            Ok(PrincipalRow {
                id: row.get(0)?,
                salt: row.get(1)?,
                hash: row.get(2)?,
                scope_mode: row.get(3)?,
                expires_at: row.get(4)?,
                revoked_at: row.get(5)?,
                sealed_at: row.get(6)?,
                predecessor: row.get(7)?,
                overlap_deadline: row.get(8)?,
            })
        })
        .map_err(|_| SecurityError::Store)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| SecurityError::Store)?;
    Ok(rows)
}

fn matching_ids(rows: &[PrincipalRow], token: &[u8]) -> Vec<String> {
    let mut matches = Vec::new();
    for row in rows {
        let candidate = credential_hash(&row.salt, token);
        if constant_time_equal(&candidate[..], &row.hash) {
            matches.push(row.id.clone());
        }
    }
    matches
}

fn stored_set<T: rusqlite::types::FromSql + Ord>(
    tx: &Transaction<'_>,
    sql: &str,
    principal_id: &str,
) -> Result<BTreeSet<T>, SecurityError> {
    let mut statement = tx.prepare(sql).map_err(|_| SecurityError::Store)?;
    let values = statement
        .query_map([principal_id], |row| row.get(0))
        .map_err(|_| SecurityError::Store)?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(|_| SecurityError::Store)?;
    Ok(values)
}

fn exact_restart_matches(
    tx: &Transaction<'_>,
    manifest: &CredentialManifest,
    row: &PrincipalRow,
) -> Result<bool, SecurityError> {
    let operations: BTreeSet<String> = stored_set(
        tx,
        "SELECT operation FROM gate_admin_principal_operations WHERE principal_id=?1",
        &row.id,
    )?;
    let subjects: BTreeSet<i64> = stored_set(
        tx,
        "SELECT subject_id FROM gate_admin_principal_subjects WHERE principal_id=?1",
        &row.id,
    )?;
    let instances: BTreeSet<String> = stored_set(
        tx,
        "SELECT instance_id FROM gate_admin_principal_instances WHERE principal_id=?1",
        &row.id,
    )?;
    let namespace: Option<String> = tx
        .query_row(
            "SELECT namespace_id FROM gate_admin_principal_creation_namespaces WHERE principal_id=?1",
            [&row.id],
            |record| record.get(0),
        )
        .optional()
        .map_err(|_| SecurityError::Store)?;
    let expected_operations = manifest
        .operations
        .iter()
        .map(|operation| operation.as_str().to_owned())
        .collect();
    let expected_instances = manifest
        .instance_ids
        .iter()
        .map(ToString::to_string)
        .collect();
    Ok(row.expires_at == manifest.expires_at
        && row.revoked_at.is_none()
        && row.sealed_at.is_some()
        && row.scope_mode
            == if manifest.creation_namespace.is_some() {
                "creation_namespace"
            } else {
                "exact"
            }
        && row.predecessor
            == manifest
                .rotation
                .as_ref()
                .map(|rotation| rotation.predecessor_principal_id.clone())
        && row.overlap_deadline
            == manifest
                .rotation
                .as_ref()
                .map(|rotation| rotation.overlap_deadline)
        && operations == expected_operations
        && subjects == manifest.subject_ids
        && instances == expected_instances
        && namespace == manifest.creation_namespace.map(|uuid| uuid.to_string()))
}

/// Atomically creates and seals an absent principal, or verifies an exact read-only restart.
pub fn bootstrap(
    conn: &mut Connection,
    manifest: &CredentialManifest,
    now: i64,
) -> Result<BootstrapOutcome, SecurityError> {
    if manifest.expires_at <= now {
        return Err(SecurityError::Conflict);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| SecurityError::Store)?;
    let rows = load_principals(&tx)?;
    if rows.iter().any(|row| row.sealed_at.is_none()) {
        return Err(SecurityError::Conflict);
    }
    let matches = matching_ids(&rows, &manifest.token[..]);
    let existing = rows.iter().find(|row| row.id == manifest.principal_id);
    if let Some(row) = existing {
        if matches.as_slice() != [manifest.principal_id.as_str()]
            || !exact_restart_matches(&tx, manifest, row)?
        {
            return Err(SecurityError::Conflict);
        }
        tx.rollback().map_err(|_| SecurityError::Store)?;
        return Ok(BootstrapOutcome {
            created: false,
            scanned_principals: rows.len(),
        });
    }
    if !matches.is_empty() {
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
            manifest
                .rotation
                .as_ref()
                .map(|value| &value.predecessor_principal_id),
            manifest
                .rotation
                .as_ref()
                .map(|value| value.overlap_deadline),
        ],
    )
    .map_err(|_| SecurityError::Conflict)?;
    for operation in &manifest.operations {
        tx.execute(
            "INSERT INTO gate_admin_principal_operations VALUES (?1, ?2)",
            params![manifest.principal_id, operation.as_str()],
        )
        .map_err(|_| SecurityError::Store)?;
    }
    for subject_id in &manifest.subject_ids {
        tx.execute(
            "INSERT INTO gate_admin_principal_subjects VALUES (?1, ?2)",
            params![manifest.principal_id, subject_id],
        )
        .map_err(|_| SecurityError::Store)?;
    }
    for instance_id in &manifest.instance_ids {
        tx.execute(
            "INSERT INTO gate_admin_principal_instances VALUES (?1, ?2)",
            params![manifest.principal_id, instance_id.to_string()],
        )
        .map_err(|_| SecurityError::Store)?;
    }
    if let Some(namespace) = manifest.creation_namespace {
        tx.execute(
            "INSERT INTO gate_admin_principal_creation_namespaces VALUES (?1, ?2)",
            params![manifest.principal_id, namespace.to_string()],
        )
        .map_err(|_| SecurityError::Store)?;
    }
    tx.execute(
        "UPDATE gate_admin_principals SET sealed_at=?2 WHERE principal_id=?1",
        params![manifest.principal_id, now],
    )
    .map_err(|_| SecurityError::Store)?;
    tx.commit().map_err(|_| SecurityError::Store)?;
    Ok(BootstrapOutcome {
        created: true,
        scanned_principals: rows.len(),
    })
}

pub fn revoke(conn: &Connection, principal_id: &str, now: i64) -> Result<(), SecurityError> {
    let changed = conn
        .execute(
            "UPDATE gate_admin_principals SET revoked_at=?2
             WHERE principal_id=?1 AND sealed_at IS NOT NULL AND revoked_at IS NULL",
            params![principal_id, now],
        )
        .map_err(|_| SecurityError::Conflict)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(SecurityError::Conflict)
    }
}

#[derive(Clone, Debug)]
pub struct Authorized {
    pub principal_id: String,
    pub subject_id: i64,
    pub instance_id: Uuid,
}

/// Full-scans every credential and authorizes only one current principal and one target scope.
pub fn authorize(
    conn: &mut Connection,
    authorization: Option<&str>,
    operation: Operation,
    subject_id: i64,
    instance_id: Uuid,
    now: i64,
) -> Result<Authorized, SecurityError> {
    let raw = authorization
        .and_then(|header| header.strip_prefix("Bearer "))
        .ok_or(SecurityError::Unauthorized)?;
    if raw.contains(char::is_whitespace) {
        return Err(SecurityError::Unauthorized);
    }
    let mut decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|_| SecurityError::Unauthorized)?,
    );
    if decoded.len() != 32 || URL_SAFE_NO_PAD.encode(&*decoded) != raw {
        decoded.zeroize();
        return Err(SecurityError::Unauthorized);
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .map_err(|_| SecurityError::Store)?;
    let rows = load_principals(&tx)?;
    if rows.iter().any(|row| row.sealed_at.is_none()) {
        return Err(SecurityError::Unauthorized);
    }
    let matches = matching_ids(&rows, &decoded);
    if matches.len() != 1 {
        return Err(SecurityError::Unauthorized);
    }
    let row = rows
        .iter()
        .find(|row| row.id == matches[0])
        .ok_or(SecurityError::Unauthorized)?;
    let successor_deadline: Option<i64> = tx
        .query_row(
            "SELECT overlap_deadline FROM gate_admin_principals
             WHERE predecessor_principal_id=?1 AND sealed_at IS NOT NULL",
            [&row.id],
            |record| record.get(0),
        )
        .optional()
        .map_err(|_| SecurityError::Store)?;
    if row.revoked_at.is_some()
        || row.expires_at <= now
        || successor_deadline.is_some_and(|deadline| deadline <= now)
    {
        return Err(SecurityError::Unauthorized);
    }
    let operation_allowed = tx
        .query_row(
            "SELECT 1 FROM gate_admin_principal_operations WHERE principal_id=?1 AND operation=?2",
            params![row.id, operation.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SecurityError::Store)?
        .is_some();
    let subject_allowed = tx
        .query_row(
            "SELECT 1 FROM gate_admin_principal_subjects WHERE principal_id=?1 AND subject_id=?2",
            params![row.id, subject_id],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SecurityError::Store)?
        .is_some();
    let instance_allowed = if row.scope_mode == "exact" {
        tx.query_row(
            "SELECT 1 FROM gate_admin_principal_instances WHERE principal_id=?1 AND instance_id=?2",
            params![row.id, instance_id.to_string()],
            |_| Ok(()),
        )
        .optional()
        .map_err(|_| SecurityError::Store)?
        .is_some()
    } else {
        let namespace: String = tx
            .query_row(
                "SELECT namespace_id FROM gate_admin_principal_creation_namespaces WHERE principal_id=?1",
                [&row.id],
                |record| record.get(0),
            )
            .map_err(|_| SecurityError::Store)?;
        let agent_id: String = tx
            .query_row(
                "SELECT agent_id FROM agents WHERE subject_id=?1",
                [subject_id],
                |record| record.get(0),
            )
            .map_err(|_| SecurityError::Unauthorized)?;
        let namespace = Uuid::parse_str(&namespace).map_err(|_| SecurityError::Store)?;
        Uuid::new_v5(&namespace, format!("instance\0{agent_id}").as_bytes()) == instance_id
    };
    if !operation_allowed || !subject_allowed || !instance_allowed {
        return Err(SecurityError::Unauthorized);
    }
    tx.commit().map_err(|_| SecurityError::Store)?;
    Ok(Authorized {
        principal_id: row.id.clone(),
        subject_id,
        instance_id,
    })
}

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

pub fn append_audit(
    conn: &Connection,
    request_id: Uuid,
    attempted_at: i64,
    operation: Operation,
    authorized: Option<&Authorized>,
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
            authorized.map(|value| &value.principal_id),
            operation.as_str(),
            authorized.map(|value| value.subject_id),
            authorized.map(|value| value.instance_id.to_string()),
            result_class,
        ],
    )
    .map_err(|_| SecurityError::Store)?;
    Ok(())
}

#[cfg(test)]
mod tests;
