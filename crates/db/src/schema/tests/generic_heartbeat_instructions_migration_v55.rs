#[test]
fn s4_v55_upgrade_creates_composite_generic_heartbeat_instructions() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();

    assert!(table_exists(&conn, "session_heartbeat_instructions").unwrap());
    let columns: Vec<(String, i64)> = conn
        .prepare("SELECT name, pk FROM pragma_table_info('session_heartbeat_instructions') ORDER BY cid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        columns,
        vec![
            ("agent_id".into(), 1),
            ("session_id".into(), 2),
            ("override_text".into(), 0),
            ("updated_at".into(), 0),
        ]
    );

    let foreign_tables: Vec<String> = conn
        .prepare("SELECT DISTINCT \"table\" FROM pragma_foreign_key_list('session_heartbeat_instructions') ORDER BY \"table\"")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        foreign_tables,
        vec!["session_heartbeat_config", "sessions"],
        "instructions must be bound to the composite config target and generic session"
    );
    assert_eq!(latest_version(), 56);
}

#[test]
fn s4_v55_upgrade_and_fresh_schema_are_identical() {
    let fresh = Connection::open_in_memory().unwrap();
    initialize(&fresh).unwrap();

    let upgraded = Connection::open_in_memory().unwrap();
    initialize(&upgraded).unwrap();
    upgraded
        .execute_batch(
            "DROP TABLE session_heartbeat_instructions;
             PRAGMA user_version=54;",
        )
        .unwrap();
    initialize(&upgraded).unwrap();

    let sql = |conn: &Connection| -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='session_heartbeat_instructions'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(sql(&fresh), sql(&upgraded));
}
