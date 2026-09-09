fn setup_pre_v47(conn: &Connection) {
    conn.execute_batch(
        "DROP TABLE IF EXISTS gateway_operation_calls;
         ALTER TABLE gate_instances DROP COLUMN operation_declaration_digest;
         PRAGMA user_version = 46;",
    )
    .unwrap();
}

fn gateway_operation_column_names(conn: &Connection) -> Vec<String> {
    conn.prepare("PRAGMA table_info(gateway_operation_calls)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn v47_from_user_version_46_reaches_current_schema_and_is_idempotent() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v47(&conn);
    assert_eq!(schema_version(&conn).unwrap(), 46);
    assert!(!table_exists(&conn, "gateway_operation_calls").unwrap());
    assert!(!column_exists(&conn, "gate_instances", "operation_declaration_digest").unwrap());

    initialize(&conn).expect("migrate through v50");
    assert_eq!(schema_version(&conn).unwrap(), 50);
    assert!(column_exists(&conn, "gate_instances", "operation_declaration_digest").unwrap());
    assert_eq!(
        gateway_operation_column_names(&conn),
        [
            "call_id",
            "binding_id",
            "operation",
            "payload_json",
            "result_json",
            "state",
            "error",
            "created_at",
            "updated_at",
        ]
    );

    initialize(&conn).expect("second initialize must be a no-op");
    assert_eq!(schema_version(&conn).unwrap(), 50);
    assert_eq!(
        gateway_operation_column_names(&conn),
        [
            "call_id",
            "binding_id",
            "operation",
            "payload_json",
            "result_json",
            "state",
            "error",
            "created_at",
            "updated_at",
        ]
    );
}

#[test]
fn v47_gateway_operation_constraints_reject_invalid_rows() {
    let conn = crate::init_memory().expect("init");
    assert!(conn
        .execute(
            "INSERT INTO gateway_operation_calls (
               call_id, binding_id, operation, payload_json, state, created_at, updated_at
             ) VALUES ('bad', 'missing', '', 'not-json', 'unknown', 1, 1)",
            [],
        )
        .is_err());
}
