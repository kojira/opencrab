/// D-1099 v65 on a database shaped like production at v64: `foreign_keys=ON`, migrated rows with
/// non-uuid5 ids whose session is the key, a closed row, rows whose session is the legacy
/// `extgate-<id>`, both partial indexes, and rows referenced by `deliveries`. The column is renamed
/// to `session_key` with every value, id and session kept; the indexes are rebuilt; the
/// referencing rows still resolve; re-running is a no-op.
#[test]
fn v65_renames_the_session_key_column_and_drops_pending_interactions() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    initialize_historical(&conn).expect("historical schema to 62");
    run_migrations(&conn, MIGRATIONS.into_iter().filter(|m| m.version <= 64)).expect("to 64");
    assert_eq!(schema_version(&conn).unwrap(), 64);
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pending_interactions (id TEXT PRIMARY KEY, channel_id TEXT);
         INSERT INTO pending_interactions VALUES ('x', 'y');",
    )
    .unwrap();

    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('agent-a', 'n', 'p')",
        [],
    )
    .unwrap();
    let subject: i64 = conn
        .query_row("SELECT subject_id FROM agents WHERE agent_id='agent-a'", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances (instance_id, kind_id, subject_id, revision, enabled,
             config_b64, config_digest, created_at, updated_at)
         VALUES ('inst-1', 'discord', ?1, 3, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject, "a".repeat(64)],
    )
    .unwrap();
    let rows: [(&str, &str, Option<i64>, &str); 4] = [
        ("legacy-row-open", "discord-a-1-2", None, "discord-a-1-2"),
        ("legacy-row-closed", "discord-a-1-3", Some(9), "discord-a-1-3"),
        ("legacy-row-reopened", "discord-a-1-3", None, "discord-a-1-3"),
        ("v4-row", "web-conv-1", None, "extgate-v4-row"),
    ];
    for (id, key, closed, session) in rows {
        conn.execute(
            "INSERT INTO sessions (id, theme, created_at, updated_at) VALUES (?1, 't', 'now', 'now')
             ON CONFLICT(id) DO NOTHING",
            [session],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at, closed_at, session_id)
             VALUES (?1, 'inst-1', ?2, 1, ?3, ?4)",
            rusqlite::params![id, key, closed, session],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO deliveries (delivery_id, binding_id, payload_json, state, created_at, updated_at)
         VALUES ('d-1', 'legacy-row-open', '{\"text\":\"hi\"}', 'delivered', 1, 1)",
        [],
    )
    .unwrap();
    let before = snapshot(&conn, "address");

    initialize(&conn).expect("upgrade 64 -> 65");
    initialize(&conn).expect("re-run is a no-op");

    assert_eq!(schema_version(&conn).unwrap(), 65);
    assert!(!table_exists(&conn, "pending_interactions").unwrap());
    assert!(!column_exists(&conn, "gate_bindings", "address").unwrap());
    assert_eq!(snapshot(&conn, "session_key"), before);
    let indexes: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='gate_bindings' AND sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        indexes,
        ["idx_gate_bindings_open_session_key", "idx_gate_bindings_open_session_key_lookup"]
    );
    // The unique partial index still allows one open row per key.
    let duplicate_open = conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, session_key, created_at)
         VALUES ('dup', 'inst-1', 'discord-a-1-2', 2)",
        [],
    );
    assert!(duplicate_open.is_err());
    let fk_violations: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk_violations, 0);
    let schema_sql: Vec<String> = conn
        .prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        schema_sql.iter().all(|sql| !sql.contains("address")),
        "no schema object names the old column"
    );
}

/// A fresh database has `session_key` and no `pending_interactions`.
#[test]
fn v65_fresh_database_has_session_key_only() {
    let conn = crate::init_memory().expect("fresh");
    assert_eq!(schema_version(&conn).unwrap(), 65);
    assert!(!table_exists(&conn, "pending_interactions").unwrap());
    assert!(column_exists(&conn, "gate_bindings", "session_key").unwrap());
    assert!(!column_exists(&conn, "gate_bindings", "address").unwrap());
}

fn snapshot(conn: &Connection, key_column: &str) -> Vec<(String, String, Option<i64>, Option<String>)> {
    conn.prepare(&format!(
        "SELECT binding_id, {key_column}, closed_at, session_id FROM gate_bindings ORDER BY binding_id"
    ))
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}
