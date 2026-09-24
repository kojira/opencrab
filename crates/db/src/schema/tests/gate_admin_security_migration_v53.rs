fn insert_unsealed_exact(conn: &Connection, id: &str, salt: u8, now: i64) {
    conn.execute(
        "INSERT INTO gate_admin_principals
         (principal_id, credential_salt, credential_hash, scope_mode, created_at, expires_at,
          revoked_at, sealed_at, predecessor_principal_id, overlap_deadline)
         VALUES (?1, zeroblob(32), ?2, 'exact', ?3, ?4, NULL, NULL, NULL, NULL)",
        rusqlite::params![id, vec![salt; 32], now, now + 1_000],
    )
    .unwrap();
}

fn add_exact_scope(conn: &Connection, id: &str) {
    conn.execute(
        "INSERT INTO gate_admin_principal_operations VALUES (?1, 'instance.read')",
        [id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_admin_principal_subjects VALUES (?1, 1)",
        [id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_admin_principal_instances VALUES (?1, '00000000-0000-0000-0000-000000000001')",
        [id],
    )
    .unwrap();
}

#[test]
fn gate_admin_security_schema_is_installed_on_fresh_and_populated_databases() {
    let fresh = Connection::open_in_memory().unwrap();
    initialize(&fresh).unwrap();
    let expected = [
        "gate_admin_principals",
        "gate_admin_principal_operations",
        "gate_admin_principal_subjects",
        "gate_admin_principal_instances",
        "gate_admin_principal_creation_namespaces",
        "gate_admin_request_audit",
    ];
    for table in expected {
        assert!(table_exists(&fresh, table).unwrap(), "missing {table}");
    }

    let populated = Connection::open_in_memory().unwrap();
    populated
        .execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE retained_fixture(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO retained_fixture(value) VALUES ('preserved');
             PRAGMA user_version=52;",
        )
        .unwrap();
    initialize(&populated).unwrap();
    assert_eq!(
        populated
            .query_row("SELECT value FROM retained_fixture", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "preserved"
    );
    for table in expected {
        assert!(table_exists(&populated, table).unwrap(), "missing {table}");
    }
}

#[test]
fn gate_admin_security_migration_rolls_back_as_one_unit() {
    fn failing(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE gate_admin_rollback_fixture(id INTEGER PRIMARY KEY);
             INSERT INTO definitely_missing_table VALUES (1);",
        )
    }
    static FAILING: &[Migration] = &[Migration {
        version: 53,
        description: "S1 rollback fixture",
        up: failing,
    }];
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA user_version=52").unwrap();
    assert!(run_migrations(&conn, FAILING).is_err());
    assert_eq!(schema_version(&conn).unwrap(), 52);
    assert!(!table_exists(&conn, "gate_admin_rollback_fixture").unwrap());
}

#[test]
fn gate_admin_seal_revoke_and_scope_transition_matrix_is_enforced() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    initialize(&conn).unwrap();
    insert_unsealed_exact(&conn, "operator-1", 1, 100);
    add_exact_scope(&conn, "operator-1");

    conn.execute(
        "UPDATE gate_admin_principals SET sealed_at=101 WHERE principal_id='operator-1'",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO gate_admin_principal_subjects VALUES ('operator-1', 2)",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "UPDATE gate_admin_principals SET sealed_at=102 WHERE principal_id='operator-1'",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "UPDATE gate_admin_principals SET sealed_at=NULL WHERE principal_id='operator-1'",
            [],
        )
        .is_err());
    insert_unsealed_exact(&conn, "combined", 3, 100);
    add_exact_scope(&conn, "combined");
    assert!(conn
        .execute(
            "UPDATE gate_admin_principals SET sealed_at=101, revoked_at=102 WHERE principal_id='combined'",
            [],
        )
        .is_err());
    conn.execute(
        "UPDATE gate_admin_principals SET revoked_at=103 WHERE principal_id='operator-1'",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "UPDATE gate_admin_principals SET revoked_at=NULL WHERE principal_id='operator-1'",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "UPDATE gate_admin_principals SET revoked_at=104 WHERE principal_id='operator-1'",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "DELETE FROM gate_admin_request_audit",
            [],
        )
        .is_ok(), "empty append-only table delete is a no-op");
}

#[test]
fn gate_admin_scope_cardinality_and_append_only_audit_are_enforced() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    initialize(&conn).unwrap();
    insert_unsealed_exact(&conn, "incomplete", 2, 100);
    assert!(conn
        .execute(
            "UPDATE gate_admin_principals SET sealed_at=101 WHERE principal_id='incomplete'",
            [],
        )
        .is_err());

    conn.execute(
        "INSERT INTO gate_admin_request_audit
         VALUES ('00000000-0000-0000-0000-000000000011', '00000000-0000-0000-0000-000000000012',
                 200, NULL, 'instance.read', NULL, NULL, 'unauthorized')",
        [],
    )
    .unwrap();
    assert!(conn
        .execute(
            "UPDATE gate_admin_request_audit SET result_class='succeeded'",
            [],
        )
        .is_err());
    assert!(conn.execute("DELETE FROM gate_admin_request_audit", []).is_err());
}
