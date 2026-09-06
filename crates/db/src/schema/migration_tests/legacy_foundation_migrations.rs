use super::super::*;
use super::support::seed_legacy_impressions;
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
