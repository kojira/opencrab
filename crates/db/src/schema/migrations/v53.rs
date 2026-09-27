use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 53,
    description: "add sealed generic gate-admin principals, normalized scopes, and audit",
    up: |conn| conn.execute_batch(GATE_ADMIN_SECURITY_SQL),
}];

const GATE_ADMIN_SECURITY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS gate_admin_principals (
    principal_id TEXT PRIMARY KEY
        CHECK(length(CAST(principal_id AS BLOB)) BETWEEN 1 AND 128)
        CHECK(principal_id NOT GLOB '*[^A-Za-z0-9._-]*'),
    credential_salt BLOB NOT NULL CHECK(length(credential_salt) = 32),
    credential_hash BLOB NOT NULL CHECK(length(credential_hash) = 32),
    scope_mode TEXT NOT NULL CHECK(scope_mode IN ('exact', 'creation_namespace')),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL CHECK(expires_at > created_at),
    revoked_at INTEGER CHECK(revoked_at IS NULL OR (sealed_at IS NOT NULL AND revoked_at >= created_at)),
    sealed_at INTEGER CHECK(sealed_at IS NULL OR sealed_at >= created_at),
    predecessor_principal_id TEXT,
    overlap_deadline INTEGER,
    CHECK((predecessor_principal_id IS NULL) = (overlap_deadline IS NULL)),
    CHECK(overlap_deadline IS NULL OR overlap_deadline > created_at),
    CHECK(predecessor_principal_id IS NULL OR predecessor_principal_id <> principal_id),
    FOREIGN KEY(predecessor_principal_id) REFERENCES gate_admin_principals(principal_id) ON DELETE RESTRICT,
    UNIQUE(predecessor_principal_id)
);
CREATE INDEX IF NOT EXISTS idx_gate_admin_principals_status
    ON gate_admin_principals(revoked_at, expires_at);
CREATE INDEX IF NOT EXISTS idx_gate_admin_principals_predecessor
    ON gate_admin_principals(predecessor_principal_id);

