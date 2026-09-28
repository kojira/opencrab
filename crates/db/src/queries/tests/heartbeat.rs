// 16. test_heartbeat_log_insert
#[test]
fn test_heartbeat_log_insert() {
    let conn = setup();

    let result = insert_heartbeat_log(&conn, "agent-1", "idle", Some(r#"{"action":"none"}"#));
    assert!(result.is_ok());
}
