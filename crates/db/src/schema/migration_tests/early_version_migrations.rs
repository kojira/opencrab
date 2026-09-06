use super::super::*;
/// E. 実 MIGRATIONS の version は厳密増加・全て baseline より大きい・重複なし。
#[test]
fn agent_sessions_backfill_migration_v4() {
    let conn = crate::init_memory().expect("init");
    // v3 状態に戻し、v4 の backfill 対象となる sessions 行を用意する
    // （うち1件は壊れた JSON — skip され、他の行は影響を受けないこと）。
    conn.execute_batch("PRAGMA user_version = 3").unwrap();
    conn.execute_batch("DELETE FROM agent_sessions").unwrap();
    conn.execute_batch(
            "INSERT INTO sessions (id, mode, theme, phase, turn_number, status, participant_ids_json, done_count, created_at, updated_at)
             VALUES ('s1', 'discord', 't', 'active', 0, 'active', '[\"a1\",\"a2\"]', 0, '2026-01-01', '2026-01-01'),
                    ('s2', 'discord', 't', 'active', 0, 'active', 'not-json', 0, '2026-01-01', '2026-01-01'),
                    ('s3', 'discord', 't', 'active', 0, 'active', '[\"a1\"]', 0, '2026-01-01', '2026-01-01')",
        )
        .unwrap();

    run_migrations(&conn, MIGRATIONS).expect("v4 migration");

    let participants = crate::queries::list_session_participants(&conn, "s1").unwrap();
    assert_eq!(participants, vec!["a1".to_string(), "a2".to_string()]);
    assert!(crate::queries::list_session_participants(&conn, "s2")
        .unwrap()
        .is_empty());
    assert_eq!(
        crate::queries::count_sessions_for_agent(&conn, "a1").unwrap(),
        2
    );
    // 部分一致の誤マッチが無いこと（旧 LIKE 実装は "a" が "a1" にもマッチした）
    assert_eq!(
        crate::queries::count_sessions_for_agent(&conn, "a").unwrap(),
        0
    );
}

#[test]
fn memory_index_fk_cascade_and_check_enforced() {
    let conn = crate::init_memory().expect("init");
    conn.execute_batch(
            "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 't', 's', '2026-01-01', '2026-01-01'),
                    ('c', 'a1', 'r', 'topic', 't', 's', '2026-01-01', '2026-01-01')",
        )
        .unwrap();
    // CHECK: 不正 node_type は拒否
    assert!(conn
            .execute_batch(
                "INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('x', 'a1', 'bogus', 't', 's', '2026-01-01', '2026-01-01')",
            )
            .is_err());
    // FK: 存在しない親は拒否
    assert!(conn
            .execute_batch(
                "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('y', 'a1', 'nope', 'topic', 't', 's', '2026-01-01', '2026-01-01')",
            )
            .is_err());
    // CASCADE: 親削除で子も消える
    conn.execute("DELETE FROM memory_index_nodes WHERE id = 'r'", [])
        .unwrap();
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
}

#[test]
fn memory_index_rebuild_migration_v5_upgrades_legacy_table() {
    let conn = crate::init_memory().expect("init");
    // v4 時点の旧テーブル形（FK/CHECK なし）を再現する
    conn.execute_batch("PRAGMA user_version = 4").unwrap();
    conn.execute_batch(
            "DROP TABLE memory_index_nodes;
             CREATE TABLE memory_index_nodes (
                id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, parent_id TEXT,
                node_type TEXT NOT NULL, source_type TEXT NOT NULL DEFAULT 'session_log',
                title TEXT NOT NULL, summary TEXT NOT NULL,
                start_log_id INTEGER, end_log_id INTEGER, source_session_id TEXT,
                date_from TEXT, date_to TEXT,
                depth INTEGER NOT NULL DEFAULT 0, child_count INTEGER NOT NULL DEFAULT 0,
                token_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 't', 's', '2026-01-01', '2026-01-01'),
                    ('c', 'a1', 'r', 'topic', 't', 's', '2026-01-01', '2026-01-01'),
                    ('orphan', 'a1', 'ghost', 'topic', 't', 's', '2026-01-01', '2026-01-01'),
                    ('junk', 'a1', NULL, 'bogus_type', 't', 's', '2026-01-01', '2026-01-01')",
        )
        .unwrap();

    run_migrations(&conn, MIGRATIONS).expect("v5 migration");

    // FK が付与され、正常行は保存、孤児は parent NULL 化、不正 node_type は除外
    let fk_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_list('memory_index_nodes')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fk_count, 1);
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM memory_index_nodes ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        ids,
        vec!["c".to_string(), "orphan".to_string(), "r".to_string()]
    );
    let orphan_parent: Option<String> = conn
        .query_row(
            "SELECT parent_id FROM memory_index_nodes WHERE id = 'orphan'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan_parent, None);
}

