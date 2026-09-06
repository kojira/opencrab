use super::*;

// ============================================
// カテゴリ層（issue #313）
// ============================================

/// **回帰ガード**: `insert_index_node` は `INSERT OR IGNORE` なので、node_type の CHECK に
/// `'category'`/`'meta'` が無いと**エラーにならず黙って落ちる**（作ったつもりで消える）。
/// この沈黙を固定する: 挿入後に実際に読み戻せることを assert する。CHECK を狭めて
/// 退行させたら、OR IGNORE で行が入らず get_index_node が None になりこのテストが落ちる。
#[test]
fn category_and_meta_nodes_persist_through_or_ignore_insert() {
    let conn = setup();
    for (id, ntype, sid) in [("cat1", "category", "c1"), ("meta1", "meta", "g1")] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: ntype.to_string(),
                source_type: "category".to_string(),
                title: format!("title-{id}"),
                summary: String::new(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(sid.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
        // 沈黙が起きていないこと（OR IGNORE で握り潰されず実際に保存されている）。
        assert!(
            get_index_node(&conn, id).unwrap().is_some(),
            "{ntype} ノードが INSERT OR IGNORE で黙って落ちた（CHECK に {ntype} が無い退行）"
        );
    }
}

/// #520: 3 つの専用ルート（category / declared / condensed）は `ensure_root` に集約された。
/// 3 経路すべてで次を固定する:
/// 正しい `node_type='root'` / `source_type` / `title` / `parent=None` / `depth=0` /
/// `short_id` 付きのノードが作られること（呼び出し元の挙動が変わらない）;
/// **read-back ガードが効くこと**＝返る id が必ず実在ノードを指すこと（#344 の沈黙を固定。
/// 集約前は category 経路だけガードが欠けていた。`ensure_root` が退行して OR IGNORE で
/// 握り潰したら `get_index_node` が None になりここが落ちる）;
/// 冪等（二度目は同じ id・二重作成しない）;
/// 3 種の id・short_id が互いに別で共存すること（相互に握り潰さない）。
#[test]
fn ensure_root_paths_persist_and_are_idempotent() {
    // (呼ぶ関数, 期待 id, 期待 source_type, 期待 title)
    type Case = (
        fn(&Connection, &str, &str) -> anyhow::Result<String>,
        &'static str,
        &'static str,
        &'static str,
    );
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";

    let cases: [Case; 3] = [
        (ensure_category_root, "catroot-a1", "category", "カテゴリ"),
        (
            ensure_declared_root,
            "declroot-a1",
            "declared",
            "宣言した記憶",
        ),
        (
            ensure_condensed_root,
            "condroot-a1",
            "condensed",
            "凝縮した記憶",
        ),
    ];

    for (ensure_fn, expected_id, source_type, title) in cases {
        let id = ensure_fn(&conn, "a1", now).unwrap();
        assert_eq!(id, expected_id, "決定的 id");

        // read-back: 返る id は必ず実在ノードを指す（OR IGNORE で握り潰されていない / #344）。
        let node = get_index_node(&conn, &id).unwrap().unwrap_or_else(|| {
            panic!("{source_type} ルートが read-back で見つからない（握り潰し）")
        });
        assert_eq!(node.node_type, "root");
        assert_eq!(node.source_type, source_type);
        assert_eq!(node.title, title);
        assert_eq!(node.parent_id, None);
        assert_eq!(node.depth, 0);
        assert!(node.short_id.is_some(), "short_id が採番されている");

        // 冪等: 二度目は同じ id を返す。
        assert_eq!(ensure_fn(&conn, "a1", now).unwrap(), id, "二度目も同じ id");
    }

    // 二重作成しない: root ノードはちょうど 3 つ（3 種各 1）。
    let root_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_nodes WHERE agent_id = 'a1' AND node_type = 'root'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(root_count, 3, "3 種のルートが各 1 つだけ（二重作成しない）");

    // 3 種の short_id は互いに別（`r` 系列で相互に握り潰さず共存する）。
    let short_ids: Vec<String> = ["catroot-a1", "declroot-a1", "condroot-a1"]
        .iter()
        .map(|id| {
            get_index_node(&conn, id)
                .unwrap()
                .unwrap()
                .short_id
                .unwrap()
        })
        .collect();
    let mut uniq = short_ids.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 3, "3 種の short_id が互いに別: {short_ids:?}");
}

