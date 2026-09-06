use super::*;

// ============================================
// short_id tests (T-1.1 ~ T-1.6)
// ============================================

#[test]
fn test_next_short_id_empty_table() {
    // T-1.1: Empty table should return "t1"
    let conn = setup();
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t1");
}

#[test]
fn test_next_short_id_sequential() {
    // T-1.2: With t1,t2,t3 existing, should return "t4"
    let conn = setup();
    for i in 1..=3 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: format!("Topic {i}"),
                summary: "test".to_string(),
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
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t4");
}

#[test]
fn test_next_short_id_independent_prefix() {
    // T-1.3: t1, t2, h1 exist -> prefix="h" returns "h2"
    let conn = setup();
    for (id, prefix, num) in &[("n1", "t", 1), ("n2", "t", 2), ("n3", "h", 1)] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
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
                short_id: Some(format!("{prefix}{num}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "h").unwrap();
    assert_eq!(result, "h2");
}

#[test]
fn test_next_short_id_independent_agent() {
    // T-1.4: agent a1 has t1-t10, agent a2 has t1 -> a2 prefix="t" returns "t2"
    let conn = setup();
    for i in 1..=10 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("a1-node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
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
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "a2-node-1".to_string(),
            agent_id: "a2".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "T".to_string(),
            summary: "s".to_string(),
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
            short_id: Some("t1".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result = next_short_id(&conn, "a2", "t").unwrap();
    assert_eq!(result, "t2");
}

#[test]
fn test_next_short_id_with_gaps() {
    // T-1.5: t1, t3, t5 exist (gaps) -> returns "t6" (MAX+1)
    let conn = setup();
    for (id, num) in &[("n1", 1), ("n2", 3), ("n3", 5)] {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
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
                short_id: Some(format!("t{num}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let result = next_short_id(&conn, "a1", "t").unwrap();
    assert_eq!(result, "t6");
}

#[test]
fn test_next_short_id_all_prefixes() {
    // T-1.6: All prefix patterns return "{prefix}1" on empty table
    let conn = setup();
    for prefix in &["t", "h", "d", "w", "m", "y", "p", "r", "s"] {
        let result = next_short_id(&conn, "a1", prefix).unwrap();
        assert_eq!(result, format!("{prefix}1"), "Failed for prefix {prefix}");
    }
}

// ============================================
// backfill_short_ids tests (T-1.7 ~ T-1.9)
// ============================================

#[test]
fn test_backfill_short_ids_basic() {
    // T-1.7: 5 topics + 3 dailies with NULL short_id -> get assigned
    let conn = setup();
    for i in 1..=5 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: format!("Topic {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    for i in 1..=3 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("daily-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "daily".to_string(),
                source_type: String::new(),
                title: format!("Daily {i}"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T01:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 8);
    // Verify topics got t1-t5, dailies got d1-d3
    let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t1".to_string()));
    let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t5".to_string()));
    let node = get_index_node(&conn, "daily-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("d1".to_string()));
    let node = get_index_node(&conn, "daily-3").unwrap().unwrap();
    assert_eq!(node.short_id, Some("d3".to_string()));
}

#[test]
fn test_backfill_short_ids_skip_existing() {
    // T-1.8: t1, t2 already set, 3 NULL -> only NULL ones get t3, t4, t5
    let conn = setup();
    for i in 1..=2 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    for i in 3..=5 {
        insert_index_node(
            &conn,
            &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 3);
    // t1, t2 unchanged
    let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t1".to_string()));
    // New ones got t3, t4, t5
    let node = get_index_node(&conn, "topic-3").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t3".to_string()));
    let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
    assert_eq!(node.short_id, Some("t5".to_string()));
}

#[test]
fn test_backfill_short_ids_empty_table() {
    // T-1.9: No nodes -> 0 changes, no error
    let conn = setup();
    let count = backfill_short_ids(&conn).unwrap();
    assert_eq!(count, 0);
}

// ============================================
// T-1.10 ~ T-1.12: date_from/date_to backfill tests
// TODO: These tests require session_log data infrastructure setup.
//       Implement when session_log-based date inference is added.
// ============================================

// ============================================
// get_index_node_by_short_or_id tests (T-1.13 ~ T-1.15)
// ============================================

#[test]
fn test_get_index_node_by_short_id() {
    // T-1.13: Search by short_id "t42"
    let conn = setup();
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "topic-agent:agent-c:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "Test Topic".to_string(),
            summary: "test summary".to_string(),
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
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result = get_index_node_by_short_or_id(&conn, "a1", "t42").unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().id, "topic-agent:agent-c:main-sess_abc-1-20");
}

#[test]
fn test_get_index_node_by_full_id() {
    // T-1.14: Search by full id
    let conn = setup();
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: "topic-agent:agent-c:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "Test Topic".to_string(),
            summary: "test summary".to_string(),
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
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();
    let result =
        get_index_node_by_short_or_id(&conn, "a1", "topic-agent:agent-c:main-sess_abc-1-20")
            .unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().id, "topic-agent:agent-c:main-sess_abc-1-20");
}

#[test]
fn test_get_index_node_by_short_id_not_found() {
    // T-1.15: Non-existent short_id returns None
    let conn = setup();
    let result = get_index_node_by_short_or_id(&conn, "a1", "t99999").unwrap();
    assert!(result.is_none());
}

/// **フル ID のフォールバック検索も agent_id でスコープされる**（#203 の一括点検）。
///
/// short_id での引きは SQL に `agent_id = ?1` があるので構造的に守られているが、
/// 見つからなかったときのフォールバック（`get_index_node`）は **agent_id を条件に
/// 持たない**ので、取得後の `node.agent_id == agent_id` 再チェックだけが境界になる。
/// ノード ID は `topic-agent:{name}:{session}-...` という予測可能な形なので、この
/// 再チェックが外れると他エージェントの非公開会話のタイトル/サマリが ID の推測だけで
/// 読める。再チェックを削っても落ちるテストが 1 件も無かったため追加する。
#[test]
fn test_get_index_node_by_full_id_is_scoped_to_agent() {
    let conn = setup();
    let node_id = "topic-agent:agent-c:secret-sess_abc-1-20";
    insert_index_node(
        &conn,
        &IndexNodeRow {
            id: node_id.to_string(),
            agent_id: "a1".to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: String::new(),
            title: "a1 の非公開トピック".to_string(),
            summary: "他エージェントに見えてはならない要約".to_string(),
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
            short_id: Some("t42".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();

    // 持ち主は引ける（フォールバック経路が生きていることの対照）。
    assert!(
        get_index_node_by_short_or_id(&conn, "a1", node_id)
            .unwrap()
            .is_some(),
        "持ち主はフル ID で引ける"
    );

    // 別エージェントはフル ID を知っていても引けない。
    assert!(
        get_index_node_by_short_or_id(&conn, "a2", node_id)
            .unwrap()
            .is_none(),
        "別エージェントのノードがフル ID 経由で漏れている"
    );

    // short_id も同様（こちらは SQL 側で守られている）。
    assert!(
        get_index_node_by_short_or_id(&conn, "a2", "t42")
            .unwrap()
            .is_none(),
        "別エージェントのノードが short_id 経由で漏れている"
    );
}
