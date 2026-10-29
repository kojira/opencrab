#[test]
fn s10_fresh_schema_omits_legacy_gateway_tables_but_retains_generic_state() {
    let conn = crate::init_memory().expect("fresh core schema");
    for table in migrations::LEGACY_GATEWAY_SETTINGS_TABLES
        .into_iter()
        .chain(migrations::GATE_ADMIN_TABLES)
    {
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

    initialize(&conn).expect("upgrade 62 -> latest");
    initialize(&conn).expect("re-run is a no-op");

    assert_eq!(schema_version(&conn).unwrap(), latest_version());
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

/// D-1098 v64: an existing database loses the gate-admin credential tables (principals, scopes,
/// audit, single-use subject grants). Instances, bindings and sessions are untouched.
#[test]
fn v64_drops_gate_admin_tables_and_keeps_instances_and_bindings() {
    assert_eq!(latest_version(), 64);
    let conn = crate::init_memory().expect("fresh");
    for table in migrations::GATE_ADMIN_TABLES {
        conn.execute_batch(&format!("CREATE TABLE {table} (id TEXT PRIMARY KEY, v TEXT);"))
            .unwrap();
        conn.execute(&format!("INSERT INTO {table} (id, v) VALUES ('x', 'y')"), [])
            .unwrap();
    }
    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('kept-agent', 'n', 'p')",
        [],
    )
    .unwrap();
    let subject: i64 = conn
        .query_row("SELECT subject_id FROM agents WHERE agent_id='kept-agent'", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances (instance_id, kind_id, subject_id, revision, enabled,
             config_b64, config_digest, created_at, updated_at)
         VALUES ('kept-instance', 'k', ?1, 3, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject, "a".repeat(64)],
    )
    .unwrap();
    conn.execute_batch("PRAGMA user_version = 63;").unwrap();

    initialize(&conn).expect("upgrade 63 -> 64");
    initialize(&conn).expect("re-run is a no-op");

    assert_eq!(schema_version(&conn).unwrap(), 64);
    for table in migrations::GATE_ADMIN_TABLES {
        assert!(
            !table_exists(&conn, table).unwrap(),
            "v64 must drop gate-admin table {table}"
        );
    }
    let revision: i64 = conn
        .query_row(
            "SELECT revision FROM gate_instances WHERE instance_id='kept-instance'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revision, 3);
}
