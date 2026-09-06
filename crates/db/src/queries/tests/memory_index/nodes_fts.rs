use super::*;

// ============================================
// memory_index_fts / キーワード逆引きテスト
// ============================================

fn fts_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_index_fts", [], |r| r.get(0))
        .unwrap()
}

fn nodes_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_index_nodes", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn test_index_fts_consistency_through_write_paths() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node(
            "t1",
            "a1",
            "Discord連携",
            "botの実装",
            &["Discord", "serenity"],
        ),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a1", "料理の話", "カレーを作った", &["カレー"]),
    )
    .unwrap();
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // キーワードでヒット
    let hits = search_index_nodes(&conn, "a1", "serenity", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");

    // summary 更新が検索に反映される
    update_index_node_summary(&conn, "t2", "料理の話", "肉じゃがを作った").unwrap();
    assert!(
        search_index_nodes(&conn, "a1", "肉じゃが", 10, None)
            .unwrap()
            .len()
            == 1
    );
    assert!(search_index_nodes(&conn, "a1", "カレーを作った", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // keywords 更新が検索に反映される
    update_index_node_keywords(&conn, "t2", "[\"肉じゃが\",\"じゃがいも\"]").unwrap();
    let hits = search_index_nodes(&conn, "a1", "じゃがいも", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t2");

    // 単体削除で FTS も消える
    delete_index_node(&conn, "t1").unwrap();
    assert!(search_index_nodes(&conn, "a1", "serenity", 10, None)
        .unwrap()
        .is_empty());
    assert_eq!(fts_count(&conn), nodes_count(&conn));

    // agent 単位 purge で FTS も消える
    delete_index_nodes_for_agent(&conn, "a1").unwrap();
    assert_eq!(fts_count(&conn), 0);
    assert_eq!(nodes_count(&conn), 0);
}

#[test]
fn test_index_fts_insert_or_ignore_keeps_existing() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "元タイトル", "元要約", &["元"]),
    )
    .unwrap();
    // 同一 id の再 insert は OR IGNORE で無視され、FTS も元のまま
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "新タイトル", "新要約", &["新"]),
    )
    .unwrap();
    assert_eq!(fts_count(&conn), 1);
    assert_eq!(
        search_index_nodes(&conn, "a1", "元要約", 10, None)
            .unwrap()
            .len(),
        1
    );
    assert!(search_index_nodes(&conn, "a1", "新要約", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_search_index_nodes_scoping_and_filters() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "Rust勉強会", "所有権の話", &["Rust"]),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a2", "Rust輪読", "他人の記憶", &["Rust"]),
    )
    .unwrap();
    let mut period = mk_topic_node("p1", "a1", "2026-05", "5月のRustまとめ", &[]);
    period.node_type = "period".to_string();
    insert_index_node(&conn, &period).unwrap();

    // agent 分離: a1 からは a2 のノードが見えない
    let hits = search_index_nodes(&conn, "a1", "Rust", 10, None).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.node_id != "t2"));

    // node_type フィルタ
    let hits = search_index_nodes(&conn, "a1", "Rust", 10, Some("period")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "p1");

    // AND で 0 件 → OR フォールバックで拾う
    let hits = search_index_nodes(&conn, "a1", "所有権 存在しない語", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");

    // 空クエリは空結果
    assert!(search_index_nodes(&conn, "a1", "   ", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_list_topics_missing_keywords() {
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "キーワードなし", "s", &[]),
    )
    .unwrap();
    insert_index_node(
        &conn,
        &mk_topic_node("t2", "a1", "キーワードあり", "s", &["kw"]),
    )
    .unwrap();
    let mut daily = mk_topic_node("d1", "a1", "daily由来", "s", &[]);
    daily.source_type = "daily_log".to_string();
    insert_index_node(&conn, &daily).unwrap();

    let missing = list_topics_missing_keywords(&conn, "a1", 10).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].id, "t1");
}

