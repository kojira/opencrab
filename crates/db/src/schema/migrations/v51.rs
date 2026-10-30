use super::super::helpers::column_exists;
use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 51,
    description: "index open gate bindings by exact address",
    up: |conn| {
        // v65 renames the column to `session_key` (with its own index); a re-run on such a
        // database leaves it alone.
        if !column_exists(conn, "gate_bindings", "address")? {
            return Ok(());
        }
        conn.execute_batch(
            "CREATE INDEX idx_gate_bindings_open_address_lookup
             ON gate_bindings(address, binding_id, instance_id)
             WHERE closed_at IS NULL;",
        )
    },
}];
