#[test]
fn v37_creates_generic_schedule_tables_idempotently() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_SQL).unwrap();
    conn.execute_batch(
        "DROP TABLE session_heartbeat_config;
         DROP TABLE agent_schedules;
         PRAGMA user_version = 36;",
    )
    .unwrap();

    migrate_v37_session_heartbeat(&conn).unwrap();
    assert!(table_exists(&conn, "session_heartbeat_config").unwrap());
    assert!(table_exists(&conn, "agent_schedules").unwrap());
    migrate_v37_session_heartbeat(&conn).unwrap();
}

#[test]
fn v38_preserves_last_run_value_while_aligning_schedule_columns() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_SQL).unwrap();
    conn.execute_batch(
        "DROP TABLE agent_schedules;
         CREATE TABLE agent_schedules (
             id TEXT PRIMARY KEY,
             agent_id TEXT NOT NULL,
             name TEXT NOT NULL,
             cron_expr TEXT NOT NULL,
             timezone TEXT NOT NULL DEFAULT 'UTC',
             enabled INTEGER NOT NULL DEFAULT 0,
             task_text TEXT NOT NULL,
             next_run_at TEXT,
             last_run_at TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         INSERT INTO agent_schedules
           (id, agent_id, name, cron_expr, enabled, task_text, last_run_at, created_at, updated_at)
         VALUES
           ('s1', 'a1', 'daily', '0 0 * * *', 1, 'task', '2026-08-09T07:00:00+09:00', 'c', 'u');
         PRAGMA user_version = 37;",
    )
    .unwrap();

    migrate_v38_align_schedule_vocab(&conn).unwrap();

    assert!(!column_exists(&conn, "agent_schedules", "last_run_at").unwrap());
    assert!(column_exists(&conn, "agent_schedules", "last_fired_at").unwrap());
    assert!(!column_exists(&conn, "agent_schedules", "next_run_at").unwrap());
    let preserved: String = conn
        .query_row(
            "SELECT last_fired_at FROM agent_schedules WHERE agent_id='a1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(preserved, "2026-08-09T07:00:00+09:00");
}
