/// Production regression (2026-10-30): v64 must drop the gate-admin tables when they hold rows
/// guarded by their v53/v54 triggers and the principals' self-referencing RESTRICT foreign key.
/// The production database had a rotated principal (predecessor set), scope rows, hundreds of
/// audit rows and grants; the first v64 failed there with "FOREIGN KEY constraint failed".
#[test]
fn v64_drops_populated_gate_admin_tables_with_triggers_and_rotation() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    initialize_historical(&conn).expect("historical schema to 62");
    run_migrations(&conn, MIGRATIONS.into_iter().filter(|m| m.version <= 63)).expect("to 63");
    assert_eq!(schema_version(&conn).unwrap(), 63);

    conn.execute(
        "INSERT INTO sessions (id, theme, created_at, updated_at) VALUES ('kept', 't', 'now', 'now')",
        [],
    )
    .unwrap();
    let salt = vec![1u8; 32];
    let hash = vec![2u8; 32];
    let seal = |principal: &str, predecessor: Option<&str>, mode: &str| {
        conn.execute(
            "INSERT INTO gate_admin_principals (principal_id, credential_salt, credential_hash,
                 scope_mode, created_at, expires_at, predecessor_principal_id, overlap_deadline)
             VALUES (?1, ?2, ?3, ?4, 10, 1000, ?5, ?6)",
            rusqlite::params![
                principal,
                salt,
                hash,
                mode,
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
            "INSERT INTO gate_admin_principal_subjects VALUES (?1, 2)",
            [principal],
        )
        .unwrap();
        if mode == "exact" {
            conn.execute(
                "INSERT INTO gate_admin_principal_instances VALUES (?1, '2b0f8e2f-16a0-51a2-8ed9-7571811d948b')",
                [principal],
            )
            .unwrap();
        } else {
            conn.execute(
                "INSERT INTO gate_admin_principal_creation_namespaces VALUES (?1, 'da7b7559-0e49-5dcd-b0d4-24693dc315ec')",
                [principal],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE gate_admin_principals SET sealed_at = 20 WHERE principal_id = ?1",
            [principal],
        )
        .unwrap();
    };
    seal("prod-nostr-daemon", None, "exact");
    seal("prod-nostr-daemon-rotated", Some("prod-nostr-daemon"), "exact");
    seal("crab-town", None, "creation_namespace");
    for i in 0..5 {
        conn.execute(
            "INSERT INTO gate_admin_request_audit (audit_id, request_id, attempted_at, principal_id,
                 operation, result_class)
             VALUES (?1, ?2, ?3, 'prod-nostr-daemon-rotated', 'instance.read', 'succeeded')",
            rusqlite::params![
                format!("00000000-0000-0000-0000-00000000000{i}"),
                format!("10000000-0000-0000-0000-00000000000{i}"),
                100 + i
            ],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO subject_association_grants (grant_hash, agent_id, subject_id, expires_at)
         VALUES (?1, 'agent', 2, 1000)",
        [vec![3u8; 64]],
    )
    .unwrap();
    let triggers_before: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND tbl_name IN (
                 'gate_admin_request_audit', 'gate_admin_principal_creation_namespaces',
                 'gate_admin_principal_instances', 'gate_admin_principal_subjects',
                 'gate_admin_principal_operations', 'gate_admin_principals',
                 'subject_association_grants')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(triggers_before >= 15, "v53/v54 triggers present: {triggers_before}");
    let other_tables_before: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .filter(|name: &String| !migrations::GATE_ADMIN_TABLES.contains(&name.as_str()))
        .collect();

    initialize(&conn).expect("upgrade a populated 63 database to 64");

    assert_eq!(schema_version(&conn).unwrap(), 64);
    for table in migrations::GATE_ADMIN_TABLES {
        assert!(!table_exists(&conn, table).unwrap(), "v64 must drop {table}");
    }
    let triggers_after: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger'
             AND (name LIKE 'gate_admin%' OR name LIKE 'subject_grants%')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(triggers_after, 0);
    let other_tables_after: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(other_tables_after, other_tables_before);
    let kept: i64 = conn
        .query_row("SELECT count(*) FROM sessions WHERE id='kept'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1);
    let violations: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(violations, 0);
}
