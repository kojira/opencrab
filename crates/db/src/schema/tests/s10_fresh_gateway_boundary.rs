#[test]
fn s10_fresh_schema_omits_legacy_gateway_tables_but_retains_generic_state() {
    let conn = crate::init_memory().expect("fresh core schema");
    for table in migrations::LEGACY_GATEWAY_SETTINGS_TABLES {
        assert!(
            !table_exists(&conn, table).expect("inspect fresh schema"),
            "fresh core schema must not create legacy gateway table {table}"
        );
    }
    for table in ["api_principals", "agents", "sessions", "gate_instances", "deliveries"] {
        assert!(
            table_exists(&conn, table).expect("inspect retained core schema"),
            "fresh core schema must retain generic table {table}"
        );
    }
}

/// D-1097 v63: an existing database still carrying the pre-separation settings tables (some hold
/// a plaintext bot token or a Nostr secret key) loses all of them; generic state is untouched and
/// re-running the migration is a no-op.
#[test]
fn v63_drops_legacy_gateway_settings_tables_from_an_upgraded_database() {
    assert_eq!(latest_version(), 63);
    let conn = crate::init_memory().expect("fresh");
    for table in migrations::LEGACY_GATEWAY_SETTINGS_TABLES {
        conn.execute_batch(&format!("CREATE TABLE {table} (id TEXT PRIMARY KEY, v TEXT);"))
            .unwrap();
        conn.execute(&format!("INSERT INTO {table} (id, v) VALUES ('x', 'y')"), [])
            .unwrap();
    }
    conn.execute(
        "INSERT INTO sessions (id, theme, created_at, updated_at)
         VALUES ('kept-session', 't', 'now', 'now')",
        [],
    )
    .unwrap();
    conn.execute_batch("PRAGMA user_version = 62;").unwrap();

    initialize(&conn).expect("upgrade 62 -> 63");
    initialize(&conn).expect("re-run is a no-op");

    assert_eq!(schema_version(&conn).unwrap(), 63);
    for table in migrations::LEGACY_GATEWAY_SETTINGS_TABLES {
        assert!(
            !table_exists(&conn, table).unwrap(),
            "v63 must drop legacy gateway table {table}"
        );
    }
    let kept: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions WHERE id='kept-session'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(kept, 1);
}
