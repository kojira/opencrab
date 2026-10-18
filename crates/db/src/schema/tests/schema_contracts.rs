/// H. SCHEMA_SQL 側と TASK_LEDGER_SQL 側で生成されるテーブル定義が一致する
/// （両所への二重記載がドリフトしていないことの検証）。
#[test]
fn task_ledger_schema_parity() {
    let dump = |conn: &Connection| -> Vec<String> {
        conn.prepare(
            "SELECT sql FROM sqlite_master
                 WHERE name IN ('task_ledger','task_progress',
                                'idx_task_ledger_session','idx_task_ledger_one_active',
                                'idx_task_progress_task')
                 ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };

    // 新規DB: SCHEMA_SQL 由来（baseline 時点でテーブルが出来ており、v2 は no-op）。
    let fresh = crate::init_memory().expect("fresh");
    // 既存DB: baseline 後に v2 マイグレーション由来で作成。
    let migrated = crate::init_memory().expect("migrated");
    migrated
        .execute_batch("DROP TABLE task_progress; DROP TABLE task_ledger; DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
        PRAGMA user_version = 1")
        .unwrap();
    initialize(&migrated).expect("re-migrate");

    assert_eq!(dump(&fresh), dump(&migrated));
    assert_eq!(dump(&fresh).len(), 5, "expected 2 tables + 3 indexes");
}

/// D-1080 v60: schema 59 upgrades to a nullable `sessions.model` (NULL = inherit) without
/// touching agents, sessions or bindings; a fresh install gets the identical column.
#[test]
fn v60_adds_nullable_session_model_preserving_rows() {
    let column = |conn: &Connection| -> Option<(String, i64, Option<String>)> {
        conn.query_row(
            "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('sessions') WHERE name='model'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    };
    let fresh = crate::init_memory().expect("fresh");
    assert_eq!(schema_version(&fresh).unwrap(), latest_version());
    assert_eq!(column(&fresh), Some(("TEXT".to_string(), 0, None)));

    let old = crate::init_memory().expect("v59 fixture");
    if column(&old).is_some() {
        old.execute_batch("ALTER TABLE sessions DROP COLUMN model").unwrap();
    }
    old.execute_batch(
        "PRAGMA user_version = 59;
         INSERT INTO agents (agent_id, name, persona_name, model, created_at, updated_at)
           VALUES ('a1', 'A', 'p', 'prov:agent-m', 'now', 'now');
         INSERT INTO sessions (id, theme, created_at, updated_at) VALUES ('s1', 'addr', 'now', 'now');
         INSERT INTO agent_sessions (agent_id, session_id) VALUES ('a1', 's1');",
    )
    .unwrap();
    initialize(&old).expect("upgrade 59 -> latest");
    assert_eq!(schema_version(&old).unwrap(), latest_version());
    assert_eq!(column(&old), column(&fresh));
    let (theme, model): (String, Option<String>) = old
        .query_row("SELECT theme, model FROM sessions WHERE id='s1'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((theme.as_str(), model), ("addr", None));
    let agent_model: Option<String> = old
        .query_row("SELECT model FROM agents WHERE agent_id='a1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(agent_model.as_deref(), Some("prov:agent-m"));
    let members: i64 = old
        .query_row("SELECT COUNT(*) FROM agent_sessions WHERE session_id='s1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(members, 1);
}

/// D-1081 v61: schema 60 upgrades to a nullable `sessions.realtime_model` (NULL = follow the
/// session's model) without touching existing session rows; a fresh install matches.
#[test]
fn v61_adds_nullable_session_realtime_model_preserving_rows() {
    let column = |conn: &Connection| -> Option<(String, i64, Option<String>)> {
        conn.query_row(
            "SELECT type, \"notnull\", dflt_value FROM pragma_table_info('sessions') WHERE name='realtime_model'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok()
    };
    let fresh = crate::init_memory().expect("fresh");
    assert_eq!(schema_version(&fresh).unwrap(), latest_version());
    assert_eq!(column(&fresh), Some(("TEXT".to_string(), 0, None)));

    let old = crate::init_memory().expect("v60 fixture");
    if column(&old).is_some() {
        old.execute_batch("ALTER TABLE sessions DROP COLUMN realtime_model")
            .unwrap();
    }
    old.execute_batch(
        "PRAGMA user_version = 60;
         INSERT INTO sessions (id, theme, created_at, updated_at, model)
           VALUES ('s1', 'addr', 'now', 'now', 'prov:session-m');",
    )
    .unwrap();
    initialize(&old).expect("upgrade 60 -> latest");
    assert_eq!(schema_version(&old).unwrap(), latest_version());
    assert_eq!(column(&old), column(&fresh));
    let (model, realtime): (Option<String>, Option<String>) = old
        .query_row(
            "SELECT model, realtime_model FROM sessions WHERE id='s1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((model.as_deref(), realtime), (Some("prov:session-m"), None));
}

/// D-1086 v62: schema 61 upgrades to nullable `sessions.reasoning_effort` and
/// `sessions.realtime_reasoning_effort` (NULL = no override) without touching existing rows; a
/// fresh install matches and re-running the migration is a no-op.
#[test]
fn v62_adds_nullable_session_reasoning_effort_slots_preserving_rows() {
    let columns = |conn: &Connection| -> Vec<(String, String, i64, Option<String>)> {
        conn.prepare(
            "SELECT name, type, \"notnull\", dflt_value FROM pragma_table_info('sessions')
             WHERE name IN ('reasoning_effort', 'realtime_reasoning_effort') ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };
    let fresh = crate::init_memory().expect("fresh");
    assert_eq!(latest_version(), 62);
    assert_eq!(schema_version(&fresh).unwrap(), latest_version());
    assert_eq!(
        columns(&fresh),
        vec![
            ("realtime_reasoning_effort".to_string(), "TEXT".to_string(), 0, None),
            ("reasoning_effort".to_string(), "TEXT".to_string(), 0, None),
        ]
    );

    let old = crate::init_memory().expect("v61 fixture");
    old.execute_batch(
        "ALTER TABLE sessions DROP COLUMN reasoning_effort;
         ALTER TABLE sessions DROP COLUMN realtime_reasoning_effort;
         PRAGMA user_version = 61;
         INSERT INTO sessions (id, theme, created_at, updated_at, model, realtime_model)
           VALUES ('s1', 'addr', 'now', 'now', 'prov:session-m', 'prov:light');",
    )
    .unwrap();
    initialize(&old).expect("upgrade 61 -> latest");
    assert_eq!(schema_version(&old).unwrap(), latest_version());
    assert_eq!(columns(&old), columns(&fresh));
    let row: (Option<String>, Option<String>, Option<String>, Option<String>) = old
        .query_row(
            "SELECT model, realtime_model, reasoning_effort, realtime_reasoning_effort
             FROM sessions WHERE id='s1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (Some("prov:session-m".to_string()), Some("prov:light".to_string()), None, None)
    );

    old.execute_batch("PRAGMA user_version = 61;").unwrap();
    initialize(&old).expect("re-run 61 -> latest is idempotent");
    assert_eq!(columns(&old), columns(&fresh));
}