#[test]
fn category_seed_and_assignment_queries_roundtrip() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";

    // 種: curated long_term/<名前> を 2 件。
    for (i, name) in ["Rustの学び", "Discord運用"].iter().enumerate() {
        upsert_curated_memory(
            &conn,
            &CuratedMemoryRow {
                id: format!("cm{i}"),
                agent_id: "a1".to_string(),
                category: format!("long_term/{name}"),
                content: "…".to_string(),
                created_at: now.to_string(),
            },
        )
        .unwrap();
    }
    let seeds = list_long_term_category_seeds(&conn, "a1").unwrap();
    assert_eq!(
        seeds,
        vec!["Discord運用".to_string(), "Rustの学び".to_string()]
    );

    // カテゴリツリーの根を確保（冪等）。
    let root = ensure_category_root(&conn, "a1", now).unwrap();
    assert_eq!(ensure_category_root(&conn, "a1", now).unwrap(), root);

    // カテゴリノードを作り、トップレベルに現れる。
    let cat = insert_category_node(&conn, "a1", &root, "Rustの学び", "", now).unwrap();
    assert!(get_category_node_by_title(&conn, "a1", "Rustの学び")
        .unwrap()
        .is_some());
    let tops = list_top_level_categories(&conn, "a1").unwrap();
    assert_eq!(tops.len(), 1);

    // topic を積み、未割当 → 割当 → sticky。
    for (id, sid) in [("t1", "t1"), ("t2", "t2")] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("topic-{id}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: now.to_string(),
                updated_at: now.to_string(),
                short_id: Some(sid.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    assert_eq!(list_unassigned_topics(&conn, "a1", 10).unwrap().len(), 2);
    assert!(assign_topic_to_category(&conn, "a1", "t1", &cat.id, now).unwrap());
    // 二重割当は sticky で false（同じ入力で結果が変わらない）。
    assert!(!assign_topic_to_category(&conn, "a1", "t1", &cat.id, now).unwrap());
    assert_eq!(list_unassigned_topics(&conn, "a1", 10).unwrap().len(), 1);
    let counts = count_category_members(&conn, "a1").unwrap();
    assert_eq!(counts.get(&cat.id).copied(), Some(1));
}

// ---- タグ操作（issue #359 / #313 段階2）----

/// #359: 1 つの topic に複数タグが付き（多対多）、付け直し・外しができる（sticky でない）。
/// 二重付与は PK 冪等。無いタグ名は新設され、既存タグは title 一致で束ねて二重作成しない。
#[test]
fn tag_topic_multi_tag_untag_retag() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";
    insert_test_topic(&conn, "a1", "t1", "t1");

    // 2 つのタグを付ける（両方新設）。1 topic に複数タグ。
    tag_topic(
        &conn,
        "a1",
        "t1",
        &["Rust".to_string(), "設計".to_string()],
        now,
    )
    .unwrap();
    let counts = count_category_members(&conn, "a1").unwrap();
    let total: i64 = counts.values().sum();
    assert_eq!(total, 2, "1 topic に 2 タグが付く（多対多）");
    // タグは category ノードとして browse で引ける。
    assert!(get_category_node_by_title(&conn, "a1", "Rust")
        .unwrap()
        .is_some());
    assert!(get_category_node_by_title(&conn, "a1", "設計")
        .unwrap()
        .is_some());
    assert_eq!(list_top_level_categories(&conn, "a1").unwrap().len(), 2);

    // 同じタグの二重付与は冪等（PK で弾かれ、件数もノード数も増えない）。
    tag_topic(&conn, "a1", "t1", &["Rust".to_string()], now).unwrap();
    let total2: i64 = count_category_members(&conn, "a1").unwrap().values().sum();
    assert_eq!(total2, 2, "二重付与は冪等（増えない）");
    assert_eq!(list_top_level_categories(&conn, "a1").unwrap().len(), 2);

    // 外せる（member を削除。タグノード自体は残る）。
    assert!(remove_tag_member(&conn, "a1", "t1", "Rust").unwrap());
    let rust_id = get_category_node_by_title(&conn, "a1", "Rust")
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&rust_id)
            .copied(),
        None,
        "外したタグの member は 0 件"
    );
    assert!(
        get_category_node_by_title(&conn, "a1", "Rust")
            .unwrap()
            .is_some(),
        "外してもタグノード自体は消えない"
    );
    // 二度目の外しは false（もう付いていない）。
    assert!(!remove_tag_member(&conn, "a1", "t1", "Rust").unwrap());

    // 付け直せる（sticky でない = 一期一会）。
    tag_topic(&conn, "a1", "t1", &["Rust".to_string()], now).unwrap();
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&rust_id)
            .copied(),
        Some(1),
        "外した後にまた付けられる（sticky でない）"
    );
}

