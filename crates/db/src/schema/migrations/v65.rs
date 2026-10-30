//! v65: core keeps sessions only (D-1099).
//!
//! - Drops `pending_interactions`: it held A2UI surfaces a transport rendered (`send_ui`) with a
//!   platform channel id. No gateway renders A2UI and core no longer has `send_ui`.
//! - Renames `gate_bindings.address` to `session_key`: the column holds the opaque session key a
//!   gateway gives in its frames. Rows, row ids and stored sessions are unchanged; the two
//!   partial indexes are rebuilt under the new column name.

use rusqlite::Connection;

use super::super::helpers::column_exists;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 65,
    description: "drop pending_interactions; gate_bindings.address -> session_key (D-1099)",
    up: sessions_only,
}];

fn sessions_only(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("DROP TABLE IF EXISTS pending_interactions;")?;
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_gate_bindings_open_address;
         DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;",
    )?;
    if column_exists(conn, "gate_bindings", "address")? {
        conn.execute_batch("ALTER TABLE gate_bindings RENAME COLUMN address TO session_key;")?;
    }
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_gate_bindings_open_session_key
         ON gate_bindings(instance_id, session_key) WHERE closed_at IS NULL;
         CREATE INDEX IF NOT EXISTS idx_gate_bindings_open_session_key_lookup
         ON gate_bindings(session_key, binding_id, instance_id) WHERE closed_at IS NULL;",
    )
}
