/// v19: Nostr 受信転記先の表が増えるだけで、既存の Nostr 設定
/// （`agent_nostr_config`）の行は 1 つも動かない（#252 段階 A）。冪等でもある。
#[test]
fn skills_created_caller_migration_v24_reaches_existing_db() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(&conn, "skills", "created_caller").unwrap());

    // v23 相当の既存 DB を模す: 列を落とし version を 23 へ戻す。
    // 列を持たない状態で入れた行は、移行後 NULL（legacy grandfather = Owner 相当）に
    // なること（＝既存行が壊れないこと）を見るための実データ。
    conn.execute_batch(
        "ALTER TABLE skills DROP COLUMN created_caller;
             INSERT INTO skills
               (id, agent_id, name, description, situation_pattern, guidance,
                source_type, usage_count, is_active, permission, archived,
                created_at, updated_at)
               VALUES ('legacy1', 'a1', 'n', 'd', 'sp', 'g',
                       'experience', 0, 1, '\"agent\"', 0,
                       '2026-01-01', '2026-01-01');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 23",
    )
    .unwrap();
    assert!(!column_exists(&conn, "skills", "created_caller").unwrap());

    // 起動経路（initialize → run_migrations）で v24 が届く。migrate() は
    // user_version >= BASELINE_VERSION のため走らない（本番と同じ経路）。
    initialize(&conn).expect("upgrade v23 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "skills", "created_caller").unwrap());

    // 既存行は残り、新しい列は NULL（legacy grandfather）。
    let (name, created_caller): (String, Option<String>) = conn
        .query_row(
            "SELECT name, created_caller FROM skills WHERE id = 'legacy1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("既存行が失われた");
    assert_eq!(name, "n");
    assert_eq!(created_caller, None, "既存行は NULL のまま = Owner 相当");

    // #349 の直接の症状: 列を含む SELECT が通ること。
    conn.query_row(
        "SELECT id, agent_id, name, created_caller FROM skills WHERE id = 'legacy1'",
        [],
        |r| r.get::<_, String>(0),
    )
    .expect("created_caller を含む SELECT が通らない");

    // 冪等性: 再実行しても落ちず、書いた値も消えない（column_exists ガード）。
    conn.execute_batch("UPDATE skills SET created_caller = 'agent' WHERE id = 'legacy1'")
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: Option<String> = conn
        .query_row(
            "SELECT created_caller FROM skills WHERE id = 'legacy1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept.as_deref(), Some("agent"));
}

/// v25: 既存 DB（`user_version = 24`, `agent_visible` 列なし）へ番号付き
/// マイグレーションが届き、列が **既定 0（fail-closed）** で追加され、既存行が
/// すべて 0（＝Agent には見せない）になることを検証する（#352 / #349 の事故ガード）。
#[test]
fn skills_agent_visible_migration_v25_reaches_existing_db_default_zero() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(&conn, "skills", "agent_visible").unwrap());

    // v24 相当の既存 DB を模す: agent_visible 列を落とし version を 24 へ戻す。
    // 列を持たない状態で複数行を入れ、移行後すべて 0 になること（既存が壊れず、
    // かつ既定で Agent 非露出になること）を見る。
    conn.execute_batch(
        "ALTER TABLE skills DROP COLUMN agent_visible;
             INSERT INTO skills
               (id, agent_id, name, description, situation_pattern, guidance,
                source_type, usage_count, is_active, permission, archived,
                created_at, updated_at)
               VALUES
               ('s1', 'a1', 'n1', 'd', 'sp', 'g', 'experience', 0, 1, '\"agent\"', 0,
                '2026-01-01', '2026-01-01'),
               ('s2', 'a1', 'n2', 'd', 'sp', 'g', 'experience', 0, 1, '\"agent\"', 0,
                '2026-01-01', '2026-01-01');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 24",
    )
    .unwrap();
    assert!(!column_exists(&conn, "skills", "agent_visible").unwrap());
    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, 2, "移行前の行数");

    // 起動経路（initialize → run_migrations）で v25 が届く。
    initialize(&conn).expect("upgrade v24 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "skills", "agent_visible").unwrap());

    // 既存の全行が残り、agent_visible は既定 0（fail-closed）。
    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, 2, "行が失われた");
    let visible_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM skills WHERE agent_visible = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        visible_count, 0,
        "既存行はすべて非露出（既定 0）でなければならない"
    );

    // 列を含む SELECT が通ること（#349 の症状の非退行）。
    conn.query_row(
        "SELECT id, agent_visible FROM skills WHERE id = 's1'",
        [],
        |r| r.get::<_, i64>(1),
    )
    .expect("agent_visible を含む SELECT が通らない");

    // 冪等性: オーナーが 1 行を露出許可した後に再実行しても落ちず、値も消えない。
    conn.execute_batch("UPDATE skills SET agent_visible = 1 WHERE id = 's1'")
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: i64 = conn
        .query_row(
            "SELECT agent_visible FROM skills WHERE id = 's1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "再実行で露出許可が消えてはならない");
}

