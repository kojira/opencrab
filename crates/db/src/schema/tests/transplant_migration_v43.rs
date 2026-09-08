/// v43 適用前（user_version=42）の DB を模す: 新列・新表を落として版を 42 へ戻す。
fn setup_pre_v43(conn: &Connection) {
    conn.execute_batch(
        "ALTER TABLE sessions DROP COLUMN policy_json;
         DROP TABLE IF EXISTS session_watches;
         DROP TABLE IF EXISTS tool_logs;
         PRAGMA user_version = 42;",
    )
    .unwrap();
}

fn session_column_names(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM pragma_table_info('sessions') ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap()
}

fn agent_session_column_names(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM pragma_table_info('agent_sessions') ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap()
}

fn expected_v44_user_tables(before_v43: &[String]) -> Vec<String> {
    let mut expected = expected_v43_user_tables(before_v43);
    for name in [
        "deliveries",
        "external_origins",
        "gate_bindings",
        "gate_instances",
    ] {
        if !expected.iter().any(|t| t == name) {
            expected.push(name.to_string());
        }
    }
    expected.sort();
    expected
}

fn expected_v45_user_tables(before_v43: &[String]) -> Vec<String> {
    let mut expected = expected_v44_user_tables(before_v43);
    if !expected.iter().any(|t| t == "nostr_bundle_state") {
        expected.push("nostr_bundle_state".to_string());
    }
    expected.sort();
    expected
}

fn assert_user_tables_closed(conn: &Connection, expected_tables: &[String]) {
    assert_eq!(
        user_tables(conn),
        expected_tables,
        "user tables != expected closed set"
    );
}

fn assert_v43_schema(conn: &Connection) {
    assert!(
        column_exists(conn, "sessions", "policy_json").unwrap(),
        "sessions.policy_json が無い"
    );
    assert!(
        table_exists(conn, "session_watches").unwrap(),
        "session_watches が無い"
    );
    assert!(table_exists(conn, "tool_logs").unwrap(), "tool_logs が無い");
    assert!(table_exists(conn, "sessions").unwrap());
    assert!(table_exists(conn, "agent_sessions").unwrap());
    let view_n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='sessions'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(view_n, 0, "VIEW sessions が作られた");
    let tool_n: i64 = conn
        .query_row("SELECT COUNT(*) FROM tool_logs", [], |r| r.get(0))
        .unwrap();
    let watch_n: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_watches", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tool_n, 0);
    assert_eq!(watch_n, 0);
}

/// 新規 DB（SCHEMA_SQL 経路）で v43 スキーマが揃うこと。
#[test]
fn v43_fresh_db_has_transplant_schema() {
    let conn = crate::init_memory().expect("init");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    let pre = {
        let tmp = crate::init_memory().expect("pre");
        setup_pre_v43(&tmp);
        user_tables(&tmp)
    };
    assert_user_tables_closed(&conn, &expected_v45_user_tables(&pre));
    assert_v43_schema(&conn);
    let cols = session_column_names(&conn);
    assert!(
        cols.contains(&"policy_json".to_string()),
        "新規 DB の sessions に policy_json が無い: {cols:?}"
    );
    assert_eq!(
        agent_session_column_names(&conn),
        vec![
            "agent_id".to_string(),
            "session_id".to_string(),
            "last_speech_at".to_string(),
            "done_declared".to_string(),
        ],
        "agent_sessions に列を足してはならない"
    );
}

