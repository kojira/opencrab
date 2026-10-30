//! v64: drop the gate-admin credential tables (D-1098).
//!
//! Gateways declare their instances and bindings with `provision` over the ordinary gate socket,
//! with no credential. The principals, their scopes, the request audit and the single-use subject
//! grants were only used by the removed gate-admin socket.

use rusqlite::Connection;

use super::super::helpers::{column_exists, table_exists};
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
    up: drop_gate_admin_tables,
}];

/// With `foreign_keys=ON`, `DROP TABLE` first deletes every row. On a populated database those
/// deletes hit the tables' own append-only / immutability triggers (v53, v54) and the principals'
/// self-referencing `ON DELETE RESTRICT` (a rotated principal names its predecessor). So the
/// triggers go first, the self-reference is cleared, and rows are removed children before parents.
pub(in crate::schema) fn drop_gate_admin_tables(conn: &Connection) -> rusqlite::Result<()> {
    let placeholders = vec!["?"; GATE_ADMIN_TABLES.len()].join(",");
    let triggers: Vec<String> = conn
        .prepare(&format!(
            "SELECT name FROM sqlite_master WHERE type='trigger' AND tbl_name IN ({placeholders})"
        ))?
        .query_map(rusqlite::params_from_iter(GATE_ADMIN_TABLES), |row| {
            row.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    for trigger in triggers {
        conn.execute_batch(&format!("DROP TRIGGER IF EXISTS \"{trigger}\";"))?;
    }
    if column_exists(conn, "gate_admin_principals", "predecessor_principal_id")? {
        conn.execute_batch(
            "UPDATE gate_admin_principals
             SET predecessor_principal_id = NULL, overlap_deadline = NULL;",
        )?;
    }
    for table in GATE_ADMIN_TABLES {
        if table_exists(conn, table)? {
            conn.execute_batch(&format!("DELETE FROM {table};"))?;
        }
    }
    for table in GATE_ADMIN_TABLES {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
    }
    Ok(())
}