/// v27（#361 / #313 段階3）: `agent_memory_index_config.last_organize_at` が
/// **既存 DB に届く**こと、既定 NULL であること、既存行が壊れないこと、再実行で
/// 落ちないこと（#349 の事故ガード）。本番は v26 なので v26 → latest の経路を模す。
#[test]
fn agent_memory_index_config_last_organize_at_migration_v27_reaches_existing_db() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(&conn, "agent_memory_index_config", "last_organize_at").unwrap());

    // v26 相当の既存 DB を模す: last_organize_at 列を落とし version を 26 へ戻す。
    // 既存の設定行（last_skill_consolidation_at には値あり）を入れておき、移行で
    // その値が消えないこと・新列が NULL で足されることを見る。
    conn.execute_batch(
        "ALTER TABLE agent_memory_index_config DROP COLUMN last_organize_at;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_skill_consolidation_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-07-01T00:00:00Z');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 26",
    )
    .unwrap();
    assert!(!column_exists(&conn, "agent_memory_index_config", "last_organize_at").unwrap());

    // 起動経路（initialize → run_migrations）で v27 が届く。
    initialize(&conn).expect("upgrade v26 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "agent_memory_index_config", "last_organize_at").unwrap());

    // 既存行が残り、新列は NULL（未実行）、隣の列の値は消えていない。
    let (organize, consolidation): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT last_organize_at, last_skill_consolidation_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("last_organize_at を含む SELECT が通らない");
    assert_eq!(
        organize, None,
        "新列は既定 NULL（未実行）でなければならない"
    );
    assert_eq!(
        consolidation.as_deref(),
        Some("2026-07-01T00:00:00Z"),
        "隣の列の既存値が失われた"
    );

    // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
    conn.execute_batch(
            "UPDATE agent_memory_index_config SET last_organize_at = '2026-08-03T00:00:00Z' WHERE agent_id = 'a1'",
        )
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: Option<String> = conn
        .query_row(
            "SELECT last_organize_at FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        kept.as_deref(),
        Some("2026-08-03T00:00:00Z"),
        "再実行でマーカーが消えてはならない"
    );
}

/// v28（#365 / #313 段階3b）: `agent_memory_index_config.organize_backlog_cursor` が
/// **既存 DB に届く**こと、既定 NULL であること、既存行・隣の列が壊れないこと、再実行で
/// 落ちないこと（#349 の事故ガード）。本番は v27 なので v27 → latest の経路を模す。
#[test]
fn agent_memory_index_config_backlog_cursor_migration_v28_reaches_existing_db() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(
        &conn,
        "agent_memory_index_config",
        "organize_backlog_cursor"
    )
    .unwrap());

    // v27 相当の既存 DB を模す: organize_backlog_cursor 列を落とし version を 27 へ戻す。
    // 既存の設定行（last_organize_at / last_skill_consolidation_at に値あり）を入れておき、
    // 移行でその値が消えないこと・新列が NULL で足されることを見る。
    conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN organize_backlog_cursor;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_skill_consolidation_at, last_organize_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-07-01T00:00:00Z', '2026-08-03T00:00:00Z|n5');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 27",
        )
        .unwrap();
    assert!(!column_exists(
        &conn,
        "agent_memory_index_config",
        "organize_backlog_cursor"
    )
    .unwrap());

    // 起動経路（initialize → run_migrations）で v28 が届く。
    initialize(&conn).expect("upgrade v27 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(
        &conn,
        "agent_memory_index_config",
        "organize_backlog_cursor"
    )
    .unwrap());

    // 既存行が残り、新列は NULL（未シード）、隣の 2 列の値は消えていない。
    let (backlog, organize, consolidation): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT organize_backlog_cursor, last_organize_at, last_skill_consolidation_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("organize_backlog_cursor を含む SELECT が通らない");
    assert_eq!(
        backlog, None,
        "新列は既定 NULL（未シード）でなければならない"
    );
    assert_eq!(
        organize.as_deref(),
        Some("2026-08-03T00:00:00Z|n5"),
        "新規側マーカーの既存値が失われた"
    );
    assert_eq!(
        consolidation.as_deref(),
        Some("2026-07-01T00:00:00Z"),
        "隣の列の既存値が失われた"
    );

    // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
    conn.execute_batch(
            "UPDATE agent_memory_index_config SET organize_backlog_cursor = '2026-06-01T00:00:00Z|old3' WHERE agent_id = 'a1'",
        )
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: Option<String> = conn
        .query_row(
            "SELECT organize_backlog_cursor FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        kept.as_deref(),
        Some("2026-06-01T00:00:00Z|old3"),
        "再実行でマーカーが消えてはならない"
    );
}

