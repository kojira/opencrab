#[test]
fn v50_creates_generic_channel_storage_for_versioned_database() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA user_version = 49;").unwrap();

    run_migrations(&conn, MIGRATION_GROUPS.iter().flat_map(|group| group.iter())).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), 50);
    assert!(table_exists(&conn, "channel_config").unwrap());
}

#[test]
fn v50_preserves_heartbeat_audit_values_with_opaque_caller_id() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE heartbeat_instructions_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            channel_id TEXT,
            caller_identity TEXT NOT NULL,
            legacy_caller_id TEXT,
            old_value TEXT,
            new_value TEXT,
            reason TEXT,
            created_at TEXT NOT NULL
         );
         INSERT INTO heartbeat_instructions_audit
           (agent_id, scope, channel_id, caller_identity, legacy_caller_id,
            old_value, new_value, reason, created_at)
         VALUES ('a1', 'channel', 'c1', 'owner', 'u1', 'old', 'new', 'test', 'now');
         PRAGMA user_version = 49;",
    )
    .unwrap();

    run_migrations(&conn, MIGRATION_GROUPS.iter().flat_map(|group| group.iter())).unwrap();

    assert!(column_exists(&conn, "heartbeat_instructions_audit", "caller_user_id").unwrap());
    let row: (String, String, String) = conn
        .query_row(
            "SELECT agent_id, caller_user_id, new_value FROM heartbeat_instructions_audit",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("a1".into(), "u1".into(), "new".into()));
}
