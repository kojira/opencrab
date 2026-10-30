/// A second initialize on a current database changes nothing and keeps every row.
#[test]
fn initialize_is_idempotent_and_non_destructive() {
    let conn = crate::init_memory().expect("init");
    conn.execute_batch(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('sentinel', 'n', 'p')",
    )
    .unwrap();

    initialize(&conn).expect("second initialize");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE agent_id = 'sentinel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "sentinel row must survive (baseline not re-run)");
}
