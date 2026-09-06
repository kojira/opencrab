use super::super::*;
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

/// #546: `idx_memory_sessions_session_type` は新規 DB（SCHEMA_SQL）にも既存 DB
/// （migration v39）にも届くこと。SCHEMA_SQL 側だけ／migration 側だけ、の食い違い
/// （#475 型の「既存 DB にだけ届かない」地雷）を両経路で固定する。
#[test]
fn session_type_index_reaches_new_and_existing_dbs() {
    fn has_session_type_index(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
             AND name='idx_memory_sessions_session_type'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    // 新規 DB: SCHEMA_SQL 経路で index を持つ。
    let conn = crate::init_memory().expect("init");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(has_session_type_index(&conn), "新規 DB に index が無い");

    // 既存 DB（v38・index 無し）を模す: index を落として版を 38 へ戻す。
    conn.execute_batch("DROP INDEX idx_memory_sessions_session_type; PRAGMA user_version = 38;")
        .unwrap();
    assert!(!has_session_type_index(&conn));

    // 再初期化で migration v39 が走り、index を復活し最新版へスタンプする。
    initialize(&conn).expect("re-initialize");
    assert!(
        has_session_type_index(&conn),
        "migration v39 が index を作っていない"
    );
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

/// #489: co_agent 逆引き列（`agent_discord_config.bot_user_id` /
/// `agent_nostr_config.self_pubkey`）が新規 DB（SCHEMA_SQL）にも既存 DB（migration v40）にも
/// 届くこと。SCHEMA_SQL 側だけ／migration 側だけ、の食い違い（#475 型の「既存 DB にだけ
/// 届かない」地雷）を両経路で固定する。#546 と同型。
#[test]
fn co_agent_reverse_lookup_columns_reach_new_and_existing_dbs() {
    // 新規 DB: SCHEMA_SQL 経路で両列を持つ。
    let conn = crate::init_memory().expect("init");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(
        column_exists(&conn, "agent_discord_config", "bot_user_id").unwrap(),
        "新規 DB に bot_user_id が無い"
    );
    assert!(
        column_exists(&conn, "agent_nostr_config", "self_pubkey").unwrap(),
        "新規 DB に self_pubkey が無い"
    );

    // 既存 DB（v39・両列無し）を模す: 列を落として版を 39 へ戻す（SQLite 3.35+ の DROP COLUMN）。
    conn.execute_batch(
        "ALTER TABLE agent_discord_config DROP COLUMN bot_user_id; \
         ALTER TABLE agent_nostr_config DROP COLUMN self_pubkey; \
         PRAGMA user_version = 39;",
    )
    .unwrap();
    assert!(!column_exists(&conn, "agent_discord_config", "bot_user_id").unwrap());
    assert!(!column_exists(&conn, "agent_nostr_config", "self_pubkey").unwrap());

    // 再初期化で migration v40 が走り、両列を復活し最新版へスタンプする。
    initialize(&conn).expect("re-initialize");
    assert!(
        column_exists(&conn, "agent_discord_config", "bot_user_id").unwrap(),
        "migration v40 が bot_user_id を足していない"
    );
    assert!(
        column_exists(&conn, "agent_nostr_config", "self_pubkey").unwrap(),
        "migration v40 が self_pubkey を足していない"
    );
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 冪等: もう一度 initialize しても列は 1 本のまま（既に v40 済みなので no-op）。
    initialize(&conn).expect("re-initialize idempotent");
    assert!(column_exists(&conn, "agent_discord_config", "bot_user_id").unwrap());
    assert!(column_exists(&conn, "agent_nostr_config", "self_pubkey").unwrap());
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
