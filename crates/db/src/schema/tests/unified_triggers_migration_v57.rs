// #612 RED-5: v57 folds enabled session heartbeats into agent_schedules and drops the old tables.

const V57_OLD_HEARTBEAT_TABLES: [&str; 4] = [
    "session_heartbeat_instructions",
    "session_heartbeat_config",
    "agent_heartbeat_config",
    "heartbeat_instructions_audit",
];

/// Rebuilds the v56 heartbeat storage shape on top of a fresh database and stamps v56.
fn v57_seed_v56_heartbeat_fixture(conn: &Connection) {
    if !column_exists(conn, "agents", "heartbeat_instructions").unwrap() {
        conn.execute_batch(
            "ALTER TABLE agents ADD COLUMN heartbeat_instructions TEXT NOT NULL DEFAULT ''",
        )
        .unwrap();
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_heartbeat_config (
             agent_id TEXT PRIMARY KEY,
             enabled INTEGER NOT NULL DEFAULT 0,
             interval_secs INTEGER,
             updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS heartbeat_instructions_audit (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             agent_id TEXT NOT NULL,
             scope TEXT NOT NULL,
             channel_id TEXT,
             session_id TEXT,
             caller_identity TEXT NOT NULL,
             caller_user_id TEXT,
             old_value TEXT,
             new_value TEXT,
             reason TEXT,
             created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS session_heartbeat_config (
             agent_id      TEXT NOT NULL,
             session_id    TEXT NOT NULL,
             enabled       INTEGER NOT NULL DEFAULT 0,
             interval_secs INTEGER,
             anchor_at     TEXT,
             last_fired_at TEXT,
             updated_at    TEXT NOT NULL,
             PRIMARY KEY (agent_id, session_id)
         );
         CREATE TABLE IF NOT EXISTS session_heartbeat_instructions (
             agent_id      TEXT NOT NULL,
             session_id    TEXT NOT NULL,
             override_text TEXT,
             updated_at    TEXT NOT NULL,
             PRIMARY KEY (agent_id, session_id),
             FOREIGN KEY (agent_id, session_id)
                 REFERENCES session_heartbeat_config(agent_id, session_id) ON DELETE CASCADE,
             FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
         );
         INSERT INTO agents (agent_id, name, persona_name, heartbeat_instructions)
         VALUES ('agent-a', 'A', 'persona', 'agent text' || char(7) || char(10) || 'line two'),
                ('agent-b', 'B', 'persona', '');
         INSERT INTO sessions (id, theme, created_at, updated_at)
         VALUES ('session-override', 't', 'x', 'x'),
                ('session-agent', 't', 'x', 'x'),
                ('session-default', 't', 'x', 'x'),
                ('session-disabled', 't', 'x', 'x');
         INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at)
         VALUES ('agent-a', 1, 1800, 'x'), ('agent-b', 0, NULL, 'x');
         INSERT INTO heartbeat_instructions_audit
             (agent_id, scope, caller_identity, new_value, created_at)
         VALUES ('agent-a', 'agent', 'owner', 'agent text', 'x');
         INSERT INTO session_heartbeat_config
             (agent_id, session_id, enabled, interval_secs, anchor_at, last_fired_at, updated_at)
         VALUES
             ('agent-a', 'session-override', 1, 18000,
              '2026-01-01T00:00:00+00:00', '2026-01-02T03:00:00+00:00', 'x'),
             ('agent-a', 'session-agent', 1, 10800,
              '2026-01-01T00:00:00+00:00', NULL, 'x'),
             ('agent-b', 'session-default', 1, 300,
              '2026-01-03T00:00:00+00:00', '2026-01-03T01:00:00+00:00', 'x'),
             ('agent-a', 'session-disabled', 0, 600,
              '2026-01-01T00:00:00+00:00', NULL, 'x');
         INSERT INTO session_heartbeat_instructions (agent_id, session_id, override_text, updated_at)
         VALUES ('agent-a', 'session-override', 'session override text', 'x'),
                ('agent-b', 'session-default', NULL, 'x');
         PRAGMA user_version = 56;",
    )
    .unwrap();
}

type V57ScheduleRow = (String, String, String, String, String, bool, Option<String>, Option<String>);

fn v57_schedule_rows(conn: &Connection) -> Vec<V57ScheduleRow> {
    conn.prepare(
        "SELECT agent_id, session_id, cron_expr, timezone, message, enabled, anchor_at, last_fired_at
         FROM agent_schedules ORDER BY session_id",
    )
    .unwrap()
    .query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
        ))
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn v57_rfc3339(value: &Option<String>) -> Option<chrono::DateTime<chrono::Utc>> {
    value.as_deref().map(|text| {
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&chrono::Utc)
    })
}

