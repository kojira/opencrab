use super::*;

// ============================================
// AGENT INBOX（webhook intake / issue #454）
// ============================================

fn inbox_insert(id: &str, agent: &str, source: &str, ev: &str, dedup: &str) -> InboxInsert {
    InboxInsert {
        id: id.to_string(),
        agent_id: agent.to_string(),
        source: source.to_string(),
        event_type: ev.to_string(),
        dedup_key: dedup.to_string(),
        payload_json: format!("{{\"k\":\"{id}\"}}"),
    }
}

#[test]
fn inbox_enqueue_and_dedup() {
    let conn = setup();
    // 新規は true。
    assert!(enqueue_inbox_event(
        &conn,
        &inbox_insert(
            "i1",
            "agent_alpha",
            "sample-source",
            "comment.created",
            "c-100"
        )
    )
    .unwrap());
    // 同じ (source, dedup_key) は false（二重に積まない）。id が違っても弾く。
    assert!(!enqueue_inbox_event(
        &conn,
        &inbox_insert(
            "i2",
            "agent_alpha",
            "sample-source",
            "comment.created",
            "c-100"
        )
    )
    .unwrap());
    // dedup_key が違えば新規。
    assert!(enqueue_inbox_event(
        &conn,
        &inbox_insert(
            "i3",
            "agent_alpha",
            "sample-source",
            "comment.created",
            "c-101"
        )
    )
    .unwrap());
    // dedup は source 単位。別 source なら同じ dedup_key でも積める。
    assert!(enqueue_inbox_event(
        &conn,
        &inbox_insert("i4", "agent_alpha", "other", "x", "c-100")
    )
    .unwrap());

    assert_eq!(count_unprocessed_inbox(&conn, "agent_alpha").unwrap(), 3);
}

#[test]
fn inbox_list_order_and_processing() {
    let conn = setup();
    enqueue_inbox_event(
        &conn,
        &inbox_insert("a", "agent_alpha", "sample-source", "comment.created", "1"),
    )
    .unwrap();
    enqueue_inbox_event(
        &conn,
        &inbox_insert("b", "agent_alpha", "sample-source", "comment.created", "2"),
    )
    .unwrap();
    enqueue_inbox_event(
        &conn,
        &inbox_insert("z", "agent_beta", "sample-source", "chat.message", "3"),
    )
    .unwrap();

    // agent スコープで絞られる。
    let agent_alpha = list_unprocessed_inbox(&conn, "agent_alpha", 10).unwrap();
    assert_eq!(agent_alpha.len(), 2);
    // received_at 同値でも id ASC で安定。
    assert_eq!(agent_alpha[0].id, "a");
    assert_eq!(agent_alpha[1].id, "b");

    // 処理済みにすると未処理から外れる。
    assert!(mark_inbox_processed(&conn, "a").unwrap());
    assert_eq!(count_unprocessed_inbox(&conn, "agent_alpha").unwrap(), 1);
    // 二重処理は false（既に刻んである）。
    assert!(!mark_inbox_processed(&conn, "a").unwrap());

    // 未処理を持つエージェント集合（クエリは agent_id ASC で返す）。
    assert_eq!(
        agents_with_unprocessed_inbox(&conn).unwrap(),
        vec!["agent_alpha".to_string(), "agent_beta".to_string()]
    );

    // limit が効く。
    enqueue_inbox_event(
        &conn,
        &inbox_insert("c", "agent_alpha", "sample-source", "comment.created", "4"),
    )
    .unwrap();
    assert_eq!(
        list_unprocessed_inbox(&conn, "agent_alpha", 1)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn inbox_scan_excludes_fully_processed_agents() {
    // 「inbox 空の tick で LLM を呼ばない」の要: 全件処理済みのエージェントは走査対象から
    // 外れる（消化ループはこの集合しか回さないので、空なら turn の到達点が 1 本も無い）。
    let conn = setup();
    enqueue_inbox_event(
        &conn,
        &inbox_insert("a", "agent_alpha", "sample-source", "comment.created", "1"),
    )
    .unwrap();
    enqueue_inbox_event(
        &conn,
        &inbox_insert("b", "agent_alpha", "sample-source", "comment.created", "2"),
    )
    .unwrap();
    assert_eq!(
        agents_with_unprocessed_inbox(&conn).unwrap(),
        vec!["agent_alpha".to_string()]
    );

    // 1 件処理してもまだ残るので対象。
    mark_inbox_processed(&conn, "a").unwrap();
    assert_eq!(
        agents_with_unprocessed_inbox(&conn).unwrap(),
        vec!["agent_alpha".to_string()]
    );

    // 全件処理済みにすると走査対象から消える（＝このエージェントには turn が起きない）。
    mark_inbox_processed(&conn, "b").unwrap();
    assert!(agents_with_unprocessed_inbox(&conn).unwrap().is_empty());
    assert_eq!(count_unprocessed_inbox(&conn, "agent_alpha").unwrap(), 0);
}
