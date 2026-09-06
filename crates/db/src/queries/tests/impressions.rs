use super::*;

// 11. test_impressions_upsert_and_get
#[test]
fn test_impressions_upsert_and_get() {
    let conn = setup();

    let impression = ImpressionRow {
        id: "imp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: "session-1".to_string(),
        target_id: "agent-2".to_string(),
        target_name: "Bob".to_string(),
        personality: "thoughtful and calm".to_string(),
        communication_style: "concise".to_string(),
        recent_behavior: "asked good questions".to_string(),
        agreement: "mostly agree".to_string(),
        notes: "potential collaborator".to_string(),
        last_updated_turn: 5,
    };

    upsert_impression(&conn, &impression).unwrap();

    let results = get_impressions(&conn, "agent-1").unwrap();
    assert_eq!(results.len(), 1);
    let fetched = &results[0];
    assert_eq!(fetched.id, "imp-1");
    assert_eq!(fetched.target_id, "agent-2");
    assert_eq!(fetched.target_name, "Bob");
    assert_eq!(fetched.personality, "thoughtful and calm");
    assert_eq!(fetched.communication_style, "concise");
    assert_eq!(fetched.recent_behavior, "asked good questions");
    assert_eq!(fetched.agreement, "mostly agree");
    assert_eq!(fetched.notes, "potential collaborator");
    assert_eq!(fetched.last_updated_turn, 5);
}

/// 人物像は agent スコープ（#314）: 別セッションで書いても同じ 1 行を更新し、
/// どのセッションからでも同じ内容が読める。
#[test]
fn test_impressions_are_agent_scoped_across_sessions() {
    let conn = setup();

    let base = ImpressionRow {
        id: "imp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: "discord-1".to_string(),
        target_id: "person-x".to_string(),
        target_name: "Bob".to_string(),
        personality: "thoughtful".to_string(),
        communication_style: String::new(),
        recent_behavior: String::new(),
        agreement: "中立".to_string(),
        notes: String::new(),
        last_updated_turn: 1,
    };
    upsert_impression(&conn, &base).unwrap();

    // 別セッション・別経路から同じ相手を更新しても行は増えない。
    let updated = ImpressionRow {
        id: "imp-2".to_string(),
        session_id: "nostr-1".to_string(),
        personality: "thoughtful and warm".to_string(),
        ..base.clone()
    };
    upsert_impression(&conn, &updated).unwrap();

    let all = get_impressions(&conn, "agent-1").unwrap();
    assert_eq!(all.len(), 1, "same person must stay a single row");
    // 既存行の id は保たれ、内容と「最後に更新したセッション」が更新される。
    assert_eq!(all[0].id, "imp-1");
    assert_eq!(all[0].personality, "thoughtful and warm");
    assert_eq!(all[0].session_id, "nostr-1");

    let one = get_impression(&conn, "agent-1", "person-x")
        .unwrap()
        .expect("impression");
    assert_eq!(one.personality, "thoughtful and warm");

    // 別エージェント / 別の相手とは混ざらない。
    assert!(get_impression(&conn, "agent-2", "person-x")
        .unwrap()
        .is_none());
    assert!(get_impression(&conn, "agent-1", "person-y")
        .unwrap()
        .is_none());
}