/// #359: タグ新設が黙って失敗しない。`resolve_or_create_tag` は新設したノードを read-back
/// で検証してから id を返す（`insert_index_node` の `INSERT OR IGNORE` が CHECK 違反等を
/// 握り潰しても、実在しなければ Err にする / #344 の教訓）。返る id は必ず実在する。
/// 既存タグは title 一致で束ねて二重作成しない＝冪等。空白のみの名前は拒否する。
#[test]
fn resolve_or_create_tag_is_verified_and_idempotent() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";

    // 新設: 返る id は必ず実在ノードを指す（黙って失敗＝ダングリング id を返さない）。
    let id1 = resolve_or_create_tag(&conn, "a1", "Rust", now).unwrap();
    assert!(
        get_index_node(&conn, &id1).unwrap().is_some(),
        "新設タグの id が実在ノードを指す（read-back 検証）"
    );

    // 冪等: 同名は同じ id を返し、ノードを二重に作らない。
    let id2 = resolve_or_create_tag(&conn, "a1", "Rust", now).unwrap();
    assert_eq!(id1, id2, "同名タグは同じ id（二重作成しない）");
    assert_eq!(
        list_top_level_categories(&conn, "a1").unwrap().len(),
        1,
        "ノードは 1 つだけ"
    );
    // 前後の空白は正規化される（別タグにならない）。
    let id3 = resolve_or_create_tag(&conn, "a1", "  Rust ", now).unwrap();
    assert_eq!(id1, id3, "前後空白は正規化されて同じタグ");

    // 空白のみ / 空文字は拒否（無名タグを作らない）。
    assert!(resolve_or_create_tag(&conn, "a1", "", now).is_err());
    assert!(resolve_or_create_tag(&conn, "a1", "   ", now).is_err());
    assert_eq!(
        list_top_level_categories(&conn, "a1").unwrap().len(),
        1,
        "拒否時にノードを増やさない"
    );
}

/// #359: `merge_tags` は from の member を into へ付け替え、from ノードを削除する。
/// - 付け替え先に同じ (topic, into) 行が既にあっても落ちない（OR IGNORE でスキップ）。
/// - from タグノード削除で **FTS 孤児を残さない**（`delete_index_node` の subtree CTE 経由）。
#[test]
fn merge_tags_reassigns_without_collision_and_cleans_fts() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";
    insert_test_topic(&conn, "a1", "t1", "t1");
    insert_test_topic(&conn, "a1", "t2", "t2");

    // t1 は from と into の両方、t2 は from だけ。統合すると t1 が付け替え衝突を起こす。
    tag_topic(
        &conn,
        "a1",
        "t1",
        &["旧".to_string(), "新".to_string()],
        now,
    )
    .unwrap();
    tag_topic(&conn, "a1", "t2", &["旧".to_string()], now).unwrap();

    let from_id = get_category_node_by_title(&conn, "a1", "旧")
        .unwrap()
        .unwrap()
        .id;
    // from ノードの FTS 行が存在することを事前確認（後で孤児が残らないことを見るため）。
    let fts_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = ?1",
            params![from_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_before, 1, "from タグは FTS に載っている");

    // 統合（付け替え衝突を含む）: t1 は既に「新」を持つので OR IGNORE でスキップされ落ちない。
    let outcome = merge_tags(&conn, "a1", "旧", "新", now).unwrap();
    assert_eq!(outcome.from_category_id, from_id);

    // from ノードは消え、from の member も残らない（孤児 member 無し）。
    assert!(
        get_category_node_by_title(&conn, "a1", "旧")
            .unwrap()
            .is_none(),
        "統合後 from タグノードは削除される"
    );
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&from_id)
            .copied(),
        None,
        "from の member は残らない"
    );
    // into には t1・t2 の 2 件（t1 は元々あった 1 件のまま、衝突で二重にならない）。
    let into_id = get_category_node_by_title(&conn, "a1", "新")
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(
        count_category_members(&conn, "a1")
            .unwrap()
            .get(&into_id)
            .copied(),
        Some(2),
        "into に 2 topic（衝突で二重にならない）"
    );

    // FTS 孤児が残らない: 消えた from ノードの FTS 行が無い。
    let fts_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_fts WHERE node_id = ?1",
            params![from_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_after, 0, "from タグ削除で FTS 孤児を残さない");

    // 自己統合（from == into）は拒否（from を消すと into も消えるため）。
    assert!(merge_tags(&conn, "a1", "新", "新", now).is_err());
    // 存在しない from の統合も拒否。
    assert!(merge_tags(&conn, "a1", "無", "新", now).is_err());
}

