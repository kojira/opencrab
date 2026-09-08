#[test]
fn v47_to_v48_adds_provider_tool_history_once_and_preserves_rows() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE llm_logs (
            id TEXT PRIMARY KEY,
            agent_id TEXT NOT NULL,
            prompt TEXT NOT NULL DEFAULT '',
            response TEXT NOT NULL DEFAULT ''
         );
         INSERT INTO llm_logs (id, agent_id) VALUES ('old', 'agent');
         PRAGMA user_version = 47;",
    )
    .unwrap();

    initialize(&conn).unwrap();
    initialize(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), 48);
    assert!(column_exists(&conn, "llm_logs", "provider_tool_history").unwrap());
    let history: String = conn
        .query_row(
            "SELECT provider_tool_history FROM llm_logs WHERE id = 'old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(history, "{}");
}

#[test]
fn fresh_schema_contains_provider_tool_history() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();
    assert!(column_exists(&conn, "llm_logs", "provider_tool_history").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), 48);
}