CREATE TABLE IF NOT EXISTS gate_admin_principal_operations (
    principal_id TEXT NOT NULL,
    operation TEXT NOT NULL CHECK(operation IN (
        'instance.read', 'instance.put', 'instance.delete',
        'instance.revise', 'binding.put', 'binding.delete'
    )),
    PRIMARY KEY(principal_id, operation),
    FOREIGN KEY(principal_id) REFERENCES gate_admin_principals(principal_id) ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS gate_admin_principal_subjects (
    principal_id TEXT NOT NULL,
    subject_id INTEGER NOT NULL CHECK(subject_id > 0),
    PRIMARY KEY(principal_id, subject_id),
    FOREIGN KEY(principal_id) REFERENCES gate_admin_principals(principal_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_gate_admin_principal_subjects_target
    ON gate_admin_principal_subjects(subject_id, principal_id);
CREATE TABLE IF NOT EXISTS gate_admin_principal_instances (
    principal_id TEXT NOT NULL,
    instance_id TEXT NOT NULL
        CHECK(length(instance_id) = 36 AND instance_id = lower(instance_id)),
    PRIMARY KEY(principal_id, instance_id),
    FOREIGN KEY(principal_id) REFERENCES gate_admin_principals(principal_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_gate_admin_principal_instances_target
    ON gate_admin_principal_instances(instance_id, principal_id);
CREATE TABLE IF NOT EXISTS gate_admin_principal_creation_namespaces (
    principal_id TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL
        CHECK(length(namespace_id) = 36 AND namespace_id = lower(namespace_id)),
    FOREIGN KEY(principal_id) REFERENCES gate_admin_principals(principal_id) ON DELETE RESTRICT
);

CREATE TRIGGER IF NOT EXISTS gate_admin_principals_no_delete
BEFORE DELETE ON gate_admin_principals BEGIN
    SELECT RAISE(ABORT, 'gate-admin principals are immutable');
END;
CREATE TRIGGER IF NOT EXISTS gate_admin_principals_update_guard
BEFORE UPDATE ON gate_admin_principals
WHEN NOT (
    OLD.sealed_at IS NULL AND NEW.sealed_at IS NOT NULL AND OLD.revoked_at IS NULL
    AND NEW.revoked_at IS NULL
    AND NEW.principal_id IS OLD.principal_id
    AND NEW.credential_salt IS OLD.credential_salt
    AND NEW.credential_hash IS OLD.credential_hash
    AND NEW.scope_mode IS OLD.scope_mode
    AND NEW.created_at IS OLD.created_at
    AND NEW.expires_at IS OLD.expires_at
    AND NEW.predecessor_principal_id IS OLD.predecessor_principal_id
    AND NEW.overlap_deadline IS OLD.overlap_deadline
) AND NOT (
    OLD.sealed_at IS NOT NULL AND NEW.sealed_at IS OLD.sealed_at
    AND OLD.revoked_at IS NULL AND NEW.revoked_at IS NOT NULL
    AND NEW.principal_id IS OLD.principal_id
    AND NEW.credential_salt IS OLD.credential_salt
    AND NEW.credential_hash IS OLD.credential_hash
    AND NEW.scope_mode IS OLD.scope_mode
    AND NEW.created_at IS OLD.created_at
    AND NEW.expires_at IS OLD.expires_at
    AND NEW.predecessor_principal_id IS OLD.predecessor_principal_id
    AND NEW.overlap_deadline IS OLD.overlap_deadline
) BEGIN
    SELECT RAISE(ABORT, 'illegal gate-admin principal transition');
END;
CREATE TRIGGER IF NOT EXISTS gate_admin_principals_seal_cardinality
BEFORE UPDATE OF sealed_at ON gate_admin_principals
WHEN OLD.sealed_at IS NULL AND NEW.sealed_at IS NOT NULL AND (
    (SELECT count(*) FROM gate_admin_principal_operations WHERE principal_id=OLD.principal_id) = 0
    OR (SELECT count(*) FROM gate_admin_principal_subjects WHERE principal_id=OLD.principal_id) = 0
    OR (OLD.scope_mode='exact' AND (
        (SELECT count(*) FROM gate_admin_principal_instances WHERE principal_id=OLD.principal_id) = 0
        OR (SELECT count(*) FROM gate_admin_principal_creation_namespaces WHERE principal_id=OLD.principal_id) <> 0
    ))
    OR (OLD.scope_mode='creation_namespace' AND (
        (SELECT count(*) FROM gate_admin_principal_instances WHERE principal_id=OLD.principal_id) <> 0
        OR (SELECT count(*) FROM gate_admin_principal_creation_namespaces WHERE principal_id=OLD.principal_id) <> 1
    ))
) BEGIN
    SELECT RAISE(ABORT, 'incomplete gate-admin scope');
END;

CREATE TRIGGER IF NOT EXISTS gate_admin_operations_insert_guard BEFORE INSERT ON gate_admin_principal_operations
WHEN COALESCE((SELECT sealed_at IS NOT NULL FROM gate_admin_principals WHERE principal_id=NEW.principal_id), 1) <> 0
BEGIN SELECT RAISE(ABORT, 'scope principal must be unsealed'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_subjects_insert_guard BEFORE INSERT ON gate_admin_principal_subjects
WHEN COALESCE((SELECT sealed_at IS NOT NULL FROM gate_admin_principals WHERE principal_id=NEW.principal_id), 1) <> 0
BEGIN SELECT RAISE(ABORT, 'scope principal must be unsealed'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_instances_insert_guard BEFORE INSERT ON gate_admin_principal_instances
WHEN COALESCE((SELECT sealed_at IS NOT NULL FROM gate_admin_principals WHERE principal_id=NEW.principal_id), 1) <> 0
 OR COALESCE((SELECT scope_mode FROM gate_admin_principals WHERE principal_id=NEW.principal_id), '') <> 'exact'
BEGIN SELECT RAISE(ABORT, 'invalid exact scope insertion'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_namespaces_insert_guard BEFORE INSERT ON gate_admin_principal_creation_namespaces
WHEN COALESCE((SELECT sealed_at IS NOT NULL FROM gate_admin_principals WHERE principal_id=NEW.principal_id), 1) <> 0
 OR COALESCE((SELECT scope_mode FROM gate_admin_principals WHERE principal_id=NEW.principal_id), '') <> 'creation_namespace'
BEGIN SELECT RAISE(ABORT, 'invalid namespace scope insertion'); END;

CREATE TRIGGER IF NOT EXISTS gate_admin_operations_no_update BEFORE UPDATE ON gate_admin_principal_operations BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_operations_no_delete BEFORE DELETE ON gate_admin_principal_operations BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_subjects_no_update BEFORE UPDATE ON gate_admin_principal_subjects BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_subjects_no_delete BEFORE DELETE ON gate_admin_principal_subjects BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_instances_no_update BEFORE UPDATE ON gate_admin_principal_instances BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_instances_no_delete BEFORE DELETE ON gate_admin_principal_instances BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_namespaces_no_update BEFORE UPDATE ON gate_admin_principal_creation_namespaces BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_namespaces_no_delete BEFORE DELETE ON gate_admin_principal_creation_namespaces BEGIN SELECT RAISE(ABORT, 'scope is immutable'); END;

CREATE TABLE IF NOT EXISTS gate_admin_request_audit (
    audit_id TEXT PRIMARY KEY CHECK(length(audit_id)=36),
    request_id TEXT NOT NULL UNIQUE CHECK(length(request_id)=36),
    attempted_at INTEGER NOT NULL,
    principal_id TEXT,
    operation TEXT NOT NULL CHECK(operation IN (
        'instance.read', 'instance.put', 'instance.delete',
        'instance.revise', 'binding.put', 'binding.delete'
    )),
    authorized_subject_id INTEGER CHECK(authorized_subject_id IS NULL OR authorized_subject_id > 0),
    authorized_instance_id TEXT,
    result_class TEXT NOT NULL CHECK(result_class IN (
        'succeeded', 'idempotent', 'unauthorized', 'bad_request',
        'not_found', 'conflict', 'store_error'
    )),
    FOREIGN KEY(principal_id) REFERENCES gate_admin_principals(principal_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_gate_admin_audit_principal_time
    ON gate_admin_request_audit(principal_id, attempted_at);
CREATE TRIGGER IF NOT EXISTS gate_admin_audit_no_update BEFORE UPDATE ON gate_admin_request_audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;
CREATE TRIGGER IF NOT EXISTS gate_admin_audit_no_delete BEFORE DELETE ON gate_admin_request_audit BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;
"#;
