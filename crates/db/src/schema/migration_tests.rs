use super::*;
use rusqlite::Connection;

/// A. バージョン管理導入前の旧DBを模して、baseline が再適用され version 1 に
/// スタンプされることを検証する。
#[test]
fn baseline_reconciles_pre_versioning_db() {
    let conn = crate::init_memory().expect("init");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 旧DBを模す: version を 0 に戻し、baseline が再追加する列を落とす。
    conn.execute_batch("PRAGMA user_version = 0").unwrap();
    conn.execute_batch("ALTER TABLE skills DROP COLUMN archived")
        .unwrap();
    assert!(!column_exists(&conn, "skills", "archived").unwrap());

    // 再初期化で baseline + 番号付きマイグレーションが走り、列が復活し最新版にスタンプされる。
    initialize(&conn).expect("re-initialize");
    assert!(column_exists(&conn, "skills", "archived").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

/// 版管理導入前（`user_version = 0`）の**旧 shape の表を実際に持つ** DB を、
/// 生成コードで作って現行 `initialize` に通す回帰スイート（#475 / #476）。
///
/// test A（`baseline_reconciles_pre_versioning_db`）は最新スキーマ（= 全列あり）から
/// 列を1つ落とすだけなので、「SCHEMA_SQL の index が参照する列が旧表に無い」欠陥
/// （#475: `idx_memory_index_nodes_short_id`）を素通りしてしまう。ここでは**その世代の
/// 表定義そのもの**を与えて再現する。**新しい世代の地雷が出たら
/// [`old_db_generations`] に (名前, 旧表 DDL, 検証クロージャ) を1件足すだけで守れる。**
fn old_db_generations() -> Vec<OldDbGeneration> {
    vec![
            // 2026-04-04 (8afaabe) 以前: `memory_index_nodes` に `short_id` 列が無い。
            // 現行 SCHEMA_SQL は `CREATE TABLE IF NOT EXISTS` でこの旧表を skip し、以前は
            // その直後の short_id partial index で `no such column: short_id` を投げて起動不能
            // だった（#475 / #476）。修正後は index を migrate() 側（列確定後）で張るので通る。
            OldDbGeneration {
                name: "pre_short_id_2026_04",
                // 8afaabe~1 の memory_index_nodes をそのまま（short_id のみ欠く）。
                schema: "
                    CREATE TABLE IF NOT EXISTS memory_index_nodes (
                        id TEXT PRIMARY KEY,
                        agent_id TEXT NOT NULL,
                        parent_id TEXT,
                        node_type TEXT NOT NULL,
                        source_type TEXT NOT NULL DEFAULT 'session_log',
                        title TEXT NOT NULL,
                        summary TEXT NOT NULL,
                        start_log_id INTEGER,
                        end_log_id INTEGER,
                        source_session_id TEXT,
                        date_from TEXT,
                        date_to TEXT,
                        depth INTEGER NOT NULL DEFAULT 0,
                        child_count INTEGER NOT NULL DEFAULT 0,
                        token_count INTEGER NOT NULL DEFAULT 0,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    );
                    INSERT INTO memory_index_nodes
                        (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
                    VALUES ('n1', 'a1', NULL, 'root', 't', 's', '2026-03-01', '2026-03-01');
                ",
                verify: |conn| {
                    // 旧 shape に short_id 列が足され、既存行が backfill される。
                    assert!(
                        column_exists(conn, "memory_index_nodes", "short_id").unwrap(),
                        "short_id 列が足されていること"
                    );
                    let sid: Option<String> = conn
                        .query_row(
                            "SELECT short_id FROM memory_index_nodes WHERE id = 'n1'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert!(sid.is_some(), "既存行が backfill されていること");
                },
            },
            // 2026-02 (b6a145e) 初期世代: `soul` が `personality_json`（JSON）を持ち
            // `personality` / `instructions` 列を持たない。現行 `migrate_soul_identity_to_agents`
            // は `SELECT ... s.personality ... FROM soul` で集約するため、修正前は
            // `no such column: s.personality` を投げて起動不能だった（#480）。修正後は
            // migrate() が集約前に `personality` 列を用意して塞ぐ。
            OldDbGeneration {
                name: "pre_personality_2026_02",
                // b6a145e の soul / identity をそのまま（soul は personality を欠く）。
                schema: "
                    CREATE TABLE IF NOT EXISTS soul (
                        agent_id TEXT PRIMARY KEY,
                        persona_name TEXT NOT NULL,
                        social_style_json TEXT NOT NULL DEFAULT '{}',
                        personality_json TEXT NOT NULL DEFAULT '{}',
                        thinking_style_json TEXT NOT NULL DEFAULT '{}',
                        custom_traits_json TEXT,
                        updated_at TEXT NOT NULL
                    );
                    CREATE TABLE IF NOT EXISTS identity (
                        agent_id TEXT PRIMARY KEY,
                        name TEXT NOT NULL,
                        role TEXT NOT NULL DEFAULT 'discussant',
                        job_title TEXT,
                        organization TEXT,
                        image_url TEXT,
                        metadata_json TEXT,
                        updated_at TEXT NOT NULL
                    );
                    -- a1: soul + identity（集約 INSERT1 経路）。identity は既存 metadata を持つ。
                    -- soul の JSON 列（Big Five / 自由記述 description / 任意 custom_traits）を全て埋める。
                    INSERT INTO soul (agent_id, persona_name, social_style_json, personality_json, thinking_style_json, custom_traits_json, updated_at)
                    VALUES ('a1', 'Shelly',
                            '{\"formal\":0.7}',
                            '{\"openness\":0.5}',
                            '{\"description\":\"deep and careful\"}',
                            '{\"favorite_color\":\"teal\"}',
                            '2026-02-20');
                    INSERT INTO identity (agent_id, name, job_title, metadata_json, updated_at)
                    VALUES ('a1', 'Shelly', 'engineer', '{\"kept\":\"yes\"}', '2026-02-20');
                    -- a2: soul のみ（identity 無し = 集約 INSERT2 経路）。metadata は NULL から退避される。
                    INSERT INTO soul (agent_id, persona_name, custom_traits_json, updated_at)
                    VALUES ('a2', 'Solo', '{\"note\":\"user wrote this\"}', '2026-02-20');
                    -- a3: custom_traits_json が不正 JSON。起動を止めず生文字列で退避する（warn 経路）。
                    INSERT INTO soul (agent_id, persona_name, custom_traits_json, updated_at)
                    VALUES ('a3', 'Broken', 'not-json{oops', '2026-02-20');
                ",
                verify: |conn| {
                    // soul / identity は集約後に DROP される。
                    assert!(
                        !table_exists(conn, "soul").unwrap(),
                        "soul は集約後に DROP されていること"
                    );
                    assert!(
                        !table_exists(conn, "identity").unwrap(),
                        "identity は集約後に DROP されていること"
                    );
                    // agents に統合され、name は identity 由来・personality は列欠落のため NULL。
                    let (name, job, personality): (String, Option<String>, Option<String>) = conn
                        .query_row(
                            "SELECT name, job_title, personality FROM agents WHERE agent_id = 'a1'",
                            [],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .expect("agents に a1 が集約されていること");
                    assert_eq!(name, "Shelly");
                    assert_eq!(job.as_deref(), Some("engineer"));
                    assert!(
                        personality.is_none(),
                        "personality 列欠落世代は NULL で集約されること（personality_json は移送しない）"
                    );

                    // #480 退避: soul の JSON 列は agents.metadata_json.legacy_soul へ保全される。
                    // a1（INSERT1 経路）: identity 既存 metadata（kept）を壊さず legacy_soul を足す。
                    let extract = |agent: &str, path: &str| -> Option<String> {
                        conn.query_row(
                            "SELECT json_extract(metadata_json, ?2) FROM agents WHERE agent_id = ?1",
                            rusqlite::params![agent, path],
                            |r| r.get::<_, Option<String>>(0),
                        )
                        .unwrap()
                    };
                    assert_eq!(
                        extract("a1", "$.kept").as_deref(),
                        Some("yes"),
                        "identity 由来の既存 metadata は退避で壊れないこと"
                    );
                    assert_eq!(
                        extract("a1", "$.legacy_soul.thinking_style_json.description")
                            .as_deref(),
                        Some("deep and careful"),
                        "thinking_style_json の自由記述 description が退避されること"
                    );
                    assert_eq!(
                        extract("a1", "$.legacy_soul.custom_traits_json.favorite_color")
                            .as_deref(),
                        Some("teal"),
                        "custom_traits_json（任意 JSON）が退避されること"
                    );
                    // 数値は json_extract が REAL を返すため、オブジェクトごと TEXT で取り出して検証。
                    assert_eq!(
                        extract("a1", "$.legacy_soul.personality_json").as_deref(),
                        Some("{\"openness\":0.5}"),
                        "personality_json（Big Five）も退避されること"
                    );
                    // a2（INSERT2 経路）: metadata が NULL からでも legacy_soul を退避できること。
                    assert_eq!(
                        extract("a2", "$.legacy_soul.custom_traits_json.note")
                            .as_deref(),
                        Some("user wrote this"),
                        "identity 無し経路（metadata=NULL）でも退避されること"
                    );
                    // a3: 不正 JSON でも起動を止めず、生文字列として退避されること（warn 経路）。
                    assert_eq!(
                        extract("a3", "$.legacy_soul.custom_traits_json").as_deref(),
                        Some("not-json{oops"),
                        "不正 JSON は生文字列で退避されること"
                    );
                },
            },
            // 2026-（v17・#159）改名前世代: 表は旧名 `trusted_discord_users`（列 `discord_user_id`）。
            // 版管理導入前（user_version=0）で作られたため、initialize の baseline 経路は
            // 先に SCHEMA_SQL を流し **空の `trusted_users` を作る**。その後 run_migrations が
            // v17 に達しても、ガード `table_exists("trusted_discord_users") && !table_exists("trusted_users")`
            // が「新表が既に存在する」で false になり **RENAME が skip** され、旧表に信頼済み
            // ユーザーのデータが取り残される（クラッシュしないので気づけない・#479）。
            // 修正後は v17 が「新表が空なら空表を DROP してから RENAME」でデータを移す。
            OldDbGeneration {
                name: "pre_trusted_rename_2026",
                // v16 相当の旧 shape（display_name / platform は既に持つ）を user_version=0 で。
                // v3 / v16 のガードはこれらの列が既にあるので no-op になり、v17 の改名だけが要点。
                schema: "
                    CREATE TABLE IF NOT EXISTS trusted_discord_users (
                        id TEXT PRIMARY KEY,
                        discord_user_id TEXT NOT NULL,
                        agent_id TEXT NOT NULL,
                        permission TEXT NOT NULL DEFAULT 'user',
                        created_by TEXT NOT NULL DEFAULT 'owner',
                        created_at TEXT NOT NULL,
                        display_name TEXT NOT NULL DEFAULT '',
                        platform TEXT NOT NULL DEFAULT 'discord',
                        UNIQUE (discord_user_id, agent_id)
                    );
                    CREATE INDEX IF NOT EXISTS idx_trusted_discord_users_agent ON trusted_discord_users(agent_id);
                    INSERT INTO trusted_discord_users
                        (id, discord_user_id, agent_id, permission, created_by, created_at, display_name, platform)
                        VALUES ('old-1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B', 'discord'),
                               ('old-2', '43', 'a1', 'user', 'owner', '2026-01-02', '', 'discord');
                ",
                verify: |conn| {
                    // 改名が成立し、旧名は消えている。
                    assert!(
                        table_exists(conn, "trusted_users").unwrap(),
                        "trusted_users へ改名されていること"
                    );
                    assert!(
                        !table_exists(conn, "trusted_discord_users").unwrap(),
                        "旧 trusted_discord_users は残っていないこと（データが取り残されない）"
                    );
                    // #479 の本丸: 旧表のデータが新表へ移っていること（空の新表に上書きされない）。
                    let n: i64 = conn
                        .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
                        .unwrap();
                    assert_eq!(n, 2, "旧表の 2 行が trusted_users に移っていること");
                    // 列も改名され、値はそのまま。permission は後続 v18 で co_agent→co-agent に統一。
                    let (user_id, permission, display_name, platform): (String, String, String, String) = conn
                        .query_row(
                            "SELECT user_id, permission, display_name, platform FROM trusted_users WHERE id = 'old-1'",
                            [],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                        )
                        .unwrap();
                    assert_eq!(user_id, "42", "discord_user_id の値が user_id に移っていること");
                    assert_eq!(permission, "co-agent");
                    assert_eq!(display_name, "Crab B");
                    assert_eq!(platform, "discord");
                    // 索引は新名で張り直され、旧名は消えている。
                    let new_idx: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_trusted_users_agent'",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(new_idx, 1, "idx_trusted_users_agent が存在すること");
                },
            },
        ]
}

struct OldDbGeneration {
    name: &'static str,
    schema: &'static str,
    verify: fn(&Connection),
}

#[test]
fn initialize_upgrades_old_pre_versioning_db_shapes() {
    for gen in old_db_generations() {
        let conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(gen.schema)
            .unwrap_or_else(|e| panic!("[{}] seed old schema: {e}", gen.name));
        // 版管理導入前の DB は user_version=0。
        conn.execute_batch("PRAGMA user_version = 0;").unwrap();

        initialize(&conn).unwrap_or_else(|e| panic!("[{}] initialize failed: {e}", gen.name));

        assert_eq!(
            schema_version(&conn).unwrap(),
            latest_version(),
            "[{}] 最新版にスタンプされていること",
            gen.name
        );
        let idx: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_memory_index_nodes_short_id'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        assert_eq!(idx, 1, "[{}] short_id index が存在すること", gen.name);
        (gen.verify)(&conn);
    }
}

/// B. 冪等性: baseline 済みDBで initialize を再実行しても破壊的再構築は走らず、
/// 既存データが保持される。
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

fn create_marker(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")
}

/// C. 番号付きマイグレーションのランナー: 未適用の version 2 を適用し、
/// 再実行では no-op になる（version gate）。
#[test]
fn run_migrations_applies_and_then_skips() {
    let conn = crate::init_memory().expect("init");
    // 実 MIGRATIONS 適用済み（= 最新版）なので、fake v2 が未適用となる状態に戻す。
    conn.execute_batch("PRAGMA user_version = 1").unwrap();
    let fake = &[Migration {
        version: 2,
        description: "add test_marker",
        up: create_marker,
    }];

    run_migrations(&conn, fake).expect("apply v2");
    assert!(table_exists(&conn, "test_marker").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), 2);

    // 再実行は no-op（既に version 2）。
    run_migrations(&conn, fake).expect("re-run no-op");
    assert_eq!(schema_version(&conn).unwrap(), 2);
}

fn fail_after_marker(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")?;
    Err(rusqlite::Error::InvalidQuery)
}

/// D. マイグレーション失敗時は、その up の変更と version スタンプが
/// トランザクションごとロールバックされる。
#[test]
fn failed_migration_rolls_back_and_leaves_version() {
    let conn = crate::init_memory().expect("init");
    // 実 MIGRATIONS 適用済みなので、fake v2 が適用対象となる状態に戻す。
    conn.execute_batch("PRAGMA user_version = 1").unwrap();
    let fake = &[Migration {
        version: 2,
        description: "fails",
        up: fail_after_marker,
    }];

    let result = run_migrations(&conn, fake);
    assert!(result.is_err());
    assert!(!table_exists(&conn, "test_marker").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), BASELINE_VERSION);
}

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

/// v19: Nostr 受信転記先の表が増えるだけで、既存の Nostr 設定
/// （`agent_nostr_config`）の行は 1 つも動かない（#252 段階 A）。冪等でもある。
#[test]
fn agent_nostr_relay_config_migration_v19_adds_table_without_touching_existing_rows() {
    let conn = crate::init_memory().expect("init");
    // v18 相当の既存 DB を模す: 新表を落として version を 18 へ戻す。
    // 既存の Nostr 設定に行を 1 件入れておき、移行で動かないことを見る。
    conn.execute_batch(
        "DROP TABLE IF EXISTS agent_nostr_relay_config;
             INSERT INTO agent_nostr_config
               (agent_id, secret_key, relays_json, filter_json, enabled, updated_at)
               VALUES ('a1', 'nsec1keep', '[\"wss://yabu.me\"]', '{}', 1, '2026-01-01');
             PRAGMA user_version = 18",
    )
    .unwrap();
    assert!(!table_exists(&conn, "agent_nostr_relay_config").unwrap());

    initialize(&conn).expect("upgrade v18 -> v19");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(table_exists(&conn, "agent_nostr_relay_config").unwrap());

    // 新表は空で始まる（既定は「設定なし」＝無効 / fail-closed）。
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_nostr_relay_config", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0);

    // 既存の Nostr 設定は 1 バイトも変わらない。
    let (secret, enabled): (String, i64) = conn
        .query_row(
            "SELECT secret_key, enabled FROM agent_nostr_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((secret.as_str(), enabled), ("nsec1keep", 1));

    // 行を入れてから再実行しても冪等（CREATE TABLE IF NOT EXISTS で消えない）。
    conn.execute_batch(
        "INSERT INTO agent_nostr_relay_config (agent_id, enabled, webhook_url, updated_at)
             VALUES ('a1', 1, 'https://discord.com/api/webhooks/1/tok', '2026-01-02')",
    )
    .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: String = conn
        .query_row(
            "SELECT webhook_url FROM agent_nostr_relay_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, "https://discord.com/api/webhooks/1/tok");
}

/// v22: `agent_nostr_config.owner_pubkey` の付与（#319）。
///
/// **既存の行を失わない**こと（秘密鍵・リレー・有効フラグがそのまま残る）と、
/// 既存行の新しい列が **空＝オーナー未設定**（誰もオーナーにならない）で始まる
/// ことを見る。冪等性（新規 DB は SCHEMA_SQL 側で列を持つ）も確認する。
#[test]
fn nostr_owner_pubkey_migration_v22_preserves_existing_rows() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来）。
    assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

    // v20 相当の既存 DB を模す: 列を持たない表を作り直して version を 20 へ戻す。
    // 行の内容は「移行後も 1 バイトも変わらない」ことを見るための実データ。
    conn.execute_batch(
            "DROP TABLE agent_nostr_config;
             CREATE TABLE agent_nostr_config (
                agent_id TEXT PRIMARY KEY,
                secret_key TEXT NOT NULL,
                relays_json TEXT NOT NULL DEFAULT '[]',
                filter_json TEXT NOT NULL DEFAULT '{}',
                enabled INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
             );
             INSERT INTO agent_nostr_config
               (agent_id, secret_key, relays_json, filter_json, enabled, updated_at)
               VALUES ('a1', 'nsec1keep', '[\"wss://relay.example\"]', '{\"keywords\":[\"x\"]}', 1, '2026-01-01');
             PRAGMA user_version = 20",
        )
        .unwrap();
    assert!(!column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

    initialize(&conn).expect("upgrade v20 -> v22");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

    // 既存行はそのまま残り、新しい列は空（＝オーナー未設定 / fail-closed）。
    let (secret, relays, filter, enabled, owner): (String, String, String, i64, String) = conn
        .query_row(
            "SELECT secret_key, relays_json, filter_json, enabled, owner_pubkey
                 FROM agent_nostr_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("既存行が失われた");
    assert_eq!(secret, "nsec1keep");
    assert_eq!(relays, r#"["wss://relay.example"]"#);
    assert_eq!(filter, r#"{"keywords":["x"]}"#);
    assert_eq!(enabled, 1);
    assert_eq!(owner, "", "移行直後にオーナーが居てはいけない");

    // 再実行しても列は 1 つのまま（column_exists ガード）で、値も消えない。
    conn.execute_batch("UPDATE agent_nostr_config SET owner_pubkey = 'ff' WHERE agent_id = 'a1'")
        .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: String = conn
        .query_row(
            "SELECT owner_pubkey FROM agent_nostr_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, "ff");
}

/// v24: 既存 DB（`user_version = 23`, `created_caller` 列なし）へ番号付き
/// マイグレーションが届き、列が追加され、既存行は NULL のまま保たれることを検証する。
///
/// これは #349 の本番事故の再現ガード。#347 は列追加を凍結 `migrate()` に置いたため、
/// 既に版がスタンプ済みの本番 DB では走らず、全スキル SELECT が
/// `no such column: created_caller` で落ちた。CI は毎回新規 DB（`migrate()` が走る）
/// なので検出できなかった。ここでは **既存 DB を模して** `run_migrations` 経路のみで
/// 列が届くことを固定する。
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

/// v20 の DB が **v21（impressions の再構築）→ v22（owner_pubkey の追加）** を
/// 順に通り、**どちらのデータも失われない**（#314 と #319 が同じ版列に並ぶ）。
///
/// v21 は表の再構築（`DROP TABLE impressions`）を伴い、v22 は別の表への列追加。
/// 別々のマイグレーションが独立に通ることは各テストで見ているが、**同じ 1 回の
/// 起動で連続して流れる**のは実運用の経路（v20 で止まっていた DB が今回の
/// バイナリで初めて起動する）なので、ここで両方を 1 本の流れとして固定する。
#[test]
fn v20_db_upgrades_through_v21_and_v22_without_losing_data() {
    let conn = crate::init_memory().expect("init");

    // (1) 旧一意制約の impressions に行を入れ、version 20 へ戻す。
    seed_legacy_impressions(
            &conn,
            "('i1', 'a1', 's-discord', 'u1', 'Alice', 'p1', '', '', '中立', '', 0, '2026-01-01', '2026-01-01'),
             ('i2', 'a1', 's-nostr',   'u2', 'Bob',   'p2', '', '', '中立', '', 0, '2026-01-02', '2026-01-02')",
        );
    // (2) 同じ DB の agent_nostr_config も v20 相当（owner_pubkey 無し）へ戻す。
    //     `seed_legacy_impressions` が既に version 20 を刻んでいるので、ここでも
    //     刻み直して「両方が v20 の状態」を作る。
    conn.execute_batch(
        "DROP TABLE agent_nostr_config;
             CREATE TABLE agent_nostr_config (
                agent_id TEXT PRIMARY KEY,
                secret_key TEXT NOT NULL,
                relays_json TEXT NOT NULL DEFAULT '[]',
                filter_json TEXT NOT NULL DEFAULT '{}',
                enabled INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
             );
             INSERT INTO agent_nostr_config
               (agent_id, secret_key, relays_json, filter_json, enabled, updated_at)
               VALUES ('a1', 'nsec1keep', '[\"wss://relay.example\"]', '{}', 1, '2026-01-01');
             PRAGMA user_version = 20",
    )
    .unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 20);
    assert!(!column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

    // (3) 1 回の起動で v21 → v22 が順に流れる。
    initialize(&conn).expect("upgrade v20 -> v21 -> v22");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // (4a) v21 の結果: 行は 2 件とも残り、一意制約が agent スコープになっている。
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM impressions ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec!["i1", "i2"], "v21 で人物像の行が失われた");
    // 別セッションからでも同じ (agent_id, target_id) は 1 行に収束する
    // （旧制約のままなら 2 行目が入ってしまう）。
    conn.execute_batch(
            "INSERT INTO impressions
               (id, agent_id, session_id, target_id, target_name, personality,
                communication_style, recent_behavior, agreement, notes,
                last_updated_turn, created_at, updated_at)
               VALUES ('i3', 'a1', 's-other', 'u1', 'Alice', 'p9', '', '', '中立', '', 0, '2026-01-05', '2026-01-05')
             ON CONFLICT(agent_id, target_id) DO UPDATE SET personality = excluded.personality",
        )
        .expect("新しい一意制約で upsert できる");
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM impressions WHERE agent_id = 'a1' AND target_id = 'u1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);

    // (4b) v22 の結果: Nostr 設定の行はそのまま残り、オーナーは未設定で始まる。
    let (secret, relays, enabled, owner): (String, String, i64, String) = conn
        .query_row(
            "SELECT secret_key, relays_json, enabled, owner_pubkey
                 FROM agent_nostr_config WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("v22 で Nostr 設定の行が失われた");
    assert_eq!(secret, "nsec1keep");
    assert_eq!(relays, r#"["wss://relay.example"]"#);
    assert_eq!(enabled, 1);
    assert_eq!(owner, "", "移行直後にオーナーが居てはいけない");
}

/// 新規 DB は最初から最新版で、**v21 と v22 の両方の構造**を持つ
/// （`SCHEMA_SQL` と `MIGRATIONS` が食い違っていない）。
#[test]
fn fresh_db_has_both_v21_and_v22_structures() {
    let conn = crate::init_memory().expect("init");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    // v22: 列がある。
    assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());
    // v21: `(agent_id, target_id)` ちょうど 2 列の UNIQUE 索引がある。
    let unique_pairs: i64 = conn
        .query_row(
            r#"SELECT COUNT(*) FROM pragma_index_list('impressions') il
                    WHERE il."unique" = 1
                      AND (SELECT COUNT(*) FROM pragma_index_info(il.name)) = 2
                      AND (SELECT COUNT(*) FROM pragma_index_info(il.name) ii
                            WHERE ii.name IN ('agent_id', 'target_id')) = 2"#,
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unique_pairs, 1, "新規 DB に v21 の一意制約が無い");
}

/// v23: node_type の CHECK を `'category'`/`'meta'` へ拡張し、参照表を新設する。
/// v22 相当（狭い CHECK）の既存 DB から、既存の時系列ノードを1件も失わずに
/// 移行できること、移行後は category/meta が実際に**保存できる**こと（`INSERT OR
/// IGNORE` による沈黙が起きないこと）を固定する。
#[test]
fn memory_index_category_migration_v23_widens_check_and_preserves_rows() {
    let conn = crate::init_memory().expect("init");

    // v22 相当の既存 DB を模す: 狭い CHECK のテーブルへ作り直し、時系列ツリーを積む。
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    conn.execute_batch(
            "DROP TABLE memory_index_nodes;
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
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT,
                keywords_json TEXT NOT NULL DEFAULT '[]', summary_refreshed_at TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 'root', 's', '2026-01-01', '2026-01-01'),
                    ('p', 'a1', 'r', 'period', '2026-01', 's', '2026-01-01', '2026-01-01'),
                    ('sess', 'a1', 'p', 'session', 'S', 's', '2026-01-01', '2026-01-01'),
                    ('t', 'a1', 'sess', 'topic', 'Rust入門', 's', '2026-01-02', '2026-01-02');
             DROP TABLE IF EXISTS memory_category_members;
             PRAGMA user_version = 22;",
        )
        .unwrap();

    // 狭い CHECK では category ノードは拒否される（＝移行が必要な証拠）。
    assert!(
            conn.execute(
                "INSERT INTO memory_index_nodes (id, agent_id, node_type, title, summary, created_at, updated_at)
                 VALUES ('c0', 'a1', 'category', 'X', '', '2026-01-01', '2026-01-01')",
                [],
            )
            .is_err(),
            "移行前は category が CHECK で拒否されるはず"
        );

    run_migrations(&conn, MIGRATIONS).expect("v23 migration");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 時系列ツリーは1件も失われていない。
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM memory_index_nodes ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec!["p", "r", "sess", "t"]);

    // 移行後は category / meta が実際に保存できる（OR IGNORE の沈黙が起きない）。
    conn.execute(
            "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('cat1', 'a1', NULL, 'category', 'category', 'kojiraさんの教え', '', '2026-02-01', '2026-02-01'),
                    ('meta1', 'a1', NULL, 'meta', 'category', 'ルール群', '', '2026-02-01', '2026-02-01')",
            [],
        )
        .expect("移行後は category/meta を登録できる");

    // 参照表が存在し、割当を保存できる。全チェーンは v26 まで走るので PK は多対多
    // （`(agent_id, topic_id, category_id)`）＝同じ topic に複数の category を付けられる。
    assert!(table_exists(&conn, "memory_category_members").unwrap());
    conn.execute(
        "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
             VALUES ('a1', 't', 'cat1', '2026-02-01'),
                    ('a1', 't', 'meta1', '2026-02-02')",
        [],
    )
    .expect("v26 の多対多 PK では同一 topic に複数 category を付けられる");
}

/// v26: 分類レイヤの白紙化 + members の多対多 PK 化（issue #358）。本番相当の v25 DB
/// （分類ノード + その FTS 行 + 旧 PK の members 行 + 記憶本文 memory_curated + 時系列
/// ツリー）を模し、`run_migrations` 経路のみで:
///  - category/meta ノードと**その FTS 行**が消えること（FTS 孤児を残さない）
///  - 時系列ツリー（node_type 別）と memory_curated が 1 行も減らないこと
///  - members が空になり、PK が多対多になって同一 topic に複数 category を入れられること
///  - user_version が上がり、2 回実行しても落ちないこと
/// を固定する。
#[test]
fn memory_index_reset_and_multi_tag_migration_v26() {
    let conn = crate::init_memory().expect("init");

    // --- v25 相当へ戻す: members を旧 PK (agent_id, topic_id) で作り直し、user_version=25 ---
    conn.execute_batch(
        "DROP TABLE IF EXISTS memory_category_members;
             CREATE TABLE memory_category_members (
                agent_id TEXT NOT NULL,
                topic_id TEXT NOT NULL,
                category_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (agent_id, topic_id)
             );",
    )
    .unwrap();

    // 時系列ツリー（root/period/session/topic/daily）+ 分類ノード（category/meta）を積む。
    // 分類ノードは insert_index_node と同様に FTS へも入れて「孤児が残らない」ことを見る。
    conn.execute_batch(
            "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 'session_log', 'root', 's', '2026-01-01', '2026-01-01'),
                    ('p', 'a1', 'r', 'period', 'session_log', '2026-01', 's', '2026-01-01', '2026-01-01'),
                    ('sess', 'a1', 'p', 'session', 'session_log', 'S', 's', '2026-01-01', '2026-01-01'),
                    ('t', 'a1', 'sess', 'topic', 'session_log', 'Rust入門', 's', '2026-01-02', '2026-01-02'),
                    ('d', 'a1', NULL, 'daily', 'daily_log', '2026-01-02', 's', '2026-01-02', '2026-01-02'),
                    ('cat1', 'a1', NULL, 'category', 'category', 'kojiraさんの教え', '', '2026-02-01', '2026-02-01'),
                    ('meta1', 'a1', NULL, 'meta', 'category', 'ルール群', '', '2026-02-01', '2026-02-01');
             -- 全ノードを FTS へ（分類ノードも。孤児掃除の対象になる）。
             INSERT INTO memory_index_fts (title, summary, keywords, node_id, agent_id, node_type, source_type)
             SELECT title, summary, '', id, agent_id, node_type, source_type FROM memory_index_nodes;
             -- 旧 PK members に割当を積む（白紙化される）。
             INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
             VALUES ('a1', 't', 'cat1', '2026-02-01');
             -- 記憶本文（絶対に消さない）。
             INSERT INTO memory_curated (id, agent_id, category, content, updated_at)
             VALUES ('m1', 'a1', 'long_term/rule', '送金は必ず二重確認', '2026-02-01'),
                    ('m2', 'a1', 'reflection', '振り返り', '2026-02-01');
             PRAGMA user_version = 25;",
        )
        .unwrap();

    // --- 適用前のスナップショット ---
    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    let curated_before = count("SELECT COUNT(*) FROM memory_curated");
    let timeline_before = count(
            "SELECT COUNT(*) FROM memory_index_nodes
                 WHERE node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')",
        );
    assert_eq!(curated_before, 2);
    assert_eq!(timeline_before, 5);
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_index_nodes WHERE node_type IN ('category','meta')"),
        2
    );
    assert_eq!(count("SELECT COUNT(*) FROM memory_category_members"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM memory_index_fts"), 7);

    // --- v26 を適用（run_migrations 経路のみ。本番と同じ道）---
    run_migrations(&conn, MIGRATIONS).expect("v26 migration");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 分類ノードは消え、時系列ツリーと memory_curated は 1 行も減らない。
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_index_nodes WHERE node_type IN ('category','meta')"),
        0,
        "分類ノードが白紙化される"
    );
    assert_eq!(
            count(
                "SELECT COUNT(*) FROM memory_index_nodes
                     WHERE node_type IN ('root','period','session','topic','daily','hourly','weekly','monthly','yearly')",
            ),
            timeline_before,
            "時系列ツリーは 1 行も減らない"
        );
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_curated"),
        curated_before,
        "記憶本文は 1 行も減らない"
    );
    // FTS 孤児が残らない: 分類ノードの 2 行が消え、時系列 5 行だけが残る。
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_index_fts"),
        5,
        "FTS 孤児を残さない"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_index_fts WHERE node_type IN ('category','meta')"),
        0
    );
    // node_type の CHECK からは category/meta を外さない（段階2で使い直す）: 再登録できる。
    conn.execute(
            "INSERT INTO memory_index_nodes (id, agent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('cat2', 'a1', 'category', 'category', 'Y', '', '2026-03-01', '2026-03-01')",
            [],
        )
        .expect("category は CHECK に残っているので再登録できる");

    // members は空になり、PK が多対多。同一 topic に複数 category を入れられる。
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_category_members"),
        0,
        "members は白紙化"
    );
    conn.execute(
        "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
             VALUES ('a1', 't', 'cat2', '2026-03-01'),
                    ('a1', 't', 'meta9', '2026-03-02')",
        [],
    )
    .expect("多対多 PK では同一 topic に複数 category を付けられる");
    assert!(
        conn.execute(
            "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
                 VALUES ('a1', 't', 'cat2', '2026-03-03')",
            [],
        )
        .is_err(),
        "同一 (agent_id, topic_id, category_id) の重複は PK で拒否される"
    );

    // --- 冪等性: 2 回目は落ちず、既に多対多なので members を作り直さない（行が残る）---
    run_migrations(&conn, MIGRATIONS).expect("v26 は冪等（2 回目も落ちない）");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_category_members"),
        2,
        "2 回目は members を作り直さない（多対多を検知して skip）"
    );
}

/// v32: 既存の受信行（送信者名義）を受信側エージェント名義へ付け替え、索引/FTS に
/// 載せる（#380）。session_id に埋まった agent_id で受信側を復元できる行だけを対象にし、
/// 復元できない行・新形・旧々形・応答は 1 行も触らないこと、FTS も同時に直ること、
/// FTS 検索へ受信側名義で載ること、冪等であることを固定する。索引ビルドについては
/// 「watermark を巻き戻せば載る形」かつ「watermark 先行下では載らない」の両側を固定する
/// （v32 が回復するのは FTS 検索のみで、索引への取り込みは #380 に残る）。
#[test]
fn v32_remaps_inbound_agent_id_to_recipient_and_indexes() {
    use crate::queries::{
        get_unindexed_session_logs, insert_session_log, search_session_logs, SessionLogRow,
    };
    let conn = crate::init_memory().expect("init");

    // 受信側エージェント（session_id に UUID が埋まる）。migration は agents と JOIN する。
    let recipient = "aaaaaaaa-1111-2222-3333-444444444444";
    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES (?1, 'r', 'p')",
        [recipient],
    )
    .unwrap();

    let mk = |agent: &str, session: &str, speaker: &str, content: &str, meta: Option<&str>| {
        SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: meta.map(|m| m.to_string()),
            created_at: None,
        }
    };

    // (1) 旧形の受信行・discord（復元可能）: agent=speaker=送信者、session に recipient が埋まる。
    let id_discord = insert_session_log(
        &conn,
        &mk(
            "sender-d",
            &format!("discord-{recipient}-100-200"),
            "sender-d",
            "discord inbound apple",
            Some(r#"{"source":"discord"}"#),
        ),
    )
    .unwrap();
    // (2) 旧形の受信行・nostr（復元可能, pubkey 付き session）。
    let id_nostr = insert_session_log(
        &conn,
        &mk(
            "sender-n",
            &format!("nostr-{recipient}-deadbeef"),
            "sender-n",
            "nostr inbound banana",
            Some(r#"{"source":"nostr"}"#),
        ),
    )
    .unwrap();
    // (3) 旧形の受信行・nostr（復元可能, recipient 単独 session）。
    let id_nostr2 = insert_session_log(
        &conn,
        &mk(
            "sender-n2",
            &format!("nostr-{recipient}"),
            "sender-n2",
            "nostr inbound cherry",
            Some(r#"{"source":"nostr"}"#),
        ),
    )
    .unwrap();
    // (4) 復元不能: discord-{guild}-{channel}（agent_id が埋まっていない）→ 触らない。
    let id_unresolved = insert_session_log(
        &conn,
        &mk(
            "sender-u",
            "discord-100-200",
            "sender-u",
            "unresolved inbound durian",
            Some(r#"{"source":"discord"}"#),
        ),
    )
    .unwrap();
    // (5) 新形の受信行（既に受信側名義, agent≠speaker）→ 触らない。
    let id_newform = insert_session_log(
        &conn,
        &mk(
            recipient,
            &format!("discord-{recipient}-100-200"),
            "sender-x",
            "newform inbound elder",
            Some(r#"{"source":"discord"}"#),
        ),
    )
    .unwrap();
    // (6) 旧々形（metadata 無し・既に受信側名義）→ 触らない。
    let id_oldold = insert_session_log(
        &conn,
        &mk(
            recipient,
            &format!("discord-{recipient}-100-200"),
            "sender-y",
            "oldold inbound fig",
            None,
        ),
    )
    .unwrap();
    // (7) 応答行（source discord_response, agent=speaker=recipient）→ 触らない。
    let id_reply = insert_session_log(
        &conn,
        &mk(
            recipient,
            &format!("discord-{recipient}-100-200"),
            recipient,
            "reply grape",
            Some(r#"{"source":"discord_response"}"#),
        ),
    )
    .unwrap();

    // v31 起点へ落として run_migrations で v32 を実際に走らせる。
    conn.execute_batch("PRAGMA user_version = 31").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v32");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    let agent_of = |id: i64| -> String {
        conn.query_row(
            "SELECT agent_id FROM memory_sessions WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let fts_agent_of = |id: i64| -> String {
        conn.query_row(
            "SELECT agent_id FROM memory_sessions_fts WHERE rowid=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let speaker_of = |id: i64| -> String {
        conn.query_row(
            "SELECT speaker_id FROM memory_sessions WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };

    // (1)(2)(3) 受信側名義へ付け替わり、FTS も追随、speaker_id は送信者のまま。
    for (id, sender) in [
        (id_discord, "sender-d"),
        (id_nostr, "sender-n"),
        (id_nostr2, "sender-n2"),
    ] {
        assert_eq!(agent_of(id), recipient, "本体 agent_id が受信側へ");
        assert_eq!(fts_agent_of(id), recipient, "FTS agent_id が受信側へ");
        assert_eq!(speaker_of(id), sender, "speaker_id は送信者のまま");
    }

    // (4)(5)(6)(7) 触っていない。
    assert_eq!(agent_of(id_unresolved), "sender-u", "復元不能行は不変");
    assert_eq!(
        fts_agent_of(id_unresolved),
        "sender-u",
        "復元不能行の FTS も不変"
    );
    assert_eq!(agent_of(id_newform), recipient, "新形は不変");
    assert_eq!(agent_of(id_oldold), recipient, "旧々形は不変");
    assert_eq!(agent_of(id_reply), recipient, "応答行は不変");

    // 索引ビルド入力に受信側名義で載る（送信者名義では載らない）。
    //
    // ここは `after_id = 0`（＝索引 watermark を巻き戻した状態）での確認であって、
    // 「watermark を巻き戻せば受信側名義で載る形になっている」ことだけを固定している。
    // 実際の索引ビルドは watermark（`last_indexed_log_id`）を `after_id` へ渡す
    // （`crates/core/src/memory_index/index_builder.rs`）ため、**本番のように watermark が
    // 対象行より先行している状況では、v32 だけでは索引へ入らない**（直下でその側も固定する）。
    // 索引ビルドへの実取り込みには別途 #380 の対応が要る。
    let indexed = get_unindexed_session_logs(&conn, recipient, 0, 100).unwrap();
    assert!(indexed.iter().any(|r| r.content == "discord inbound apple"));
    assert!(indexed.iter().any(|r| r.content == "nostr inbound banana"));
    assert!(
        get_unindexed_session_logs(&conn, "sender-d", 0, 100)
            .unwrap()
            .is_empty(),
        "受信行が送信者名義で索引入力に残っている"
    );

    // watermark が対象行より先行している状態（本番がこれ）では、付け替えても索引ビルド
    // 入力には載らない。v32 の効き目が FTS 検索に限られることを明示的に固定する。
    //
    // watermark は**付け替え対象の最大 id**（=(3)）にする。全体の MAX(id) にすると結果が
    // 必ず空になり、付け替えが 1 行も起きていなくても通る空回りのテストになる。(3) を境に
    // すれば recipient 名義の (5)(6)(7) は結果に残るので、「クエリが何も返していないだけ」
    // ではなく「付け替え行**だけ**が watermark に切られている」ことを固定できる。
    let above_watermark = get_unindexed_session_logs(&conn, recipient, id_nostr2, 100).unwrap();
    assert!(
        above_watermark
            .iter()
            .any(|r| r.content == "newform inbound elder"),
        "watermark より上の受信側名義行は載る（フィルタが空を返しているだけではない証拠）"
    );
    assert!(
        !above_watermark
            .iter()
            .any(|r| r.content == "discord inbound apple"
                || r.content == "nostr inbound banana"
                || r.content == "nostr inbound cherry"),
        "watermark 先行下では付け替え行は索引ビルド入力に載らない（#380 の残課題）"
    );

    // FTS 記憶検索で受信側が相手の発言を引ける。送信者名義では引けない。
    let hits = search_session_logs(&conn, recipient, "apple", 10).unwrap();
    assert!(hits.iter().any(|h| h.content == "discord inbound apple"));
    assert!(search_session_logs(&conn, "sender-d", "apple", 10)
        .unwrap()
        .is_empty());
    // 復元不能行は送信者名義のまま（付け替えていない証拠 = 誤って混ぜていない）。
    assert!(search_session_logs(&conn, "sender-u", "durian", 10)
        .unwrap()
        .iter()
        .any(|h| h.content == "unresolved inbound durian"));

    // 冪等: 版を 31 へ戻して up() を再実行しても、付け替えは二重に起きない。
    let same_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = speaker_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute_batch("PRAGMA user_version = 31").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v32 再実行");
    let same_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = speaker_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(same_before, same_after, "2 回目で付け替えが二重に起きない");
    assert_eq!(
        agent_of(id_discord),
        recipient,
        "再実行後も (1) は受信側のまま"
    );
    assert_eq!(
        agent_of(id_unresolved),
        "sender-u",
        "再実行後も復元不能行は不変"
    );
}

/// v33: sleep のメンテナンスラン（宣言 `sleep-declare-*` / 整理 `sleep-organize-*`）が
/// 生んだ生ログを消す（#393）。本体と FTS を**同一 rowid 集合**で消すこと（＝孤児を
/// 増やさない）、他の接頭辞のログを 1 行も巻き込まないこと、既存の孤児に触らないこと、
/// 運用記録（`llm_logs` / `agent_logs`）を消さないこと、冪等であることを固定する。
#[test]
fn v33_deletes_maintenance_run_logs_from_both_tables() {
    use crate::queries::{insert_session_log, SessionLogRow};
    let conn = crate::init_memory().expect("init");
    let agent = "a1";

    let mk = |session: &str, content: &str| SessionLogRow {
        id: None,
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        log_type: "speech".to_string(),
        content: content.to_string(),
        speaker_id: Some(agent.to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };

    // メンテナンスランの生ログ（消える）。
    let id_declare =
        insert_session_log(&conn, &mk("sleep-declare-a1-1700000000", "declare")).expect("declare");
    let id_organize = insert_session_log(&conn, &mk("sleep-organize-a1-1700000001", "organize"))
        .expect("organize");
    // 通常の会話・ハートビート等（残る）。`sleep` を含むが接頭辞ではない session_id や、
    // 接頭辞に見えて別物の `sleep-` も混ぜて、LIKE が広く効きすぎないことを確かめる。
    let keep = [
        ("discord-a1-100-200", "discord"),
        ("heartbeat-a1-100", "heartbeat"),
        ("subtask-42", "subtask"),
        ("nostr-a1", "nostr"),
        ("web-a1-conv", "web"),
        ("agent-msg-a1-u1", "rest"),
        ("discord-a1-sleep-declare-x", "not a maintenance run"),
    ];
    let keep_ids: Vec<i64> = keep
        .iter()
        .map(|(s, c)| insert_session_log(&conn, &mk(s, c)).expect("keep"))
        .collect();

    // 既存の孤児（本体に対応行が無い FTS 行）を 1 件仕込む。本番にも 208 行あり、
    // v33 がそれを増やしも減らしもしないことを固定する。
    conn.execute(
        "INSERT INTO memory_sessions_fts (rowid, content, agent_id, session_id, log_type)
             VALUES (999999, 'orphan', ?1, 'discord-a1-orphan', 'speech')",
        [agent],
    )
    .expect("orphan");

    // 運用記録（#393 の追加受け入れ条件）。同じメンテナンスランの `llm_logs`
    // （session_id で引ける生プロンプト/生応答/tool_calls）と `agent_logs`
    // （context="sleep" の 1 ラン 1 行の要約）を仕込み、**v33 がこれらを消さない**ことを
    // 固定する。生ログから外すのは「記憶の材料としての扱い」だけで、何を行ったかの
    // 運用記録は残す。
    crate::queries::insert_llm_log(
        &conn,
        &crate::queries::LlmLogRow {
            id: "llm-1".to_string(),
            agent_id: agent.to_string(),
            session_id: Some("sleep-declare-a1-1700000000".to_string()),
            model: Some("m".to_string()),
            prompt: "p".to_string(),
            response: "r".to_string(),
            tool_calls: Some("[]".to_string()),
            latency_ms: Some(1),
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            error_code: None,
            error_body: None,
            requested_at: None,
            trigger_message_id: None,
            is_bot_iteration: false,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            created_at: "2026-08-05T00:00:00+00:00".to_string(),
        },
    )
    .expect("llm_log");
    crate::queries::insert_agent_log(
        &conn,
        &crate::queries::AgentLogRow {
            id: "audit-1".to_string(),
            agent_id: Some(agent.to_string()),
            level: "info".to_string(),
            context: "sleep".to_string(),
            message: r#"{"kind":"memory_declare"}"#.to_string(),
            created_at: None,
        },
    )
    .expect("agent_log");

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    let orphans = |c: &Connection| -> i64 {
        c.query_row(
            "SELECT COUNT(*) FROM memory_sessions_fts f
                 LEFT JOIN memory_sessions m ON m.id = f.rowid WHERE m.id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    let body_before = count("SELECT COUNT(*) FROM memory_sessions");
    let fts_before = count("SELECT COUNT(*) FROM memory_sessions_fts");
    let orphans_before = orphans(&conn);
    assert_eq!(orphans_before, 1, "孤児の仕込みが効いている");

    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 対象 2 行が本体からも FTS からも消えている。
    for id in [id_declare, id_organize] {
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE id = {id}"
            )),
            0,
            "本体から消える"
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions_fts WHERE rowid = {id}"
            )),
            0,
            "FTS からも同じ rowid で消える"
        );
    }
    // 他は 1 行も巻き込んでいない（本体・FTS とも）。
    for id in &keep_ids {
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE id = {id}"
            )),
            1,
            "メンテナンスラン以外は残る"
        );
        assert_eq!(
            count(&format!(
                "SELECT COUNT(*) FROM memory_sessions_fts WHERE rowid = {id}"
            )),
            1,
            "FTS 側も残る"
        );
    }
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_sessions"),
        body_before - 2
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_sessions_fts"),
        fts_before - 2
    );
    assert_eq!(orphans(&conn), orphans_before, "孤児は増えも減りもしない");

    // 運用記録は 1 行も消えていない。session_id が対象と同じ `sleep-declare-%` でも
    // `llm_logs` は対象外（消すのは memory_sessions と memory_sessions_fts だけ）。
    assert_eq!(
        count("SELECT COUNT(*) FROM llm_logs WHERE session_id LIKE 'sleep-declare-%'"),
        1,
        "llm_logs は消さない（何を行ったかの記録は残す）"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM agent_logs WHERE context = 'sleep'"),
        1,
        "agent_logs（sleep 監査）は消さない"
    );

    // 冪等: 版を 32 へ戻して再実行しても何も変わらない（対象 0 行）。
    let body_after = count("SELECT COUNT(*) FROM memory_sessions");
    let fts_after = count("SELECT COUNT(*) FROM memory_sessions_fts");
    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33 再実行");
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), body_after);
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions_fts"), fts_after);
    assert_eq!(orphans(&conn), orphans_before);
    assert_eq!(count("SELECT COUNT(*) FROM llm_logs"), 1);
    assert_eq!(count("SELECT COUNT(*) FROM agent_logs"), 1);
}

/// v33（索引側 / #393 の 2 巡目レビュー指摘）: メンテナンスランのログから作られた
/// 索引ノードも消す。生ログだけ消すと「タイトルと要約はあるが中身を引くと空が返る記憶」が
/// 残るため（索引ビルドは本番で対象 id 帯を通過済み）。
///
/// 固定するもの:
/// - `source_session_id` がメンテナンスランを指す session / topic ノードが本体と
///   `memory_index_fts` の**同一 node_id 集合**で消えること
/// - 対象ノードの子孫も消えること（FTS 孤児を残さない）
/// - 空になる親（period）は**残す**こと
/// - 通常の会話由来のノードを 1 件も巻き込まないこと
/// - 宣言ユニットは「範囲が丸ごとメンテナンスラン由来」のものだけ消し、通常ログが
///   混じるユニット・範囲が空のユニットは残すこと
/// - `memory_category_members` の宙に浮く参照が消えること（残るノードの行は残ること）
/// - 冪等であること
#[test]
fn v33_deletes_maintenance_run_index_nodes_and_only_pure_maintenance_units() {
    use crate::queries::{insert_index_node, insert_session_log, IndexNodeRow, SessionLogRow};
    let conn = crate::init_memory().expect("init");
    let agent = "a1";

    let log = |session: &str, content: &str| SessionLogRow {
        id: None,
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        log_type: "speech".to_string(),
        content: content.to_string(),
        speaker_id: Some(agent.to_string()),
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    // 生ログ: メンテナンスラン 2 行 → 通常 1 行 → メンテナンスラン 1 行 の順に入れて、
    // ユニットの範囲が「丸ごと」か「混在」かを id 範囲で作り分けられるようにする。
    let m1 = insert_session_log(&conn, &log("sleep-declare-a1-1", "m1")).unwrap();
    let m2 = insert_session_log(&conn, &log("sleep-declare-a1-1", "m2")).unwrap();
    let normal = insert_session_log(&conn, &log("discord-a1-1-2", "normal")).unwrap();
    let m3 = insert_session_log(&conn, &log("sleep-organize-a1-1", "m3")).unwrap();

    let node = |id: &str,
                parent: Option<&str>,
                node_type: &str,
                src_session: Option<&str>,
                range: Option<(i64, i64)>| IndexNodeRow {
        id: id.to_string(),
        agent_id: agent.to_string(),
        parent_id: parent.map(|s| s.to_string()),
        node_type: node_type.to_string(),
        source_type: if node_type == "unit" {
            "declared".to_string()
        } else {
            "session_log".to_string()
        },
        title: format!("title-{id}"),
        summary: format!("summary-{id}"),
        start_log_id: range.map(|r| r.0),
        end_log_id: range.map(|r| r.1),
        source_session_id: src_session.map(|s| s.to_string()),
        date_from: None,
        date_to: None,
        depth: 2,
        child_count: 0,
        token_count: 0,
        created_at: "2026-08-05T00:00:00+00:00".to_string(),
        updated_at: "2026-08-05T00:00:00+00:00".to_string(),
        short_id: None,
        keywords_json: "[]".to_string(),
        summary_refreshed_at: None,
    };

    // 親（period）と、その下のメンテナンス由来 session + その topic 子。
    insert_index_node(&conn, &node("period-1", None, "period", None, None)).unwrap();
    insert_index_node(
        &conn,
        &node(
            "sess-m",
            Some("period-1"),
            "session",
            Some("sleep-declare-a1-1"),
            Some((m1, m2)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "topic-m",
            Some("sess-m"),
            "topic",
            Some("sleep-declare-a1-1"),
            Some((m1, m2)),
        ),
    )
    .unwrap();
    // 対象 session の子だが `source_session_id` が無い（再帰で一緒に消える = FTS 孤児を残さない）。
    insert_index_node(
        &conn,
        &node(
            "topic-m-nosrc",
            Some("sess-m"),
            "topic",
            None,
            Some((m1, m2)),
        ),
    )
    .unwrap();
    // 通常の会話由来（残る）。period-1 の子でもあるので、親が空にならない側も確かめられる。
    insert_index_node(
        &conn,
        &node(
            "sess-ok",
            Some("period-1"),
            "session",
            Some("discord-a1-1-2"),
            Some((normal, normal)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "topic-ok",
            Some("sess-ok"),
            "topic",
            Some("discord-a1-1-2"),
            Some((normal, normal)),
        ),
    )
    .unwrap();
    // 子が全て消えて空になる period（本番に 2 件あるケース）。索引ビルダの child_count
    // 再計算は「子を持つ親」しか書かないので、ここが 0 へ直らないと永久にずれたままになる。
    insert_index_node(&conn, &node("period-2", None, "period", None, None)).unwrap();
    insert_index_node(
        &conn,
        &node(
            "sess-m2",
            Some("period-2"),
            "session",
            Some("sleep-organize-a1-1"),
            Some((m3, m3)),
        ),
    )
    .unwrap();
    // ユニット 3 種。範囲が丸ごとメンテナンス由来 / 通常ログ混在 / 範囲が空。
    // 宣言ユニットの根（本番の `declroot-{agent_id}`）も置き、子を 1 つ失う側を作る。
    insert_index_node(&conn, &node("declroot-1", None, "root", None, None)).unwrap();
    insert_index_node(
        &conn,
        &node(
            "unit-pure",
            Some("declroot-1"),
            "unit",
            None,
            Some((m1, m2)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "unit-mixed",
            Some("declroot-1"),
            "unit",
            None,
            Some((m1, m3)),
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &node(
            "unit-empty",
            Some("declroot-1"),
            "unit",
            None,
            Some((900_000, 900_001)),
        ),
    )
    .unwrap();

    // 健全な索引の状態を作る: child_count を実カウントに揃える（本番も全ノードで一致している）。
    conn.execute_batch(
        "UPDATE memory_index_nodes
                 SET child_count = (SELECT COUNT(*) FROM memory_index_nodes c
                                     WHERE c.parent_id = memory_index_nodes.id);",
    )
    .unwrap();

    // カテゴリ所属: 消える topic と残る topic の両方に付ける。
    conn.execute_batch(
        "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
                 VALUES ('a1','topic-m','cat-1','2026-08-05T00:00:00+00:00'),
                        ('a1','topic-ok','cat-1','2026-08-05T00:00:00+00:00');",
    )
    .unwrap();

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    let exists = |id: &str| -> i64 {
        count(&format!(
            "SELECT COUNT(*) FROM memory_index_nodes WHERE id = '{id}'"
        ))
    };
    let in_fts = |id: &str| -> i64 {
        count(&format!(
            "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = '{id}'"
        ))
    };
    let fts_orphans = || -> i64 {
        count(
            "SELECT COUNT(*) FROM memory_index_fts f
                 LEFT JOIN memory_index_nodes n ON n.id = f.node_id WHERE n.id IS NULL",
        )
    };
    let nodes_without_fts = || -> i64 {
        count(
            "SELECT COUNT(*) FROM memory_index_nodes n
                 LEFT JOIN memory_index_fts f ON f.node_id = n.id WHERE f.node_id IS NULL",
        )
    };
    let child_count_of = |id: &str| -> i64 {
        count(&format!(
            "SELECT child_count FROM memory_index_nodes WHERE id = '{id}'"
        ))
    };
    let child_count_mismatch = || -> i64 {
        count(
            "SELECT COUNT(*) FROM memory_index_nodes n
                 WHERE n.child_count <> (SELECT COUNT(*) FROM memory_index_nodes c
                                          WHERE c.parent_id = n.id)",
        )
    };
    assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 12);
    assert_eq!(fts_orphans(), 0);
    assert_eq!(nodes_without_fts(), 0);
    assert_eq!(child_count_mismatch(), 0, "仕込み時点では全ノード一致");
    assert_eq!(child_count_of("period-1"), 2);
    assert_eq!(child_count_of("period-2"), 1);
    assert_eq!(child_count_of("declroot-1"), 3);

    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 消えたもの（本体と FTS の両方から）。
    for id in ["sess-m", "sess-m2", "topic-m", "topic-m-nosrc", "unit-pure"] {
        assert_eq!(exists(id), 0, "{id} は消える");
        assert_eq!(in_fts(id), 0, "{id} は FTS からも同じ集合で消える");
    }
    // 残るもの。空になった period も残す（索引ビルダが同じ id で再利用するキー）。
    for id in [
        "period-1",
        "period-2",
        "declroot-1",
        "sess-ok",
        "topic-ok",
        "unit-mixed",
        "unit-empty",
    ] {
        assert_eq!(exists(id), 1, "{id} は残る");
        assert_eq!(in_fts(id), 1, "{id} の FTS も残る");
    }
    assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 7);
    assert_eq!(fts_orphans(), 0, "FTS 孤児を作らない");
    assert_eq!(nodes_without_fts(), 0, "本体だけのノードも作らない");

    // 生き残る親の child_count が実カウントへ直っている。**子が 0 になった period-2 まで
    // 直ること**が肝（索引ビルダの再計算は子を持つ親しか書かないので、ここで直さないと
    // 永久にずれたまま残る）。
    assert_eq!(child_count_mismatch(), 0, "削除後も全ノードで一致");
    assert_eq!(child_count_of("period-1"), 1, "sess-m を失って 2→1");
    assert_eq!(child_count_of("period-2"), 0, "子が全て消えて 1→0");
    assert_eq!(child_count_of("declroot-1"), 2, "unit-pure を失って 3→2");

    // カテゴリ所属: 消えた topic への参照だけが消え、残る topic の行は残る。
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_category_members WHERE topic_id = 'topic-m'"),
        0,
        "宙に浮く参照を残さない"
    );
    assert_eq!(
        count("SELECT COUNT(*) FROM memory_category_members WHERE topic_id = 'topic-ok'"),
        1,
        "残るノードの所属は残す"
    );

    // 生ログ側: メンテナンス 3 行が消え、通常 1 行は残る。
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), 1);
    assert_eq!(
        count(&format!(
            "SELECT COUNT(*) FROM memory_sessions WHERE id = {normal}"
        )),
        1
    );

    // FTS integrity-check（削除で索引が壊れていないこと）。
    conn.execute_batch("INSERT INTO memory_index_fts(memory_index_fts) VALUES('integrity-check');")
        .expect("memory_index_fts integrity");

    // 冪等: 版を 32 へ戻して再実行しても何も変わらない。生ログが消えた後は
    // 「範囲に 1 件以上ある」が成り立たないので、unit-mixed / unit-empty も対象外のまま。
    conn.execute_batch("PRAGMA user_version = 32").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v33 再実行");
    assert_eq!(count("SELECT COUNT(*) FROM memory_index_nodes"), 7);
    assert_eq!(count("SELECT COUNT(*) FROM memory_sessions"), 1);
    assert_eq!(exists("unit-mixed"), 1, "再実行で本人の記憶を巻き込まない");
    assert_eq!(exists("unit-empty"), 1);
    assert_eq!(fts_orphans(), 0);
    assert_eq!(
        child_count_mismatch(),
        0,
        "再実行でも child_count は一致のまま"
    );
}

/// v20 起点の一気通貫（v20→v21→v22→v23）。稼働中の本番 DB は v22 なので実運用の
/// 経路は v22→v23 だが、新規環境や古い DB からの復元では v20 から連鎖する。この道で
/// (1) memory_index の時系列ツリーが 1 件も失われず CHECK が広がること、(2) 途中の
/// v21（impressions を agent スコープへ）と v22（owner_pubkey 追加）も併せて適用され
/// 最終版へ到達することを固定する（従来は user_version=22 を手で置いた単独移行のみ）。
#[test]
fn migration_chain_from_v20_reaches_latest_and_preserves_memory_index() {
    let conn = crate::init_memory().expect("init");

    // --- v22 相当の memory_index_nodes（狭い CHECK）へ戻し、時系列ツリーを積む ---
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    conn.execute_batch(
            "DROP TABLE memory_index_nodes;
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
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, short_id TEXT,
                keywords_json TEXT NOT NULL DEFAULT '[]', summary_refreshed_at TEXT
             );
             INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, title, summary, created_at, updated_at)
             VALUES ('r', 'a1', NULL, 'root', 'root', 's', '2026-01-01', '2026-01-01'),
                    ('p', 'a1', 'r', 'period', '2026-01', 's', '2026-01-01', '2026-01-01'),
                    ('sess', 'a1', 'p', 'session', 'S', 's', '2026-01-01', '2026-01-01'),
                    ('t', 'a1', 'sess', 'topic', 'Rust入門', 's', '2026-01-02', '2026-01-02');
             DROP TABLE IF EXISTS memory_category_members;",
        )
        .unwrap();

    // --- v21 相当の agent_nostr_config（owner_pubkey 無し）へ戻す（v22 を実際に走らせる）---
    conn.execute_batch("ALTER TABLE agent_nostr_config DROP COLUMN owner_pubkey")
        .unwrap();
    assert!(!column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());

    // --- v20 相当の impressions（旧一意制約）へ戻し、user_version を 20 に落とす ---
    // 同一 (agent_id, target_id) を別セッションで 2 行 + 別 target を 1 行（v21 の統合を確認）。
    seed_legacy_impressions(
        &conn,
        "('i1','a1','s1','u1','U','','','','中立','',0,'2026-01-01','2026-01-02'),\
             ('i2','a1','s2','u1','U','','','','中立','',0,'2026-01-03','2026-01-01'),\
             ('i3','a1','s1','u2','V','','','','中立','',0,'2026-01-01','2026-01-01')",
    );
    assert_eq!(schema_version(&conn).unwrap(), 20);

    // 一気通貫で v20 → 最新へ。
    run_migrations(&conn, MIGRATIONS).expect("v20 -> latest chain");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // (1) memory_index: 時系列ツリーは 1 件も失われない。
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM memory_index_nodes ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec!["p", "r", "sess", "t"]);
    // CHECK は広がり、category が実際に保存できる（OR IGNORE の沈黙が起きない）。
    conn.execute(
            "INSERT INTO memory_index_nodes (id, agent_id, node_type, source_type, title, summary, created_at, updated_at)
             VALUES ('cat1', 'a1', 'category', 'category', 'X', '', '2026-02-01', '2026-02-01')",
            [],
        )
        .expect("移行後は category を登録できる");
    assert!(table_exists(&conn, "memory_category_members").unwrap());

    // (2) v21 が適用され、impressions が (agent_id, target_id) スコープへ畳まれている。
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM impressions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2, "同一 (agent_id, target_id) は 1 行へ統合される");
    let u1_created: String = conn
        .query_row(
            "SELECT created_at FROM impressions WHERE agent_id='a1' AND target_id='u1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        u1_created, "2026-01-01",
        "created_at は統合対象の最小値を引き継ぐ"
    );

    // (3) v22 が適用され、owner_pubkey 列が復活している。
    assert!(column_exists(&conn, "agent_nostr_config", "owner_pubkey").unwrap());
}