/// v29（#365 レビュー / #313 段階3b）: `agent_memory_index_config.organize_last_run_at` が
/// **既存 DB に届く**こと、既定 NULL であること、既存の 2 軸マーカー・隣の列が壊れないこと、
/// 再実行で落ちないこと。本番は v27 なので v28 相当（列 3 本目だけ欠く）→ latest を模す。
#[test]
fn agent_memory_index_config_last_run_at_migration_v29_reaches_existing_db() {
    let conn = crate::init_memory().expect("init");
    assert!(column_exists(&conn, "agent_memory_index_config", "organize_last_run_at").unwrap());

    // v28 相当の既存 DB を模す: organize_last_run_at 列を落とし version を 28 へ戻す。
    // 2 軸マーカーに値を入れておき、移行で消えないこと・新列が NULL で足されることを見る。
    conn.execute_batch(
            "ALTER TABLE agent_memory_index_config DROP COLUMN organize_last_run_at;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_organize_at, organize_backlog_cursor)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-04T00:00:00Z|n5', '2026-06-01T00:00:00Z|old3');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 28",
        )
        .unwrap();
    assert!(!column_exists(&conn, "agent_memory_index_config", "organize_last_run_at").unwrap());

    // 起動経路（initialize → run_migrations）で v29 が届く。
    initialize(&conn).expect("upgrade v28 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "agent_memory_index_config", "organize_last_run_at").unwrap());

    // 既存の 2 軸マーカーは残り、新列は NULL（未刻）。
    let (last_run, organize, backlog): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT organize_last_run_at, last_organize_at, organize_backlog_cursor
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("organize_last_run_at を含む SELECT が通らない");
    assert_eq!(last_run, None, "新列は既定 NULL（未刻）でなければならない");
    assert_eq!(organize.as_deref(), Some("2026-08-04T00:00:00Z|n5"));
    assert_eq!(backlog.as_deref(), Some("2026-06-01T00:00:00Z|old3"));

    // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
    conn.execute_batch(
            "UPDATE agent_memory_index_config SET organize_last_run_at = '2026-08-05T00:00:00Z' WHERE agent_id = 'a1'",
        )
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: Option<String> = conn
        .query_row(
            "SELECT organize_last_run_at FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept.as_deref(), Some("2026-08-05T00:00:00Z"));
}

/// v31（#384 / #376 段階2）: `agent_memory_index_config.memory_declare_cursor` が
/// **既存 DB に届く**こと、既定 NULL であること、既存行・隣の列（タグ整理ランの 3 列）が
/// 壊れないこと、再実行で落ちないこと（#349 の事故ガード）。本番は v30 なので v30 →
/// latest の経路を模す。
#[test]
fn agent_memory_index_config_declare_cursor_migration_v31_reaches_existing_db() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(&conn, "agent_memory_index_config", "memory_declare_cursor").unwrap());

    // v30 相当の既存 DB を模す: memory_declare_cursor 列を落とし version を 30 へ戻す。
    // タグ整理ランの 3 マーカーに値を入れておき、移行でそれらが消えないこと・新列が
    // NULL で足されることを見る。
    conn.execute_batch(
        "ALTER TABLE agent_memory_index_config DROP COLUMN memory_declare_cursor;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, last_organize_at,
                organize_backlog_cursor, organize_last_run_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-04T00:00:00Z|n5',
                       '2026-06-01T00:00:00Z|old3', '2026-08-05T00:00:00Z');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 30",
    )
    .unwrap();
    assert!(!column_exists(&conn, "agent_memory_index_config", "memory_declare_cursor").unwrap());

    // 起動経路（initialize → run_migrations）で v31 が届く。
    initialize(&conn).expect("upgrade v30 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "agent_memory_index_config", "memory_declare_cursor").unwrap());

    // 新列は NULL（未実行）、タグ整理ランの 3 マーカーは残る。
    let (declare, organize, backlog, last_run): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT memory_declare_cursor, last_organize_at, organize_backlog_cursor,
                        organize_last_run_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("memory_declare_cursor を含む SELECT が通らない");
    assert_eq!(declare, None, "新列は既定 NULL（未実行）でなければならない");
    assert_eq!(organize.as_deref(), Some("2026-08-04T00:00:00Z|n5"));
    assert_eq!(backlog.as_deref(), Some("2026-06-01T00:00:00Z|old3"));
    assert_eq!(last_run.as_deref(), Some("2026-08-05T00:00:00Z"));

    // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
    conn.execute_batch(
            "UPDATE agent_memory_index_config SET memory_declare_cursor = '2026-08-06T00:00:00Z|4242' WHERE agent_id = 'a1'",
        )
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: Option<String> = conn
        .query_row(
            "SELECT memory_declare_cursor FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        kept.as_deref(),
        Some("2026-08-06T00:00:00Z|4242"),
        "再実行でマーカーが消えてはならない"
    );
}

