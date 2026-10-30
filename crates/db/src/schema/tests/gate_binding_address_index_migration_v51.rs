const GATE_BINDING_ADDRESS_LOOKUP_INDEX: &str = "idx_gate_bindings_open_address_lookup";

/// A database at v50 with the historical `address` column (v65 later renames it).
fn historical_v50() -> Connection {
    let conn = init_historical_schema_fixture().unwrap();
    conn.execute_batch(&format!(
        "DROP INDEX IF EXISTS {GATE_BINDING_ADDRESS_LOOKUP_INDEX}; PRAGMA user_version = 50;"
    ))
    .unwrap();
    conn
}

fn upgrade_to_v51(conn: &Connection) -> rusqlite::Result<()> {
    run_migrations(conn, MIGRATIONS.into_iter().filter(|m| m.version <= 51))
}

fn gate_binding_lookup_index_shape(conn: &Connection) -> Option<(bool, bool, Vec<String>)> {
    index_shape(conn, GATE_BINDING_ADDRESS_LOOKUP_INDEX)
}

fn index_shape(conn: &Connection, index: &str) -> Option<(bool, bool, Vec<String>)> {
    let (unique, partial): (i64, i64) = conn
        .query_row(
            "SELECT il.[unique], il.partial
             FROM pragma_index_list('gate_bindings') il
             WHERE il.name = ?1",
            [index],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    let columns = conn
        .prepare(&format!(
            "SELECT name FROM pragma_index_info('{index}') ORDER BY seqno"
        ))
        .ok()?
        .query_map([], |row| row.get(0))
        .ok()?
        .collect::<Result<Vec<String>, _>>()
        .ok()?;
    Some((unique != 0, partial != 0, columns))
}

fn seed_gate_index_rows(conn: &Connection) {
    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('index-agent', 'n', 'p')",
        [],
    )
    .unwrap();
    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = 'index-agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES ('index-instance', 'generic', ?1, 1, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject_id, "0".repeat(64)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at, closed_at)
         VALUES ('index-open', 'index-instance', 'opaque-open', 1, NULL),
                ('index-closed', 'index-instance', 'opaque-closed', 2, 3)",
        [],
    )
    .unwrap();
}

fn gate_binding_rows(conn: &Connection) -> Vec<(String, String, String, i64, Option<i64>)> {
    conn.prepare(
        "SELECT binding_id, instance_id, address, created_at, closed_at
         FROM gate_bindings ORDER BY binding_id",
    )
    .unwrap()
    .query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

/// The fresh schema carries the lookup index under v65's column name.
#[test]
fn fresh_schema_has_open_session_key_lookup_index() {
    let conn = crate::init_memory().unwrap();
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert_eq!(gate_binding_lookup_index_shape(&conn), None);
    assert_eq!(
        index_shape(&conn, "idx_gate_bindings_open_session_key_lookup"),
        Some((
            false,
            true,
            vec!["session_key".into(), "binding_id".into(), "instance_id".into()]
        ))
    );
}

#[test]
fn v50_to_v51_adds_open_address_lookup_index_without_rewriting_rows() {
    let conn = historical_v50();
    seed_gate_index_rows(&conn);
    let before = gate_binding_rows(&conn);

    upgrade_to_v51(&conn).unwrap();
    upgrade_to_v51(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), 51);
    assert_eq!(gate_binding_rows(&conn), before);
    assert_eq!(
        gate_binding_lookup_index_shape(&conn),
        Some((
            false,
            true,
            vec!["address".into(), "binding_id".into(), "instance_id".into()]
        ))
    );
}

#[test]
fn v51_index_failure_rolls_back_and_keeps_version_50() {
    let conn = historical_v50();
    conn.execute_batch(&format!(
        "CREATE TABLE {GATE_BINDING_ADDRESS_LOOKUP_INDEX} (id INTEGER);"
    ))
    .unwrap();
    seed_gate_index_rows(&conn);
    let before = gate_binding_rows(&conn);

    assert!(upgrade_to_v51(&conn).is_err());
    assert_eq!(schema_version(&conn).unwrap(), 50);
    assert_eq!(gate_binding_rows(&conn), before);

    conn.execute_batch(&format!(
        "DROP TABLE {GATE_BINDING_ADDRESS_LOOKUP_INDEX};"
    ))
    .unwrap();
    upgrade_to_v51(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 51);
    assert!(gate_binding_lookup_index_shape(&conn).is_some());
}

#[test]
fn v51_index_rollback_to_v50_and_forward_reapply_preserves_rows() {
    let conn = historical_v50();
    upgrade_to_v51(&conn).unwrap();
    seed_gate_index_rows(&conn);
    let before = gate_binding_rows(&conn);

    conn.execute_batch(&format!(
        "DROP INDEX {GATE_BINDING_ADDRESS_LOOKUP_INDEX}; PRAGMA user_version = 50;"
    ))
    .unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 50);
    assert_eq!(gate_binding_rows(&conn), before);

    upgrade_to_v51(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 51);
    assert_eq!(gate_binding_rows(&conn), before);
    assert!(gate_binding_lookup_index_shape(&conn).is_some());
}
