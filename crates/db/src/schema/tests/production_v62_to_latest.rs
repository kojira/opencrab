/// The oldest schema still on a production host is v62. A database shaped like production at
/// v62 (gate-admin principals with a rotation, their guard triggers, migrated bindings, sessions,
/// conversation rows) reaches the latest schema in one `initialize`, keeping every agent, session,
/// conversation row and binding.
#[test]
fn production_shaped_v62_reaches_latest_with_data_intact() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    initialize_historical(&conn).expect("schema at 62");
    assert_eq!(schema_version(&conn).unwrap(), 62);

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
         VALUES ('2b0f8e2f-16a0-51a2-8ed9-7571811d948b', 'discord', ?1, 3, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject, "a".repeat(64)],
    )
    .unwrap();
    for (id, key, closed, session) in [
        ("legacy-open", "discord-a-1-2", None::<i64>, "discord-a-1-2"),
        ("legacy-closed", "discord-a-1-3", Some(9), "discord-a-1-3"),
        ("v4-row", "web-conv-1", None, "extgate-v4-row"),
    ] {
        conn.execute(
            "INSERT INTO sessions (id, theme, created_at, updated_at) VALUES (?1, 't', 'now', 'now')",
            [session],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at, closed_at, session_id)
             VALUES (?1, '2b0f8e2f-16a0-51a2-8ed9-7571811d948b', ?2, 1, ?3, ?4)",
            rusqlite::params![id, key, closed, session],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
             VALUES ('agent-a', ?1, 'speech', 'hello', '2026-10-01T00:00:00Z')",
            [session],
        )
        .unwrap();
    }
    // A rotated gate-admin principal guarded by its v53 triggers (the shape that broke v64).
    for (principal, predecessor) in [("p-old", None), ("p-new", Some("p-old"))] {
        conn.execute(
            "INSERT INTO gate_admin_principals (principal_id, credential_salt, credential_hash,
                 scope_mode, created_at, expires_at, predecessor_principal_id, overlap_deadline)
             VALUES (?1, ?2, ?3, 'exact', 10, 1000, ?4, ?5)",
            rusqlite::params![
                principal,
                vec![1u8; 32],
                vec![2u8; 32],
                predecessor,
                predecessor.map(|_| 500i64)
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_admin_principal_operations VALUES (?1, 'instance.read')",
            [principal],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_admin_principal_subjects VALUES (?1, ?2)",
            rusqlite::params![principal, subject],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_admin_principal_instances VALUES (?1, '2b0f8e2f-16a0-51a2-8ed9-7571811d948b')",
            [principal],
        )
        .unwrap();
        conn.execute(
            "UPDATE gate_admin_principals SET sealed_at = 20 WHERE principal_id = ?1",
            [principal],
        )
        .unwrap();
    }
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    let before = (
        count("SELECT count(*) FROM agents"),
        count("SELECT count(*) FROM sessions"),
        count("SELECT count(*) FROM memory_sessions"),
        count("SELECT count(*) FROM gate_bindings"),
    );

    initialize(&conn).expect("62 -> latest");

    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert_eq!(
        (
            count("SELECT count(*) FROM agents"),
            count("SELECT count(*) FROM sessions"),
            count("SELECT count(*) FROM memory_sessions"),
            count("SELECT count(*) FROM gate_bindings"),
        ),
        before
    );
    assert_eq!(
        count("SELECT count(*) FROM gate_bindings WHERE binding_id='legacy-open' AND session_key='discord-a-1-2' AND session_id='discord-a-1-2'"),
        1
    );
    assert!(!table_exists(&conn, "gate_admin_principals").unwrap());
    assert_eq!(count("SELECT count(*) FROM pragma_foreign_key_check"), 0);
}