/// G. v1 DB が v2 マイグレーションでタスク台帳テーブルを獲得する。
#[test]
fn task_ledger_migration_upgrades_v1_db() {
    let conn = crate::init_memory().expect("init");
    // v1 相当の既存DBを模す: タスク台帳を落として version 1 に戻す。
    conn.execute_batch("DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1")
        .unwrap();
    assert!(!table_exists(&conn, "task_ledger").unwrap());

    initialize(&conn).expect("upgrade v1 -> latest");
    assert!(table_exists(&conn, "task_ledger").unwrap());
    assert!(table_exists(&conn, "task_progress").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

/// v3: display_name 列の付与。v2 相当 DB（列なし）からのアップグレードと、
/// 新規 DB（SCHEMA_SQL 由来で列あり）での冪等性の両方を確認する。
#[test]
fn display_name_migration_upgrades_v2_db() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来。表名は v17 で改名済みの新しい方）
    assert!(column_exists(&conn, "trusted_users", "display_name").unwrap());

    // v2 相当の既存 DB を模す: 旧表名・列なしのテーブルに作り直して version 2 に戻す
    conn.execute_batch(
        "DROP TABLE trusted_users;
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               UNIQUE (discord_user_id, agent_id)
             );
             PRAGMA user_version = 2",
    )
    .unwrap();
    assert!(!column_exists(&conn, "trusted_discord_users", "display_name").unwrap());

    initialize(&conn).expect("upgrade v2 -> latest");
    // v3 で列が付き、v17 で表ごと改名される
    assert!(column_exists(&conn, "trusted_users", "display_name").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 再実行しても冪等
    initialize(&conn).expect("idempotent");
}

/// v16: `trusted_discord_users.platform` の付与（#214）。
///
/// 列追加のみで、**既存行は従来の経路（`discord`）として生きる**こと
/// （既存の信頼済みユーザーが移行で一斉に権限を失わない）。
#[test]
fn platform_migration_keeps_existing_rows_on_discord() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB には既に列がある（SCHEMA_SQL 由来。表名は v17 で改名済みの新しい方）
    assert!(column_exists(&conn, "trusted_users", "platform").unwrap());

    // v15 相当の既存 DB を模す: 旧表名・platform 列なしのテーブルに作り直し、行を 1 件
    // 入れて version 15 へ戻す。
    conn.execute_batch(
        "DROP TABLE trusted_users;
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               UNIQUE (discord_user_id, agent_id)
             );
             INSERT INTO trusted_discord_users
               (id, discord_user_id, agent_id, permission, created_by, created_at, display_name)
               VALUES ('old-1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B');
             PRAGMA user_version = 15",
    )
    .unwrap();
    assert!(!column_exists(&conn, "trusted_discord_users", "platform").unwrap());

    initialize(&conn).expect("upgrade v15 -> latest");
    // v16 で列が付き、v17 で表ごと改名される
    assert!(column_exists(&conn, "trusted_users", "platform").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 既存行は残り、従来の経路として引ける（他の列も失われていない）。
    let (platform, permission): (String, String) = conn
        .query_row(
            "SELECT platform, permission FROM trusted_users WHERE id = 'old-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(platform, "discord");
    // 権限は失われない。表記だけ v18 (#234) がケバブケースへ移す。
    assert_eq!(permission, "co-agent");

    // 再実行しても冪等
    initialize(&conn).expect("idempotent");
}

/// v17: `trusted_discord_users` → `trusted_users` / `discord_user_id` → `user_id`（#159）。
///
/// 改名のみで、**既存行は 1 件も失われず、値もそのまま**であること。
/// 一意制約と索引も新しい名前で生き続けること。
#[test]
fn trusted_users_rename_migration_preserves_rows() {
    let conn = crate::init_memory().expect("init");
    // 新規 DB は SCHEMA_SQL 由来で既に新しい名前
    assert!(table_exists(&conn, "trusted_users").unwrap());
    assert!(!table_exists(&conn, "trusted_discord_users").unwrap());

    // v16 相当の既存 DB を模す: 旧表名・旧列名で作り直し、行を 2 件入れて version 16 へ戻す。
    conn.execute_batch(
            "DROP TABLE trusted_users;
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               platform TEXT NOT NULL DEFAULT 'discord',
               UNIQUE (discord_user_id, agent_id)
             );
             CREATE INDEX idx_trusted_discord_users_agent ON trusted_discord_users(agent_id);
             INSERT INTO trusted_discord_users
               (id, discord_user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('old-1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B', 'discord'),
                      ('old-2', '43', 'a1', 'user', 'owner', '2026-01-02', '', 'discord');
             PRAGMA user_version = 16",
        )
        .unwrap();

    initialize(&conn).expect("upgrade v16 -> v17");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 表と列が改名され、旧名は消えている
    assert!(table_exists(&conn, "trusted_users").unwrap());
    assert!(!table_exists(&conn, "trusted_discord_users").unwrap());
    assert!(column_exists(&conn, "trusted_users", "user_id").unwrap());
    assert!(!column_exists(&conn, "trusted_users", "discord_user_id").unwrap());

    // 行は 2 件とも残り、値もそのまま
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
    let (user_id, permission, display_name, platform): (String, String, String, String) = conn
            .query_row(
                "SELECT user_id, permission, display_name, platform FROM trusted_users WHERE id = 'old-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
    assert_eq!(user_id, "42");
    // 改名では値は動かない。表記が変わるのは後続の v18 (#234)。
    assert_eq!(permission, "co-agent");
    assert_eq!(display_name, "Crab B");
    assert_eq!(platform, "discord");

    // 一意制約は改名後も効いている（列名だけが変わった）
    assert!(conn
            .execute(
                "INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at, display_name, platform) \
                 VALUES ('dup', '42', 'a1', 'user', 'owner', '2026-01-03', '', 'discord')",
                [],
            )
            .is_err());

    // 索引も新しい名前で貼り直されている
    let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_trusted_users_agent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(idx, 1);
    let old_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_trusted_discord_users_agent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(old_idx, 0);

    // 再実行しても冪等
    initialize(&conn).expect("idempotent");
}

/// v17 の #479 分岐が、**既にデータのある `trusted_users` を絶対に壊さない**こと。
///
/// baseline 経路（user_version<1）では SCHEMA_SQL が空の `trusted_users` を先に作るため、
/// #479 の修正で「新表が空なら DROP して旧表を RENAME」する分岐を足した。ここで検証するのは
/// その分岐の安全条件: **新表にデータがある並存状態**（通常経路では起きないが冪等・保全のため
/// 想定する）では、新表を DROP せず・旧表にも触れないこと。v17 の `up` を直接呼んで固定する。
#[test]
fn v17_never_destroys_populated_trusted_users() {
    let conn = Connection::open_in_memory().expect("open");
    // 新表（最終 shape）にデータを 1 件、旧表にも別データを 1 件置いた並存状態を作る。
    conn.execute_batch(
            "CREATE TABLE trusted_users (
               id TEXT PRIMARY KEY,
               user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               platform TEXT NOT NULL DEFAULT 'discord',
               UNIQUE (user_id, agent_id)
             );
             INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('new-1', '100', 'a1', 'co-agent', 'owner', '2026-05-01', 'Keep Me', 'web');
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               platform TEXT NOT NULL DEFAULT 'discord',
               UNIQUE (discord_user_id, agent_id)
             );
             INSERT INTO trusted_discord_users (id, discord_user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('old-1', '42', 'a1', 'user', 'owner', '2026-01-01', 'Stale', 'discord');",
        )
        .unwrap();

    // v17 の up を直接適用（並存状態に対する分岐だけを検証する）。
    let v17 = MIGRATIONS
        .iter()
        .find(|m| m.version == 17)
        .expect("v17 migration exists");
    (v17.up)(&conn).expect("v17 up");

    // 新表のデータはそのまま（DROP されていない・置き換わっていない）。
    let (n, display): (i64, String) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM trusted_users), \
                        (SELECT display_name FROM trusted_users WHERE id = 'new-1')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(n, 1, "既存データのある trusted_users は 1 行のまま");
    assert_eq!(
        display, "Keep Me",
        "既存行が旧表のデータで上書きされないこと"
    );
    // 旧表は触られず残る（実データを勝手に消さない）。ここでの並存は通常経路では起きないが、
    // 起きても「新表のデータを守る」方を優先する。
    assert!(
        table_exists(&conn, "trusted_discord_users").unwrap(),
        "新表にデータがある場合、旧表は破棄されない"
    );

    // 冪等: もう一度流しても新表のデータは不変。
    (v17.up)(&conn).expect("v17 up idempotent");
    let n2: i64 = conn
        .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n2, 1);
}