/// 実データに近い規模（topic 数千件）でカテゴリ層のクエリが破綻しないことを確認する。
/// LLM は使わず、割当の「頭脳」以外（未割当抽出・sticky 割当・件数集計・冪等性）を
/// 実規模で検証する。CI を重くしないため #[ignore]（`--ignored` で明示実行）。
#[test]
#[ignore]
fn category_layer_scales_to_thousands_of_topics() {
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";
    let root = ensure_category_root(&conn, "a1", now).unwrap();
    // 5 カテゴリを種として用意。
    let cats: Vec<String> = (0..5)
        .map(|i| {
            insert_category_node(&conn, "a1", &root, &format!("カテゴリ{i}"), "", now)
                .unwrap()
                .id
        })
        .collect();

    // 3,000 topic を積む（session_log ツリー）。
    let n: i64 = 3000;
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "s1".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "session".to_string(),
            source_type: "session_log".to_string(),
            title: "S".to_string(),
            summary: String::new(),
            start_log_id: None,
            end_log_id: None,
            source_session_id: None,
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            short_id: Some("s1".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    for i in 0..n {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("t{i}"),
                agent_id: "a1".to_string(),
                parent_id: Some("s1".to_string()),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("topic {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-06-01T00:00:{:02}Z", i % 60),
                updated_at: now.to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    // バッチ 12 件ずつ、未割当が尽きるまで sticky 割当する（LLM の代わりに round-robin）。
    let started = std::time::Instant::now();
    let mut total = 0usize;
    let mut batches = 0usize;
    loop {
        let batch = list_unassigned_topics(&conn, "a1", 12).unwrap();
        if batch.is_empty() {
            break;
        }
        for (k, t) in batch.iter().enumerate() {
            assert!(
                assign_topic_to_category(&conn, "a1", &t.id, &cats[(total + k) % 5], now).unwrap()
            );
        }
        total += batch.len();
        batches += 1;
        assert!((batches as i64) < n, "バッチが前進していない（無限ループ）");
    }
    let elapsed = started.elapsed();
    assert_eq!(total, n as usize, "全 topic が割り当てられる");
    // 未割当が尽き、再度引いても空（sticky・冪等）。
    assert!(list_unassigned_topics(&conn, "a1", 12).unwrap().is_empty());
    // 件数集計はカテゴリ数ぶんに収まる（トップレベルが 5 のまま膨らまない）。
    let counts = count_category_members(&conn, "a1").unwrap();
    assert_eq!(counts.values().sum::<i64>(), n);
    assert_eq!(counts.len(), 5);
    assert_eq!(list_top_level_categories(&conn, "a1").unwrap().len(), 5);
    // 冪等: 既割当への再割当は false（結果が変わらない）。
    assert!(!assign_topic_to_category(&conn, "a1", "t0", &cats[0], now).unwrap());
    eprintln!(
        "[scale] {n} topics assigned in {batches} batches, {} ms",
        elapsed.as_millis()
    );
}
