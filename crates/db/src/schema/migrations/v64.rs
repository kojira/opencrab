//! v64: drop the gate-admin credential tables (D-1098).
//!
//! Gateways declare their instances and bindings with `provision` over the ordinary gate socket,
//! with no credential. The principals, their scopes, the request audit and the single-use subject
//! grants were only used by the removed gate-admin socket.

use super::Migration;

/// Removed with gate-admin. Dropped in dependency order (children before parents). Kept in sync
/// with the fresh-schema cleanup in `schema::initialize`.
pub(in crate::schema) const GATE_ADMIN_TABLES: [&str; 7] = [
    "gate_admin_request_audit",
    "gate_admin_principal_creation_namespaces",
    "gate_admin_principal_instances",
    "gate_admin_principal_subjects",
    "gate_admin_principal_operations",
    "gate_admin_principals",
    "subject_association_grants",
];

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 64,
    description: "drop gate-admin credential tables (D-1098)",
    up: |conn| {
        for table in GATE_ADMIN_TABLES {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        }
        Ok(())
    },
}];
