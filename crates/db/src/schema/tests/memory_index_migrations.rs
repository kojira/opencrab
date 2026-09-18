/// v20 の DB が **v21（impressions の再構築）→ v22（owner_pubkey の追加）** を
/// 順に通り、**どちらのデータも失われない**（#314 と #319 が同じ版列に並ぶ）。
///
/// v21 は表の再構築（`DROP TABLE impressions`）を伴い、v22 は別の表への列追加。
/// 別々のマイグレーションが独立に通ることは各テストで見ているが、**同じ 1 回の
/// 起動で連続して流れる**のは実運用の経路（v20 で止まっていた DB が今回の
/// バイナリで初めて起動する）なので、ここで両方を 1 本の流れとして固定する。
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
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
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
             VALUES ('cat1', 'a1', NULL, 'category', 'category', 'ownerさんの教え', '', '2026-02-01', '2026-02-01'),
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
///
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
                    ('cat1', 'a1', NULL, 'category', 'category', 'ownerさんの教え', '', '2026-02-01', '2026-02-01'),
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
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
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