#[test]
fn test_search_index_nodes_short_query_like_fallback() {
    // trigram は 3 文字未満の語に当たらない → LIKE フォールバックで拾う
    let conn = setup();
    insert_index_node(
        &conn,
        &mk_topic_node("t1", "a1", "AI導入の相談", "LLMの選定", &["AI"]),
    )
    .unwrap();
    let hits = search_index_nodes(&conn, "a1", "AI", 10, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, "t1");
    // LIKE フォールバックでも agent 分離は効く
    assert!(search_index_nodes(&conn, "a2", "AI", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_delete_index_node_cascades_fts_for_subtree() {
    // parent_id の ON DELETE CASCADE で子孫ノードが消えるとき、FTS も部分木ごと消える
    let conn = setup();
    let mut parent = mk_topic_node("s1", "a1", "親セッション", "親要約", &[]);
    parent.node_type = "session".to_string();
    insert_index_node(&conn, &parent).unwrap();
    let mut child = mk_topic_node("t1", "a1", "子トピック", "子要約ユニーク", &["子kw"]);
    child.parent_id = Some("s1".to_string());
    insert_index_node(&conn, &child).unwrap();
    assert_eq!(nodes_count(&conn), 2);
    assert_eq!(fts_count(&conn), 2);

    delete_index_node(&conn, "s1").unwrap();
    // CASCADE で子も消え、FTS に孤児が残らない
    assert_eq!(nodes_count(&conn), 0);
    assert_eq!(fts_count(&conn), 0);
    assert!(search_index_nodes(&conn, "a1", "子要約ユニーク", 10, None)
        .unwrap()
        .is_empty());
}

#[test]
fn test_delete_index_node_cleans_category_members() {
    // ノード削除時に memory_category_members の該当行が topic_id / category_id の
    // **両方向**で掃除される（宙に浮く参照を残さない）。v33 マイグレーションと同じ意味論。
    let conn = setup();
    let now = "2026-06-01T00:00:00Z";
    // topic を 2 件（t1 は削除、t2 は残す）と、topic に付けるタグ（category）ノード 2 件。
    insert_test_topic(&conn, "a1", "t1", "t1");
    insert_test_topic(&conn, "a1", "t2", "t2");
    let root = ensure_category_root(&conn, "a1", now).unwrap();
    let tag_del = insert_category_node(&conn, "a1", &root, "消すタグ", "", now).unwrap();
    let tag_keep = insert_category_node(&conn, "a1", &root, "残すタグ", "", now).unwrap();

    // (1) topic_id = 削除対象 の member: t1 に 2 タグ付与。
    assert!(assign_topic_to_category(&conn, "a1", "t1", &tag_del.id, now).unwrap());
    assert!(assign_topic_to_category(&conn, "a1", "t1", &tag_keep.id, now).unwrap());
    // (2) category_id = 削除対象 の member: 別 topic t2 に「消すタグ」を付与。
    assert!(assign_topic_to_category(&conn, "a1", "t2", &tag_del.id, now).unwrap());
    assert_eq!(members_for(&conn, "a1", "t1"), 2);
    assert_eq!(members_for(&conn, "a1", "t2"), 1);

    // topic t1 を削除 → topic_id=t1 の member（2 行）が消える。t2 側は無関係なので残る。
    delete_index_node(&conn, "t1").unwrap();
    assert_eq!(members_for(&conn, "a1", "t1"), 0);
    assert_eq!(members_for(&conn, "a1", "t2"), 1);

    // タグノード tag_del を削除 → category_id=tag_del の member（t2 の 1 行）が消える。
    delete_index_node(&conn, &tag_del.id).unwrap();
    assert_eq!(members_for(&conn, "a1", "t2"), 0);
    // 全 member が掃除されている（残骸ゼロ）。
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_category_members", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(total, 0);
}

#[test]
fn test_delete_index_node_fixes_parent_child_count() {
    // 子ノード削除時に親の child_count を実カウントへ直す。特に**子が 0 になる親**は
    // 索引ビルダの再計算（現存する子を持つ親しか UPDATE しない）では永久に直らないので、
    // ここで直る必要がある。v33 マイグレーションと同じ意味論。
    let conn = setup();

    // 親 p1: 子 1 つ（c1）。child_count はあらかじめ 1（正しい状態）にしておく。
    let mut p1 = mk_topic_node("p1", "a1", "親（子1つ）", "s", &[]);
    p1.node_type = "session".to_string();
    p1.child_count = 1;
    insert_index_node(&conn, &p1).unwrap();
    let mut c1 = mk_topic_node("c1", "a1", "子1", "s", &[]);
    c1.parent_id = Some("p1".to_string());
    insert_index_node(&conn, &c1).unwrap();

    // 親 p2: 子 2 つ（c2a, c2b）。child_count = 2。
    let mut p2 = mk_topic_node("p2", "a1", "親（子2つ）", "s", &[]);
    p2.node_type = "session".to_string();
    p2.child_count = 2;
    insert_index_node(&conn, &p2).unwrap();
    let mut c2a = mk_topic_node("c2a", "a1", "子2a", "s", &[]);
    c2a.parent_id = Some("p2".to_string());
    insert_index_node(&conn, &c2a).unwrap();
    let mut c2b = mk_topic_node("c2b", "a1", "子2b", "s", &[]);
    c2b.parent_id = Some("p2".to_string());
    insert_index_node(&conn, &c2b).unwrap();

    let child_count = |id: &str| get_index_node(&conn, id).unwrap().unwrap().child_count;

    // 子が 0 になるケース: p1 の最後の子 c1 を消す → p1.child_count は 1 → 0。
    delete_index_node(&conn, "c1").unwrap();
    assert_eq!(child_count("p1"), 0);

    // 子が残るケース: p2 の子 1 つを消す → p2.child_count は 2 → 1。
    delete_index_node(&conn, "c2a").unwrap();
    assert_eq!(child_count("p2"), 1);
}

#[test]
fn test_index_write_helpers_work_inside_outer_transaction() {
    // index_builder::delete_index はトランザクション内から delete_index_nodes_for_agent
    // を呼ぶ。SAVEPOINT 方式なので外側 tx があっても動くこと（BEGIN の入れ子は不可）。
    let conn = setup();
    insert_index_node(&conn, &mk_topic_node("t1", "a1", "T", "S", &["kw"])).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    delete_index_nodes_for_agent(&tx, "a1").unwrap();
    insert_index_node(&tx, &mk_topic_node("t2", "a1", "T2", "S2", &["kw2"])).unwrap();
    tx.commit().unwrap();
    assert_eq!(nodes_count(&conn), 1);
    assert_eq!(fts_count(&conn), 1);
}