/// 既存 DB（user_version=42）へ v43 を適用し、既存行は動かず新スキーマだけ届くこと。
#[test]
fn v43_from_user_version_42_leaves_existing_rows_untouched() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v43(&conn);
    assert_eq!(schema_version(&conn).unwrap(), 42);
    assert!(!column_exists(&conn, "sessions", "policy_json").unwrap());
    assert!(!table_exists(&conn, "session_watches").unwrap());
    assert!(!table_exists(&conn, "tool_logs").unwrap());

    conn.execute_batch(
        "INSERT INTO sessions
            (id, mode, theme, phase, turn_number, status, participant_ids_json,
             facilitator_id, done_count, max_turns, metadata_json, created_at, updated_at)
         VALUES
            ('sess-a', 'facilitated', 'theme-a', 'divergent', 3, 'active', '[\"ag-a\"]',
             'ag-a', 1, 8, '{\"k\":\"v\"}', '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z');
         INSERT INTO agent_sessions (agent_id, session_id, last_speech_at, done_declared)
         VALUES ('ag-a', 'sess-a', '2026-01-02T00:00:00Z', 1);",
    )
    .unwrap();

    let before_tables = user_tables(&conn);
    let before_session: (String, String, i32, String, Option<String>) = conn
        .query_row(
            "SELECT id, theme, turn_number, participant_ids_json, metadata_json
             FROM sessions WHERE id='sess-a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();

    run_migrations(&conn, MIGRATIONS).expect("v43+v45 migration");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert_user_tables_closed(&conn, &expected_v45_user_tables(&before_tables));
    assert_v43_schema(&conn);

    let policy: String = conn
        .query_row(
            "SELECT policy_json FROM sessions WHERE id='sess-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(policy, "{}", "既存行の policy_json は DEFAULT '{{}}'");

    let after_session: (String, String, i32, String, Option<String>) = conn
        .query_row(
            "SELECT id, theme, turn_number, participant_ids_json, metadata_json
             FROM sessions WHERE id='sess-a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(before_session, after_session);

    let done: i64 = conn
        .query_row(
            "SELECT done_declared FROM agent_sessions WHERE agent_id='ag-a' AND session_id='sess-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(done, 1);

    initialize(&conn).expect("second initialize is a no-op");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM sessions WHERE id='sess-a'", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

/// v43: outcome / interval CHECK が DDL で閉じていること。
#[test]
fn v43_check_constraints_reject_invalid_rows() {
    let conn = crate::init_memory().expect("init");
    let bad_outcome = conn.execute(
        "INSERT INTO tool_logs (agent_id, tool_name, args_json, outcome)
         VALUES ('ag-a', 't', '{}', 'unknown')",
        [],
    );
    assert!(bad_outcome.is_err(), "outcome 閉集合外を受け入れた");

    for outcome in ["done", "failed", "refused", "deadline", "stopped"] {
        conn.execute(
            "INSERT INTO tool_logs (agent_id, tool_name, args_json, outcome)
             VALUES ('ag-a', 't', '{}', ?1)",
            [outcome],
        )
        .unwrap_or_else(|e| panic!("{outcome} を拒否した: {e}"));
    }

    let bad_interval = conn.execute(
        "INSERT INTO session_watches (session_id, agent_id, interval_secs, filter_json, created_at)
         VALUES ('sess-a', 'ag-a', 0, '{}', '2026-01-01T00:00:00Z')",
        [],
    );
    assert!(bad_interval.is_err(), "interval_secs=0 を受け入れた");

    conn.execute(
        "INSERT INTO session_watches (session_id, agent_id, interval_secs, filter_json, created_at)
         VALUES ('sess-a', 'ag-a', 300, '{}', '2026-01-01T00:00:00Z')",
        [],
    )
    .expect("正の interval を拒否した");
}

/// session_watches は同一 session_id の複数行を許す（UNIQUE 無し）。
#[test]
fn v43_session_watches_allows_multiple_rows_per_session() {
    let conn = crate::init_memory().expect("init");
    conn.execute_batch(
        "INSERT INTO session_watches (session_id, agent_id, interval_secs, filter_json, created_at)
         VALUES
           ('sess-a', 'ag-a', 300, '{}', '2026-01-01T00:00:00Z'),
           ('sess-a', 'ag-a', 60, '{\"kinds\":[1]}', '2026-01-01T00:00:01Z');",
    )
    .expect("同一 session_id の複数行を拒否した");
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_watches WHERE session_id='sess-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2);
}

/// SCHEMA_SQL 経路と v43 マイグレーション経路で新表の sqlite_master SQL が一致する。
#[test]
fn v43_schema_parity_fresh_vs_migrated() {
    let dump = |conn: &Connection| -> Vec<String> {
        conn.prepare(
            "SELECT sql FROM sqlite_master
             WHERE name IN (
                'session_watches', 'idx_session_watches_session',
                'tool_logs', 'idx_tool_logs_agent', 'idx_tool_logs_session'
             )
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, Option<String>>(0))
        .unwrap()
        .map(|r| r.unwrap().unwrap_or_default())
        .collect()
    };

    let fresh = crate::init_memory().expect("fresh");
    let migrated = crate::init_memory().expect("migrated");
    setup_pre_v43(&migrated);
    initialize(&migrated).expect("re-migrate");

    assert_eq!(dump(&fresh), dump(&migrated));
    assert_eq!(schema_version(&fresh).unwrap(), latest_version());
    assert_eq!(schema_version(&migrated).unwrap(), latest_version());
}

fn assert_synthetic_v43_reaches_latest() {
    let conn = crate::init_memory().expect("synthetic v43");
    setup_pre_v43(&conn);
    initialize(&conn).expect("v43 to latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert_v43_schema(&conn);
    assert_v44_schema(&conn);
    assert_v45_schema(&conn);
}

/// 互換test FQN。外部DBは開かずsynthetic fixtureだけを検証する。
#[test]
fn apply_initialize_to_v47_copy_db() {
    assert_synthetic_v43_reaches_latest();
}

/// 旧test FQN互換。検証対象は同じsynthetic v43→latest chain。
#[test]
fn apply_initialize_to_v43_copy_db() {
    assert_synthetic_v43_reaches_latest();
}