/// v18: `trusted_users.permission` の表記統一（#234）。
///
/// **移行前後で権限の判定結果が変わらない**こと（同じ人が同じ権限のまま、表記だけ
/// ケバブケースになる）。行は 1 件も増減しない。旧い綴りのうち**判定が完全一致で
/// 見ていたもの（`co_agent`）だけ**を移し、それ以外は触らない（権限を増やさない）。
#[test]
fn permission_spelling_migration_rewrites_rows_without_changing_who_is_a_co_agent() {
    use crate::queries::TrustedUserPermission;

    let conn = crate::init_memory().expect("init");
    // v17 相当の既存 DB を模す: 旧表記の行を含めて 4 件入れ、version 17 へ戻す。
    conn.execute_batch(
        "DELETE FROM trusted_users;
             INSERT INTO trusted_users
               (id, user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('r1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B', 'discord'),
                      ('r2', '43', 'a1', 'user',     'owner', '2026-01-02', '',       'discord'),
                      ('r3', '44', 'a1', 'owner',    'owner', '2026-01-03', '',       'discord'),
                      ('r4', '45', 'a1', 'coagent',  'owner', '2026-01-04', 'Typo',   'discord');
             PRAGMA user_version = 17",
    )
    .unwrap();

    // 移行前の判定（旧い読み出し = permission == 'co_agent' の完全一致）。
    let judged_before: Vec<(String, bool)> = conn
        .prepare("SELECT id, permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)? == "co_agent"))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    initialize(&conn).expect("upgrade v17 -> v18");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 行は増減しない。
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4);

    // 移行後の判定（新しい読み出し = 列挙型）。移行前と一致すること。
    let judged_after: Vec<(String, bool)> = conn
        .prepare("SELECT id, permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                TrustedUserPermission::from_db_str(&r.get::<_, String>(1)?)
                    == TrustedUserPermission::CoAgent,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(judged_before, judged_after);
    assert_eq!(
        judged_after,
        vec![
            ("r1".to_string(), true),
            ("r2".to_string(), false),
            ("r3".to_string(), false),
            ("r4".to_string(), false),
        ]
    );

    // 表記は移り、触らない行はそのまま（`coagent` は判定が拾っていなかったので拾わない）。
    let spellings: Vec<String> = conn
        .prepare("SELECT permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(spellings, vec!["co-agent", "user", "owner", "coagent"]);

    // 再実行しても冪等（2 回目は 0 行更新）。
    initialize(&conn).expect("idempotent");
    let after: Vec<String> = conn
        .prepare("SELECT permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(after, spellings);
}

/// v20: エージェント単位ハートビート設定の表が増えるだけで、既存の
/// チャンネル単位設定（`discord_channel_config`）の行は 1 つも動かない（#247）。
#[test]
fn agent_heartbeat_config_migration_v20_adds_table_without_touching_channel_config() {
    let conn = crate::init_memory().expect("init");
    // v19 相当の既存 DB を模す: 新表を落として version を 19 へ戻す。
    // チャンネル単位設定には行を 1 件入れておき、移行で動かないことを見る。
    // channel_id/guild_id は数値の Discord snowflake にする（v37 の session_id 形式検証
    // `discord-{agent}-{digits}-{digits}` を通すため。実データも常に数値）。
    conn.execute_batch(
        "DROP TABLE IF EXISTS agent_heartbeat_config;
             INSERT INTO discord_channel_config
               (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted,
                heartbeat_enabled, heartbeat_interval_secs, updated_at)
               VALUES ('2001', 'a1', '1001', 'general', 1, 1, 1, 1, 60, '2026-01-01');
             PRAGMA user_version = 19",
    )
    .unwrap();
    assert!(!table_exists(&conn, "agent_heartbeat_config").unwrap());

    initialize(&conn).expect("upgrade v19 -> v20");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(table_exists(&conn, "agent_heartbeat_config").unwrap());

    // 新表は空で始まる（既定は「設定なし」＝無効）。
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_heartbeat_config", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0);

    // チャンネル単位設定は 1 バイトも変わらない（段階 3 まで残す）。
    let (hb_enabled, hb_interval): (i64, Option<i64>) = conn
        .query_row(
            "SELECT heartbeat_enabled, heartbeat_interval_secs FROM discord_channel_config
                 WHERE channel_id = '2001' AND agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((hb_enabled, hb_interval), (1, Some(60)));

    // 行を入れてから再実行しても冪等（CREATE TABLE IF NOT EXISTS で消えない）。
    conn.execute_batch(
        "INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at)
             VALUES ('a1', 1, 900, '2026-01-02')",
    )
    .unwrap();
    initialize(&conn).expect("idempotent");
    let kept: i64 = conn
        .query_row(
            "SELECT interval_secs FROM agent_heartbeat_config WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 900);
}

/// v20 相当（旧一意制約）の `impressions` を作り直し、行を入れて version 20 へ戻す。
fn seed_legacy_impressions(conn: &Connection, rows: &str) {
    conn.execute_batch(&format!(
        "DROP TABLE impressions;
             CREATE TABLE impressions (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                target_name TEXT NOT NULL,
                personality TEXT DEFAULT '',
                communication_style TEXT DEFAULT '',
                recent_behavior TEXT DEFAULT '',
                agreement TEXT DEFAULT '中立',
                notes TEXT DEFAULT '',
                last_updated_turn INTEGER DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(agent_id, session_id, target_id)
             );
             CREATE INDEX idx_impressions_session ON impressions(agent_id, session_id);
             INSERT INTO impressions
               (id, agent_id, session_id, target_id, target_name, personality,
                communication_style, recent_behavior, agreement, notes,
                last_updated_turn, created_at, updated_at)
               VALUES {rows};
             PRAGMA user_version = 20"
    ))
    .unwrap();
}

/// v21: 人物像を agent スコープへ（#314）。
///
/// **重複が無い実データでは 1 行も失われない**こと。一意制約が
/// `(agent_id, target_id)` に付け替わり、同じ相手なら別セッションからでも
/// 同じ行を更新するようになること。
#[test]
fn impressions_agent_scope_migration_v21_preserves_rows() {
    let conn = crate::init_memory().expect("init");
    seed_legacy_impressions(
            &conn,
            "('i1', 'a1', 's-discord', 'u1', 'Alice', 'p1', '', '', '中立', '', 0, '2026-01-01', '2026-01-01'),
             ('i2', 'a1', 's-discord', 'u2', 'Bob',   'p2', '', '', '中立', '', 0, '2026-01-02', '2026-01-02'),
             ('i3', 'a1', 's-nostr',   'u3', 'Carol', 'p3', '', '', '中立', '', 0, '2026-01-03', '2026-01-03'),
             ('i4', 'a2', 's-discord', 'u1', 'Alice', 'p4', '', '', '中立', '', 0, '2026-01-04', '2026-01-04')",
        );

    initialize(&conn).expect("upgrade v20 -> v21");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 4 行すべて残る（重複が無いので統合は起きない）。
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM impressions ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec!["i1", "i2", "i3", "i4"]);
    // 中身も session_id も動かない。
    let (session_id, personality): (String, String) = conn
        .query_row(
            "SELECT session_id, personality FROM impressions WHERE id = 'i3'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_id, "s-nostr");
    assert_eq!(personality, "p3");

    // 新しい一意制約: 同じ (agent_id, target_id) は別セッションでも 1 行。
    assert!(conn
            .execute(
                "INSERT INTO impressions (id, agent_id, session_id, target_id, target_name, created_at, updated_at) \
                 VALUES ('dup', 'a1', 's-other', 'u1', 'Alice', '2026-02-01', '2026-02-01')",
                [],
            )
            .is_err());
    // エージェントが違えば別の行（agent スコープは維持）。
    assert_eq!(
        crate::queries::get_impressions(&conn, "a1").unwrap().len(),
        3
    );
    assert_eq!(
        crate::queries::get_impressions(&conn, "a2").unwrap().len(),
        1
    );

    // 再実行しても冪等（再構築は走らない）。
    initialize(&conn).expect("idempotent");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM impressions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4);
}

/// v21: 同じ相手が複数セッションに散っている（＝旧スキーマで分断していた）DB でも
/// 壊れない。統合方針は「`updated_at` が最新の行を残す・`created_at` は最古を継ぐ」。
#[test]
fn impressions_agent_scope_migration_v21_merges_duplicates() {
    let conn = crate::init_memory().expect("init");
    seed_legacy_impressions(
            &conn,
            "('old', 'a1', 's-discord', 'u1', 'Alice', 'old-note', '', '', '中立', '', 1, '2026-01-01', '2026-01-01'),
             ('new', 'a1', 's-nostr',   'u1', 'Alice2','new-note', '', '', '中立', '', 2, '2026-03-01', '2026-03-01'),
             ('keep','a1', 's-discord', 'u2', 'Bob',   'bob-note', '', '', '中立', '', 0, '2026-02-01', '2026-02-01')",
        );

    initialize(&conn).expect("upgrade v20 -> v21");

    let rows = crate::queries::get_impressions(&conn, "a1").unwrap();
    assert_eq!(rows.len(), 2, "u1 の 2 行が 1 行に統合される");

    let merged = crate::queries::get_impression(&conn, "a1", "u1")
        .unwrap()
        .expect("merged row");
    assert_eq!(merged.id, "new", "updated_at が新しい行が残る");
    assert_eq!(merged.personality, "new-note");
    assert_eq!(merged.target_name, "Alice2");
    assert_eq!(merged.session_id, "s-nostr");
    assert_eq!(merged.last_updated_turn, 2);
    // 「いつからの知り合いか」は統合対象の最古を引き継ぐ。
    let created_at: String = conn
        .query_row(
            "SELECT created_at FROM impressions WHERE target_id = 'u1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created_at, "2026-01-01");

    // 重複していない相手は影響を受けない。
    let bob = crate::queries::get_impression(&conn, "a1", "u2")
        .unwrap()
        .expect("bob");
    assert_eq!(bob.id, "keep");
    assert_eq!(bob.personality, "bob-note");
}

/// v21: 旧制約の判定は `sqlite_master.sql` の文字列ではなく実際の索引の列を見る。
///
/// 同じ旧制約でも表記（空白・列の並べ方）は DB を作ったバイナリによって揺れる。
/// 表記が違っても再構築が走り、`upsert_impression` の `ON CONFLICT(agent_id,
/// target_id)` が通ること。
#[test]
fn impressions_agent_scope_migration_v21_detects_reformatted_legacy_schema() {
    let conn = crate::init_memory().expect("init");
    // 旧制約と等価だが `sql LIKE '%UNIQUE(agent_id, session_id, target_id)%'` には
    // 引っ掛からない表記（`UNIQUE (` と余分な空白）。
    conn.execute_batch(
        "DROP TABLE impressions;
             CREATE TABLE impressions (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                target_name TEXT NOT NULL,
                personality TEXT DEFAULT '',
                communication_style TEXT DEFAULT '',
                recent_behavior TEXT DEFAULT '',
                agreement TEXT DEFAULT '中立',
                notes TEXT DEFAULT '',
                last_updated_turn INTEGER DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE  (agent_id,session_id,target_id)
             );
             PRAGMA user_version = 20",
    )
    .unwrap();

    initialize(&conn).expect("upgrade v20 -> v21");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 新制約になっているので upsert（ON CONFLICT(agent_id, target_id)）が通る。
    let row = crate::queries::ImpressionRow {
        id: "i1".to_string(),
        agent_id: "a1".to_string(),
        session_id: "s-discord".to_string(),
        target_id: "u1".to_string(),
        target_name: "Alice".to_string(),
        personality: "p1".to_string(),
        communication_style: String::new(),
        recent_behavior: String::new(),
        agreement: "中立".to_string(),
        notes: String::new(),
        last_updated_turn: 0,
    };
    crate::queries::upsert_impression(&conn, &row).expect("upsert on new constraint");
    let mut again = row.clone();
    again.session_id = "s-nostr".to_string();
    again.personality = "p2".to_string();
    crate::queries::upsert_impression(&conn, &again).expect("upsert across sessions");
    assert_eq!(
        crate::queries::get_impressions(&conn, "a1").unwrap().len(),
        1,
        "別セッションからでも同じ 1 行を更新する"
    );
}

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
        .execute_batch("DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1")
        .unwrap();
    initialize(&migrated).expect("re-migrate");

    assert_eq!(dump(&fresh), dump(&migrated));
    assert_eq!(dump(&fresh).len(), 5, "expected 2 tables + 3 indexes");
}

/// F. ダウングレードガード: DB が既知の最新版より新しい場合はエラーにする。
#[test]
fn downgrade_is_rejected() {
    let conn = crate::init_memory().expect("init");
    conn.execute_batch("PRAGMA user_version = 999").unwrap();
    let fake = &[Migration {
        version: 2,
        description: "v2",
        up: create_marker,
    }];
    let result = run_migrations(&conn, fake);
    assert!(result.is_err(), "newer-than-supported DB must be rejected");
    assert!(!table_exists(&conn, "test_marker").unwrap());
}

// ── v37: セッション一本化スキーマ + 移行（#439 × #455 × #456・PR1）──────────

/// v37 適用前（user_version=36）の DB を模す: 新表 2 つを落として版を 36 へ戻す。
/// 旧表（agent_heartbeat_config / discord_channel_config / agent_nostr_config）は
/// baseline/番号付き migration で既に存在するので、そこへ fixture を積む。
fn setup_pre_v37(conn: &Connection) {
    conn.execute_batch(
        "DROP TABLE IF EXISTS session_heartbeat_config;
             DROP TABLE IF EXISTS agent_schedules;
             PRAGMA user_version = 36;",
    )
    .unwrap();
    assert_eq!(schema_version(conn).unwrap(), 36);
}

fn shc_row(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> crate::queries::SessionHeartbeatConfigRow {
    crate::queries::get_session_heartbeat_config(conn, agent_id, session_id)
        .unwrap()
        .unwrap_or_else(|| panic!("expected session row {agent_id} / {session_id}"))
}

/// v37 backfill が **現状の発火挙動を保存**することを検証する（設計 §4.2 の step1/step2/
/// step3・正規化）。step3 の global 展開は **enabled=0**（発火を増やさない）で作る。
#[test]
fn v37_backfill_preserves_firing_and_normalizes() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v37(&conn);

    // 旧設定の fixture。
    //  A: opt-in 済み(enabled=1) かつ Nostr 有り  → nostr-A enabled=1、Discord 抑止(0)
    //  B: opt-in 済み かつ Nostr 無し             → nostr 作らない（出口なし・沈黙）
    //  C: 未 opt-in（行はあるが disabled）        → Discord enabled=1
    //  D: agent_heartbeat_config に行なし=未 opt-in、guild/channel が引用符付き → 正規化
    //  E: heartbeat_enabled=0 の Discord 行       → 移行しない
    //  global('' 行, ch205, whitelisted=1)        → step3 で A〜E に enabled=0 展開
    conn.execute_batch(
            "INSERT INTO agents (agent_id, name, persona_name) VALUES
                ('A','A','A'),('B','B','B'),('C','C','C'),('D','D','D'),('E','E','E');
             INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at) VALUES
                ('A', 1, 18000, '2026-01-01'),
                ('B', 1, 1200,  '2026-01-01'),
                ('C', 0, 10800, '2026-01-01');
             INSERT INTO agent_nostr_config (agent_id, secret_key, relays_json, filter_json, enabled, updated_at) VALUES
                ('A', 'nsecA', '[]', '{}', 1, '2026-01-01');
             INSERT INTO discord_channel_config
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions, updated_at) VALUES
                ('201', 'A', '100', '', 1, 1, 1, 1, NULL,  '', '2026-01-01'),
                ('202', 'C', '100', '', 1, 1, 1, 1, 10800, '', '2026-01-01'),
                ('\"222\"', 'D', '\"111\"', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('204', 'E', '100', '', 1, 1, 1, 0, NULL,  '', '2026-01-01'),
                ('205', '',  '100', '', 1, 1, 1, 1, NULL,  '', '2026-01-01');",
        )
        .unwrap();

    initialize(&conn).expect("apply v37");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    // v38（#455 の agent_schedules 語彙整合）が最新。v38 は session_heartbeat_config を
    // 触らないので、下の v37 backfill 検証（発火集合・正規化）はそのまま成立する。
    assert_eq!(latest_version(), 38, "v38 が最新版であること");

    // 期待: 9 行 = step1(nostr-A) 1 + step2(A/201=0, C/202=1, D/222=1) 3 +
    //             step3(A,B,C,D,E の ch205 展開・全 enabled=0) 5。
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM session_heartbeat_config", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(total, 9, "backfill 行数");

    // step1: Nostr セッション（G 非依存で発火していたので enabled=1・anchor 打つ）。
    let a_nostr = shc_row(&conn, "A", "nostr-A");
    assert!(a_nostr.enabled);
    assert_eq!(
        a_nostr.interval_secs,
        Some(18000),
        "意図した interval を保持"
    );
    assert!(a_nostr.anchor_at.is_some(), "enabled 行は anchor を打つ");
    assert!(a_nostr.last_fired_at.is_none());

    // step2: A の Discord 行は opt-in 抑止を enabled=0 として保存（anchor は NULL）。
    let a_disc = shc_row(&conn, "A", "discord-A-100-201");
    assert!(
        !a_disc.enabled,
        "opt-in 済みの Discord 発火は現状沈黙＝enabled=0 で保存"
    );
    assert!(a_disc.anchor_at.is_none(), "enabled=0 は anchor を打たない");

    // step2: C（未 opt-in）は enabled=1・interval 保持・anchor 打つ。
    let c_disc = shc_row(&conn, "C", "discord-C-100-202");
    assert!(c_disc.enabled);
    assert_eq!(c_disc.interval_secs, Some(10800));
    assert!(c_disc.anchor_at.is_some());

    // step2 正規化(B3): guild/channel の引用符が除去され discord-D-111-222 になる。
    let d_disc = shc_row(&conn, "D", "discord-D-111-222");
    assert!(d_disc.enabled);

    // B: Nostr 無しの opt-in は出口なし＝セッションを作らない（#456 決定3）。
    assert!(
        crate::queries::get_session_heartbeat_config(&conn, "B", "nostr-B")
            .unwrap()
            .is_none(),
        "Discord 専用 opt-in は Nostr セッションを作らない"
    );
    // E: heartbeat_enabled=0 は移行しない。
    assert!(
        crate::queries::get_session_heartbeat_config(&conn, "E", "discord-E-100-204")
            .unwrap()
            .is_none()
    );
    // step3: global 行(ch205)を A〜E へ **enabled=0** で展開（発火は増やさない・統括裁定）。
    //  B は他に行が無いエージェントだが、global 既定の到達先として enabled=0 行が残る。
    let b_205 = shc_row(&conn, "B", "discord-B-100-205");
    assert!(!b_205.enabled, "global 展開は enabled=0（発火させない）");
    assert!(b_205.anchor_at.is_none());
    let a_205 = shc_row(&conn, "A", "discord-A-100-205");
    assert!(!a_205.enabled);
    // step3 が発火（enabled=1）を 1 件も増やさないこと。
    let expanded_enabled: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_heartbeat_config WHERE session_id LIKE '%-100-205' AND enabled = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(expanded_enabled, 0, "global 展開で発火を増やさない");
    // agent_id='' のセッションは決して作らない（global 行そのものは session にしない）。
    let global_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session_heartbeat_config WHERE agent_id = ''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(global_rows, 0, "agent_id='' のセッションは作らない");
}

/// v37 backfill が壊れた session_id（非数値 channel）を作ったら `up()` 内検証で `Err` を
/// 返し、per-migration トランザクションで**アトミックにロールバック**する（設計 §4.2.4）。
#[test]
fn v37_backfill_rejects_malformed_session_id_and_rolls_back() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v37(&conn);

    // 未 opt-in agent X の Discord 行で channel_id が非数値 → discord-X-100-abc（不正形式）。
    conn.execute_batch(
            "INSERT INTO discord_channel_config
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions, updated_at) VALUES
                ('abc', 'X', '100', '', 1, 1, 1, 1, NULL, '', '2026-01-01');",
        )
        .unwrap();

    let result = initialize(&conn);
    assert!(result.is_err(), "壊れた session_id は fail-closed で Err");
    // ロールバック: 版は 36 のまま、新表も作られていない（CREATE ごと巻き戻る）。
    assert_eq!(schema_version(&conn).unwrap(), 36, "版トラップは起きない");
    assert!(
        !table_exists(&conn, "session_heartbeat_config").unwrap(),
        "Err なら CREATE TABLE ごとロールバックされる"
    );
}

/// SCHEMA_SQL（新規 DB）と v37 migration（既存 DB）が **同じ形**の新表を作ることを、
/// sqlite_master の SQL 文字列で比較して固定する（定数と SCHEMA_SQL の drift を検出）。
/// 新規 DB（SCHEMA_SQL 経由）と既存 DB（v37→v38 migration 経由）が **同じ最終形**の
/// スキーマへ収束することを固定する（定数と SCHEMA_SQL の drift を検出）。
///
/// v38 で `agent_schedules` は v37 の CREATE を **ALTER で書き換える**ため、
/// `sqlite_master.sql` の生テキストは両経路で一致しない（fresh=SCHEMA_SQL の手書き、
/// migrated=旧定数を ALTER が書き換えたもの）。→ **agent_schedules は列構造
/// （pragma_table_info）で比較**する（空白非依存・ALTER 安全・構造契約そのもの）。
/// v38 が触らない `session_heartbeat_config` と index は従来どおり生 SQL で比較する。
#[test]
fn schedule_schema_parity_fresh_vs_migrated() {
    // 生 SQL 比較対象（v38 が触らない = 両経路で CREATE テキストが同一）。
    let raw_sql = |conn: &Connection| -> Vec<String> {
        conn.prepare(
            "SELECT sql FROM sqlite_master
                 WHERE name IN ('session_heartbeat_config', 'idx_agent_schedules_agent')
                   AND sql IS NOT NULL
                 ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    // 列構造比較（name, type, notnull, dflt_value, pk）。ALTER 経由でも一致する契約。
    let cols = |conn: &Connection,
                table: &str|
     -> Vec<(String, String, i64, Option<String>, i64)> {
        conn.prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1) ORDER BY cid")
                .unwrap()
                .query_map([table], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
    };

    // 新規 DB: SCHEMA_SQL 由来。
    let fresh = crate::init_memory().expect("fresh");
    // 既存 DB: 新表を落として版を 36 へ戻し、v37→v38 migration で作り直す。
    let migrated = crate::init_memory().expect("migrated");
    setup_pre_v37(&migrated);
    initialize(&migrated).expect("re-migrate v37+v38");

    assert_eq!(raw_sql(&fresh), raw_sql(&migrated));
    assert_eq!(raw_sql(&fresh).len(), 2, "1 table(shc) + 1 index");
    assert_eq!(
        cols(&fresh, "agent_schedules"),
        cols(&migrated, "agent_schedules"),
        "agent_schedules の列構造は新規/既存で一致（v38 収束）"
    );
    // 語彙が heartbeat へ揃い、キャッシュ列が消えたことを両経路で固定する。
    let names: Vec<String> = cols(&fresh, "agent_schedules")
        .into_iter()
        .map(|c| c.0)
        .collect();
    assert!(
        names.contains(&"last_fired_at".to_string()),
        "last_fired_at に揃っている"
    );
    assert!(
        !names.contains(&"last_run_at".to_string()),
        "旧 last_run_at は消えている"
    );
    assert!(
        !names.contains(&"next_run_at".to_string()),
        "next_run_at キャッシュ列は撤去されている（照会時算出）"
    );
}

/// v38 が **非破壊**（RENAME は行データを保存し、DROP は next_run_at だけを落とす）で
/// あることを、v37 の旧列に値を入れた行が v38 後も `last_fired_at` に生き残ることで固定する。
/// DROP TABLE 方式へ退行するとこのテストが落ちる（0 行前提でも安全側＝保存を守る）。
#[test]
fn v38_is_non_destructive_and_preserves_last_fired() {
    let conn = crate::init_memory().expect("db");
    // v37 の旧スキーマ（last_run_at / next_run_at）まで巻き戻す。
    setup_pre_v37(&conn);
    // v37 だけ適用した状態を作るため、v38 を含まない一時 MIGRATIONS で v37 まで進める。
    let up_to_v37: Vec<Migration> = MIGRATIONS
        .iter()
        .filter(|m| m.version <= 37)
        .map(|m| Migration {
            version: m.version,
            description: m.description,
            up: m.up,
        })
        .collect();
    run_migrations(&conn, &up_to_v37).expect("apply through v37");
    assert!(column_exists(&conn, "agent_schedules", "last_run_at").unwrap());
    assert!(column_exists(&conn, "agent_schedules", "next_run_at").unwrap());

    // 旧列に値を持つ行を入れる（万一本番に行があっても保存されることの代理検証）。
    conn.execute(
            "INSERT INTO agent_schedules
                (agent_id, session_id, cron_expr, timezone, message, enabled, anchor_at, last_run_at, next_run_at, created_at, updated_at)
             VALUES ('a1','nostr-a1','0 7 * * *','Asia/Tokyo','morning',1,'2026-01-01T00:00:00Z','2026-08-09T07:00:00+09:00','2026-08-10T07:00:00+09:00','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

    // v38 を適用。
    run_migrations(&conn, MIGRATIONS).expect("apply v38");

    // RENAME で last_fired_at に値が保存され、next_run_at 列は消えている。
    assert!(!column_exists(&conn, "agent_schedules", "last_run_at").unwrap());
    assert!(column_exists(&conn, "agent_schedules", "last_fired_at").unwrap());
    assert!(!column_exists(&conn, "agent_schedules", "next_run_at").unwrap());
    let preserved: String = conn
        .query_row(
            "SELECT last_fired_at FROM agent_schedules WHERE agent_id='a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        preserved, "2026-08-09T07:00:00+09:00",
        "RENAME は last_run_at の値を last_fired_at に保存する（非破壊）"
    );
}

#[test]
fn norm_discord_id_strips_quotes_and_whitespace() {
    assert_eq!(
        norm_discord_id("\"1465697209541726362\""),
        "1465697209541726362"
    );
    assert_eq!(norm_discord_id("  123 456 "), "123456");
    assert_eq!(norm_discord_id("123\t\n"), "123");
    assert_eq!(norm_discord_id("123"), "123");
}

#[test]
fn session_id_is_valid_handles_uuid_agent_and_fail_closed() {
    let agent = "6b79ac3a-7f17-4618-a827-5bda992a3698"; // ハイフンを含む UUID
    assert!(session_id_is_valid(&format!("nostr-{agent}"), agent));
    assert!(session_id_is_valid(
        &format!("discord-{agent}-100-201"),
        agent
    ));
    // 非数値 guild/channel は fail-closed。
    assert!(!session_id_is_valid(
        &format!("discord-{agent}-100-abc"),
        agent
    ));
    assert!(!session_id_is_valid(
        &format!("discord-{agent}-abc-201"),
        agent
    ));
    // 発火経路を持たない種別・未知接頭辞は fail-closed。
    assert!(!session_id_is_valid(&format!("web-{agent}"), agent));
    assert!(!session_id_is_valid(&format!("heartbeat-{agent}"), agent));
    // 別 agent の id で剥がそうとしても合致しない。
    assert!(!session_id_is_valid(
        &format!("nostr-{agent}"),
        "other-agent"
    ));
}

// ── 不変条件テスト（設計 §4.2 A2 / 受け入れ基準 B1）─────────────────────────
//
// 「移行が発火集合を変えない」を、**期待集合を手書きせず**に検証する。旧側は実コード
// 経路どおりに計算し、新側は移行後の enabled セッションから計算して**一致**を見る。
// `G ∈ {true, false}` でパラメタライズする（移行は G を焼き込まないので DB は 1 つで両方
// 計算できる）。
//
// **旧側の実発火の定義（実コードを正・統括指示で訂正済み）**: 現行の ChannelScoped 発火
// 経路（main.rs:494-590）は **whitelist ゲートも writable ゲートも適用しない**（発火先は
// `list_heartbeat_channels`＝`heartbeat_enabled=1` のみ・channel_config.rs:198）。よって
// 旧側実発火に `is_channel_whitelisted_for_agent` を含めてはいけない。旧発火は **HB ループ
// が立つエージェント**（config discord `agent_ids` ∪ opt-in）にのみ起こるため、loop 集合を
// 入力に取る。

const INV_DEFAULT_INTERVAL: u64 = 1800;
const INV_MIN_INTERVAL: u64 = 300;

/// runtime で Nostr が実際に鳴る条件を判定する。**`enabled = 1` を要求する**（存在だけの
/// COUNT にしない・F1）。runtime は enabled=1 の gateway だけ起動する（nostr_runner_impl.rs:94）
/// ため、移行の EXISTS 近似と**同じ判定式をテストが共有しない**ようにここで実効条件を使う。
fn agent_nostr_fires(conn: &Connection, agent_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM agent_nostr_config WHERE agent_id = ?1 AND enabled = 1",
        [agent_id],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// ChannelScoped の発火先 channel_id 群（実コード main.rs:326-372 の dedup を再現）。
/// `list_heartbeat_channels`（heartbeat_enabled=1）を当該 agent 向け（agent 固有 or global）に
/// 絞り、同一 channel_id では agent 固有行を global 行より優先して dedup する。
/// **whitelist ゲートは適用しない**（実コードの ChannelScoped 経路に存在しない）。
fn channelscoped_targets(conn: &Connection, agent_id: &str) -> std::collections::BTreeSet<String> {
    let all = crate::queries::list_heartbeat_channels(conn).unwrap();
    let mut selected: std::collections::HashMap<String, crate::queries::ChannelConfigRow> =
        std::collections::HashMap::new();
    for c in all {
        if !c.agent_id.is_empty() && c.agent_id != agent_id {
            continue;
        }
        match selected.get(&c.channel_id) {
            Some(existing) if !existing.agent_id.is_empty() && c.agent_id.is_empty() => continue,
            _ => {
                selected.insert(c.channel_id.clone(), c);
            }
        }
    }
    selected.into_keys().collect()
}

/// 旧システムが実際に外部へ届ける発火集合を、実コード経路どおりに計算する。
fn old_real_firing(
    conn: &Connection,
    g: bool,
    loop_agents: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for agent in loop_agents {
        let resolved = crate::queries::resolve_agent_heartbeat(
            conn,
            agent,
            INV_DEFAULT_INTERVAL,
            INV_MIN_INTERVAL,
        );
        // firing_plan（実 main.rs:169-183 の分岐を再現）。
        if resolved.enabled {
            // AgentScoped: 1 回発火。外部到達は Nostr gateway が**実際に鳴る**ときだけ
            // （Discord 専用・Nostr disabled は空 channel or 未起動 → 外部発火ゼロ）。
            if agent_nostr_fires(conn, agent) {
                out.insert((agent.clone(), "nostr".to_string()));
            }
        } else if g {
            // ChannelScoped: heartbeat_enabled=1 チャンネル（dedup）。whitelist ゲート無し。
            for ch in channelscoped_targets(conn, agent) {
                out.insert((agent.clone(), ch));
            }
        }
        // None（未 opt-in かつ G=false）: 発火なし。
    }
    out
}

/// 移行後に実際に外部へ届ける発火集合を、enabled=1 セッションから計算する。
/// `discord-` は G ゲート、`nostr-` は G 非依存。whitelist ゲートは現状経路に無いので掛けない。
fn new_real_firing(conn: &Connection, g: bool) -> std::collections::BTreeSet<(String, String)> {
    let mut out = std::collections::BTreeSet::new();
    for row in crate::queries::list_enabled_session_heartbeat_configs(conn).unwrap() {
        let agent = &row.agent_id;
        let sid = &row.session_id;
        if *sid == format!("nostr-{agent}") {
            out.insert((agent.clone(), "nostr".to_string()));
        } else if let Some(rest) = sid.strip_prefix(&format!("discord-{agent}-")) {
            if let Some((_guild, channel)) = rest.rsplit_once('-') {
                if g {
                    out.insert((agent.clone(), channel.to_string()));
                }
            }
        }
    }
    out
}

/// **不変条件**: 移行直後に発火するセッション集合＝移行前に実発火していた集合。
/// 旧側は実 precedence（resolve_agent_heartbeat + firing_plan + Nostr 到達 + ChannelScoped
/// dedup、whitelist ゲート無し + loop membership）で計算、新側は enabled セッションから計算。
/// `G ∈ {true, false}` の両方で一致することを見る。**期待集合は手書きしない。**
#[test]
fn v37_invariant_old_firing_equals_new_firing() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v37(&conn);

    // prod を模した fixture（channel/guild は clean numeric = 正規化と同値）。
    //  optn : opt-in + Nostr enabled=1（→ AgentScoped Nostr 発火）
    //  optnn: opt-in・Nostr 無し（→ AgentScoped だが外部到達なし）
    //  plain: 未 opt-in・loop に居る（→ ChannelScoped）。global ch304 にも**明示行**を持つ
    //         （＝global fallback 経由の発火を持ち込まない＝prod と同じ状況）
    //  noloop: 未 opt-in・loop に**居ない**（→ 発火しない。prod の e2e-test 相当）
    //  optn0: opt-in・Nostr 行はあるが **enabled=0**（→ runtime で鳴らない・F1 の probe）。
    //         移行が EXISTS 近似だと nostr セッションを enabled=1 で作り新側だけ発火＝不一致。
    //  optbad: hb enabled=1 だが **interval_secs=0**（resolve が enabled:false へ倒す・F2 の
    //         probe）。resolve 意味論では未 opt-in なので ChannelScoped で Discord 発火する。
    //         移行が raw enabled=1 だと opt-in 扱いして Discord を抑止・nostr を作り不一致。
    //         ※ optbad は global ch304 にも**明示行**を持たせる（plain と同様）。持たせないと
    //         loop エージェントが global fallback 経由で ch304 に発火する状況になり、それは
    //         移行が保存できない既知の限界（enabled=0 展開）＝設計どおりの不一致になるため。
    //         prod では loop エージェントは global fallback に依存していないので、それを模す。
    conn.execute_batch(
            "INSERT INTO agents (agent_id, name, persona_name) VALUES
                ('optn','optn','optn'),('optnn','optnn','optnn'),('plain','plain','plain'),
                ('noloop','noloop','noloop'),('optn0','optn0','optn0'),('optbad','optbad','optbad');
             INSERT INTO agent_heartbeat_config (agent_id, enabled, interval_secs, updated_at) VALUES
                ('optn', 1, 18000, '2026-01-01'),
                ('optnn',1, 1200,  '2026-01-01'),
                ('optn0',1, 5000,  '2026-01-01'),
                ('optbad',1, 0,    '2026-01-01');
             INSERT INTO agent_nostr_config (agent_id, secret_key, relays_json, filter_json, enabled, updated_at) VALUES
                ('optn',  'nsec', '[]', '{}', 1, '2026-01-01'),
                ('optn0', 'nsec', '[]', '{}', 0, '2026-01-01'),
                ('optbad','nsec', '[]', '{}', 1, '2026-01-01');
             INSERT INTO discord_channel_config
                (channel_id, agent_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, heartbeat_instructions, updated_at) VALUES
                ('300', 'optn',  '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('301', 'optnn', '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('302', 'plain', '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('304', 'plain', '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('305', 'optbad','900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('304', 'optbad','900', '', 1, 1, 1, 1, NULL, '', '2026-01-01'),
                ('304', '',      '900', '', 1, 1, 1, 1, NULL, '', '2026-01-01');",
        )
        .unwrap();

    initialize(&conn).expect("apply v37");

    // loop membership の模型（config discord agent_ids ∪ opt-in）。noloop は含めない。
    let loop_agents: std::collections::BTreeSet<String> =
        ["optn", "optnn", "plain", "optn0", "optbad"]
            .iter()
            .map(|s| s.to_string())
            .collect();

    for g in [true, false] {
        let old = old_real_firing(&conn, g, &loop_agents);
        let new = new_real_firing(&conn, g);
        assert_eq!(old, new, "移行が発火集合を変えた (G={g})");
    }
    // 非空（vacuous でない）ことを確かめる。
    assert!(
        !old_real_firing(&conn, true, &loop_agents).is_empty(),
        "fixture が発火を含むこと"
    );
    // noloop（prod の e2e-test 相当）は新側で発火しない＝移行が発火を増やしていない。
    let fires_noloop = new_real_firing(&conn, true)
        .iter()
        .any(|(a, _)| a == "noloop");
    assert!(
        !fires_noloop,
        "loop に居ないエージェントを移行が発火させない"
    );
}
