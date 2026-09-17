use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 51,
    description: "index open gate bindings by exact address",
    up: |conn| {
        conn.execute_batch(
            "CREATE INDEX idx_gate_bindings_open_address_lookup
             ON gate_bindings(address, binding_id, instance_id)
             WHERE closed_at IS NULL;",
        )
    },
}];