/// v34（#394）: `agent_memory_index_config.memory_declare_window`（本人が決める窓の希望）が
/// **既存 DB に届く**こと、既定 NULL であること、隣の列（宣言ランのマーカー・タグ整理ランの
/// 3 列）が壊れないこと、再実行で値が消えないこと。
#[test]
fn agent_memory_index_config_declare_window_migration_v34_reaches_existing_db() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(&conn, "agent_memory_index_config", "memory_declare_window").unwrap());

    // 列を落とし、宣言ランのマーカーに値を入れた状態で版を戻す（既存 DB を模す）。
    conn.execute_batch(
        "ALTER TABLE agent_memory_index_config DROP COLUMN memory_declare_window;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, memory_declare_cursor,
                organize_last_run_at)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-05T00:00:00Z|23594',
                       '2026-08-05T00:00:00Z');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 32",
    )
    .unwrap();
    assert!(!column_exists(&conn, "agent_memory_index_config", "memory_declare_window").unwrap());

    initialize(&conn).expect("upgrade v32 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "agent_memory_index_config", "memory_declare_window").unwrap());

    let (window, cursor, organize): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT memory_declare_window, memory_declare_cursor, organize_last_run_at
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("memory_declare_window を含む SELECT が通らない");
    assert_eq!(
        window, None,
        "新列は既定 NULL（希望なし）でなければならない"
    );
    assert_eq!(cursor.as_deref(), Some("2026-08-05T00:00:00Z|23594"));
    assert_eq!(organize.as_deref(), Some("2026-08-05T00:00:00Z"));

    // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
    conn.execute_batch(
            "UPDATE agent_memory_index_config SET memory_declare_window = '{\"window_size\":450}' WHERE agent_id = 'a1'",
        )
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: Option<String> = conn
        .query_row(
            "SELECT memory_declare_window FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept.as_deref(), Some("{\"window_size\":450}"));
}

/// v35（#411）: `agent_memory_index_config.memory_condense_cursor`（凝縮ランのマーカー）が
/// **既存 DB に届く**こと、既定 NULL であること、隣の列（宣言ランのマーカー/窓・タグ整理ランの
/// マーカー）が壊れないこと、再実行で値が消えないこと。
#[test]
fn agent_memory_index_config_condense_cursor_migration_v35_reaches_existing_db() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(&conn, "agent_memory_index_config", "memory_condense_cursor").unwrap());

    // 列を落とし、隣の列に値を入れた状態で版を戻す（既存 DB を模す）。
    conn.execute_batch(
        "ALTER TABLE agent_memory_index_config DROP COLUMN memory_condense_cursor;
             INSERT INTO agent_memory_index_config
               (agent_id, batch_size, threshold, updated_at, memory_declare_cursor,
                memory_declare_window)
               VALUES ('a1', 50, 20, '2026-01-01', '2026-08-07T00:00:00Z|60000',
                       '{\"window_size\":300}');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 34",
    )
    .unwrap();
    assert!(!column_exists(&conn, "agent_memory_index_config", "memory_condense_cursor").unwrap());

    initialize(&conn).expect("upgrade v34 -> latest");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "agent_memory_index_config", "memory_condense_cursor").unwrap());

    let (condense, declare, window): (Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT memory_condense_cursor, memory_declare_cursor, memory_declare_window
                 FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("memory_condense_cursor を含む SELECT が通らない");
    assert_eq!(
        condense, None,
        "新列は既定 NULL（未実行）でなければならない"
    );
    assert_eq!(declare.as_deref(), Some("2026-08-07T00:00:00Z|60000"));
    assert_eq!(window.as_deref(), Some("{\"window_size\":300}"));

    // 冪等性: 値を書いた後に再実行しても落ちず、値も消えない。
    conn.execute_batch(
            "UPDATE agent_memory_index_config SET memory_condense_cursor = '2026-08-08T00:00:00Z|346' WHERE agent_id = 'a1'",
        )
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: Option<String> = conn
        .query_row(
            "SELECT memory_condense_cursor FROM agent_memory_index_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept.as_deref(), Some("2026-08-08T00:00:00Z|346"));
}