#[test]
fn task_ledger_restart_count_migration_v6() {
    // 新規DB: init 直後から列がある（v6 適用済み）。
    let conn = crate::init_memory().expect("init");
    assert!(column_exists(&conn, "task_ledger", "restart_count").unwrap());

    // 既存DB（v5 時点 = 列なし）からの upgrade。
    conn.execute_batch("DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1")
        .unwrap();
    conn.execute_batch(TASK_LEDGER_SQL).unwrap();
    conn.execute_batch("PRAGMA user_version = 5").unwrap();
    conn.execute_batch(
        "INSERT INTO task_ledger (agent_id, session_id, goal, status, created_at, updated_at)
             VALUES ('a1', 's1', 'g', 'active', '2026-01-01', '2026-01-01')",
    )
    .unwrap();
    assert!(!column_exists(&conn, "task_ledger", "restart_count").unwrap());

    run_migrations(&conn, MIGRATIONS).expect("v6 migration");
    assert!(column_exists(&conn, "task_ledger", "restart_count").unwrap());
    // 既存行は DEFAULT 0 で読める
    let count: i64 = conn
        .query_row(
            "SELECT restart_count FROM task_ledger WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

#[test]
fn memory_index_keywords_migration_v7() {
    // 新規DB: init 直後から列と FTS 表がある。
    let conn = crate::init_memory().expect("init");
    assert!(column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());
    assert!(column_exists(&conn, "memory_index_nodes", "summary_refreshed_at").unwrap());
    assert!(table_exists(&conn, "memory_index_fts").unwrap());

    // 既存DB（v6 時点 = 列も FTS も無い）からの upgrade。
    conn.execute_batch(
            "DROP TABLE memory_index_fts;
             DROP TABLE memory_index_nodes;
             CREATE TABLE memory_index_nodes (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                parent_id TEXT REFERENCES memory_index_nodes(id) ON DELETE CASCADE,
                node_type TEXT NOT NULL CHECK (node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')),
                source_type TEXT NOT NULL DEFAULT 'session_log',
                title TEXT NOT NULL, summary TEXT NOT NULL,
                start_log_id INTEGER, end_log_id INTEGER, source_session_id TEXT,
                date_from TEXT, date_to TEXT,
                depth INTEGER NOT NULL DEFAULT 0, child_count INTEGER NOT NULL DEFAULT 0,
                token_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('t-legacy', 'a1', 'topic', '旧トピック', '旧要約テキスト', '2026-01-01', '2026-01-01');
             PRAGMA user_version = 6;",
        )
        .unwrap();
    assert!(!column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());

    run_migrations(&conn, MIGRATIONS).expect("v7 migration");
    assert!(column_exists(&conn, "memory_index_nodes", "keywords_json").unwrap());
    assert!(column_exists(&conn, "memory_index_nodes", "summary_refreshed_at").unwrap());
    // 既存行が FTS にバックフィルされ、trigram で部分一致検索できる
    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_rows, 1);
    let hits = crate::queries::search_index_nodes(&conn, "a1", "旧要約", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t-legacy");
    // keywords_json は DEFAULT '[]' で読める
    assert_eq!(hits[0].keywords_json, "[]");
    // 再実行してもバックフィルは重複しない
    conn.execute_batch("PRAGMA user_version = 6").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v7 rerun");
    let fts_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fts_rows, 1);
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

#[test]
fn migrations_versions_are_strictly_increasing() {
    let mut prev = BASELINE_VERSION;
    for m in MIGRATIONS {
        assert!(
            m.version > prev,
            "migration versions must be strictly increasing and > baseline"
        );
        prev = m.version;
    }
}
