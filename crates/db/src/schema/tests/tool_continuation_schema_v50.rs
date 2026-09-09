fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect columns")
}

#[test]
fn v49_upgrade_adds_v50_schema_without_overwriting_operator_pricing() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();
    conn.execute_batch(
        "DROP TABLE tool_completion_events;
         DROP TABLE tool_completion_requests;
         DROP TABLE tool_call_correlations;
         ALTER TABLE model_pricing DROP COLUMN max_total_tokens;
         ALTER TABLE model_pricing DROP COLUMN max_input_tokens;
         PRAGMA user_version = 49;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO model_pricing
         (provider, model, input_price_per_1m, output_price_per_1m,
          context_window, max_output_tokens, updated_at)
         VALUES ('chatgpt', 'gpt-5.6-sol', 0, 0, 123456, 6543, datetime('now'))",
        [],
    )
    .unwrap();

    initialize(&conn).unwrap();
    let row: (i64, i64, Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT context_window, max_output_tokens, max_input_tokens, max_total_tokens
             FROM model_pricing WHERE provider = 'chatgpt' AND model = 'gpt-5.6-sol'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.0, 123456);
    assert_eq!(row.1, 6543);
    assert!(row.2.is_some(), "known model input budget is backfilled");
    assert_eq!(row.3, None, "independent limits do not invent shared total");
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        50
    );
}

#[test]
fn fresh_schema_has_unambiguous_model_input_and_shared_total_limits() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();

    let columns = table_columns(&conn, "model_pricing");
    assert!(
        columns.iter().any(|column| column == "max_input_tokens"),
        "最大入力と最大出力を独立に扱うmax_input_tokens列が必要: {columns:?}"
    );
    assert!(
        columns.iter().any(|column| column == "max_total_tokens"),
        "共有窓を持つモデルだけに使うmax_total_tokens列が必要: {columns:?}"
    );
}

#[test]
fn fresh_schema_has_session_scoped_tool_call_correlations() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();
    let columns = table_columns(&conn, "tool_call_correlations");
    for required in [
        "session_id",
        "sequence",
        "short_id",
        "provider_call_id",
        "created_at",
    ] {
        assert!(
            columns.iter().any(|column| column == required),
            "tool_call_correlations.{required}が必要: {columns:?}"
        );
    }
}

#[test]
fn fresh_schema_has_durable_exactly_once_tool_completion_events() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();

    let request_columns = table_columns(&conn, "tool_completion_requests");
    for required in [
        "request_id",
        "session_id",
        "request_digest",
        "request_json",
        "response_json",
        "effects_state",
        "created_at",
        "applied_at",
    ] {
        assert!(
            request_columns.iter().any(|column| column == required),
            "tool_completion_requests.{required}が必要: {request_columns:?}"
        );
    }
    let columns = table_columns(&conn, "tool_completion_events");
    for required in [
        "event_id",
        "session_id",
        "causal_turn_id",
        "tool_call_id",
        "execution_id",
        "result_log_id",
        "state",
        "included_request_id",
        "request_digest",
        "completed_at",
        "consumed_at",
    ] {
        assert!(
            columns.iter().any(|column| column == required),
            "tool_completion_events.{required}が必要: {columns:?}"
        );
    }

    let mut stmt = conn
        .prepare("PRAGMA index_list(tool_completion_events)")
        .expect("completion event index list");
    let unique_index_count = stmt
        .query_map([], |row| row.get::<_, i64>(2))
        .expect("query completion event indexes")
        .filter_map(Result::ok)
        .filter(|unique| *unique == 1)
        .count();
    assert!(
        unique_index_count >= 1,
        "execution_idとtool_call_idを冪等化するUNIQUE indexが必要"
    );
}

#[test]
fn duplicate_tool_in_execution_is_idempotent_but_batch_tools_are_distinct() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();
    conn.execute(
        "INSERT INTO memory_sessions
         (id, agent_id, session_id, log_type, content, created_at)
         VALUES (42, 'agent-1', 'session-1', 'tool_result', 'done',
                 '2026-09-08T00:00:00Z')",
        [],
    )
    .unwrap();

    for event_id in ["event-1", "event-2"] {
        conn.execute(
            "INSERT OR IGNORE INTO tool_completion_events
             (event_id, session_id, causal_turn_id, tool_call_id, execution_id,
              result_log_id, state, completed_at)
             VALUES (?1, 'session-1', 'turn-1', 't32', 'execution-1', 42, 'queued',
                     '2026-09-08T00:00:00Z')",
            [event_id],
        )
        .unwrap();
    }

    conn.execute(
        "INSERT INTO tool_completion_events
         (event_id, session_id, causal_turn_id, tool_call_id, execution_id,
          result_log_id, state, completed_at)
         VALUES ('event-3', 'session-1', 'turn-1', 't33', 'execution-1', 42,
                 'queued', '2026-09-08T00:00:00Z')",
        [],
    )
    .unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tool_completion_events WHERE execution_id = 'execution-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "同じbatchの異なるtool IDは別eventになる");
}

#[test]
fn completion_event_state_rejects_unknown_values() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();

    let result = conn.execute(
        "INSERT INTO tool_completion_events
         (event_id, session_id, causal_turn_id, tool_call_id, execution_id,
          result_log_id, state, completed_at)
         VALUES ('event-1', 'session-1', 'turn-1', 't32', 'execution-1', 42,
                 'silently_dropped', '2026-09-08T00:00:00Z')",
        [],
    );
    assert!(result.is_err(), "stateはqueued/included/consumedだけを許す");
}