#[test]
fn v57_moves_enabled_session_heartbeats_into_agent_schedules_and_drops_old_storage() {
    let conn = crate::init_memory().unwrap();
    conn.execute_batch("DELETE FROM agent_schedules").unwrap();
    v57_seed_v56_heartbeat_fixture(&conn);

    // Old next fire (v56 heartbeat): last_fired.or(anchor) + interval.
    let before: Vec<(String, chrono::DateTime<chrono::Utc>)> = conn
        .prepare(
            "SELECT session_id, interval_secs, anchor_at, last_fired_at
             FROM session_heartbeat_config WHERE enabled = 1 ORDER BY session_id",
        )
        .unwrap()
        .query_map([], |row| {
            let session_id: String = row.get(0)?;
            let interval: i64 = row.get(1)?;
            let anchor: Option<String> = row.get(2)?;
            let last: Option<String> = row.get(3)?;
            let base = v57_rfc3339(&last).or(v57_rfc3339(&anchor)).unwrap();
            Ok((session_id, base + chrono::Duration::seconds(interval)))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    initialize(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 58);

    let rows = v57_schedule_rows(&conn);
    assert_eq!(
        rows,
        vec![
            (
                "agent-a".into(),
                "session-agent".into(),
                "@every 10800s".into(),
                "Asia/Tokyo".into(),
                "agent text\nline two".into(),
                true,
                Some("2026-01-01T00:00:00+00:00".into()),
                None,
            ),
            (
                "agent-b".into(),
                "session-default".into(),
                "@every 300s".into(),
                "Asia/Tokyo".into(),
                "今この瞬間、自律的に何をするか判断してください。発言は30分に1回以下が望ましい。"
                    .into(),
                true,
                Some("2026-01-03T00:00:00+00:00".into()),
                Some("2026-01-03T01:00:00+00:00".into()),
            ),
            (
                "agent-a".into(),
                "session-override".into(),
                "@every 18000s".into(),
                "Asia/Tokyo".into(),
                "session override text".into(),
                true,
                Some("2026-01-01T00:00:00+00:00".into()),
                Some("2026-01-02T03:00:00+00:00".into()),
            ),
        ],
        "only enabled rows move; disabled rows are dropped (D3)"
    );

    // New next fire (#612 I1): max(last_fired, anchor) + @every seconds. Must equal the old value.
    let after: Vec<(String, chrono::DateTime<chrono::Utc>)> = rows
        .iter()
        .map(|row| {
            let secs: i64 = row
                .2
                .strip_prefix("@every ")
                .and_then(|value| value.strip_suffix('s'))
                .unwrap()
                .parse()
                .unwrap();
            let base = match (v57_rfc3339(&row.7), v57_rfc3339(&row.6)) {
                (Some(last), Some(anchor)) => last.max(anchor),
                (last, anchor) => last.or(anchor).unwrap(),
            };
            (row.1.clone(), base + chrono::Duration::seconds(secs))
        })
        .collect();
    assert_eq!(after, before, "next_fire_at is identical across v57");

    for table in V57_OLD_HEARTBEAT_TABLES {
        assert!(!table_exists(&conn, table).unwrap(), "{table} must be dropped");
    }
    assert!(!column_exists(&conn, "agents", "heartbeat_instructions").unwrap());
}

#[test]
fn v57_rejects_enabled_heartbeat_without_interval_or_below_floor() {
    for (interval, label) in [("NULL", "null interval"), ("299", "below 300s floor")] {
        let conn = crate::init_memory().unwrap();
        v57_seed_v56_heartbeat_fixture(&conn);
        conn.execute_batch(&format!(
            "UPDATE session_heartbeat_config SET interval_secs = {interval}
             WHERE session_id = 'session-agent'"
        ))
        .unwrap();
        assert!(initialize(&conn).is_err(), "{label} must fail v57");
        assert_eq!(schema_version(&conn).unwrap(), 56, "{label}: stays at v56");
        for table in V57_OLD_HEARTBEAT_TABLES {
            assert!(table_exists(&conn, table).unwrap(), "{label}: {table} kept");
        }
        assert!(column_exists(&conn, "agents", "heartbeat_instructions").unwrap());
    }
}

#[test]
fn v57_fresh_schema_has_no_heartbeat_storage() {
    let fresh = crate::init_memory().unwrap();
    assert_eq!(schema_version(&fresh).unwrap(), 58);
    for table in V57_OLD_HEARTBEAT_TABLES {
        assert!(!table_exists(&fresh, table).unwrap(), "{table} must be absent");
    }
    assert!(!column_exists(&fresh, "agents", "heartbeat_instructions").unwrap());
}

#[test]
fn v58_adds_cache_prices_to_model_pricing() {
    let fresh = crate::init_memory().unwrap();
    assert!(column_exists(&fresh, "model_pricing", "cached_input_price_per_1m").unwrap());
    assert!(column_exists(&fresh, "model_pricing", "cache_write_price_per_1m").unwrap());
}
