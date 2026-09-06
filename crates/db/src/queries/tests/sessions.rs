use super::*;

#[test]
fn insert_session_dual_writes_agent_sessions() {
    let conn = crate::init_memory().unwrap();
    let session = SessionRow {
        id: "sess-dw".to_string(),
        mode: "discord".to_string(),
        theme: "t".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: "[\"agent-x\",\"agent-y\"]".to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    insert_session(&conn, &session).unwrap();
    assert_eq!(
        list_session_participants(&conn, "sess-dw").unwrap(),
        vec!["agent-x".to_string(), "agent-y".to_string()]
    );
    assert_eq!(count_sessions_for_agent(&conn, "agent-x").unwrap(), 1);
    // JSON 投影も従来どおり保存されている（wire 契約）
    let row = get_session(&conn, "sess-dw").unwrap().unwrap();
    assert_eq!(row.participant_ids_json, "[\"agent-x\",\"agent-y\"]");
}

/// #553: 起動時リコンサイルは **active な subtask セッションだけ**を `'interrupted'` へ
/// 終端化し、他モード（discord / heartbeat / autonomous / nostr）や既に終端の subtask には
/// 一切触れない。あわせて `set_session_status` が任意の 1 セッションを遷移させ、存在しない
/// id では無害（0 行）であることを固定する。
#[test]
fn reconcile_orphaned_subtasks_only_touches_active_subtasks() {
    let conn = setup();
    let mk = |id: &str, mode: &str, status: &str| SessionRow {
        id: id.to_string(),
        mode: mode.to_string(),
        theme: "t".to_string(),
        phase: "active".to_string(),
        turn_number: 0,
        status: status.to_string(),
        participant_ids_json: "[]".to_string(),
        facilitator_id: None,
        done_count: 0,
        max_turns: None,
        metadata_json: None,
    };
    // 孤児（active subtask）2 件・既に終端の subtask 1 件・他モード各種。
    insert_session(&conn, &mk("subtask-a", "subtask", "active")).unwrap();
    insert_session(&conn, &mk("subtask-b", "subtask", "active")).unwrap();
    insert_session(&conn, &mk("subtask-done", "subtask", "completed")).unwrap();
    insert_session(&conn, &mk("discord-x", "discord", "active")).unwrap();
    insert_session(&conn, &mk("heartbeat-x", "heartbeat", "active")).unwrap();
    insert_session(&conn, &mk("autonomous-x", "autonomous", "active")).unwrap();

    let n = reconcile_orphaned_subtasks(&conn).unwrap();
    assert_eq!(n, 2, "active な subtask 2 件だけが対象");

    // active subtask → interrupted
    let st = |id: &str| get_session(&conn, id).unwrap().unwrap().status;
    assert_eq!(st("subtask-a"), "interrupted");
    assert_eq!(st("subtask-b"), "interrupted");
    // 既に終端の subtask は不変。
    assert_eq!(st("subtask-done"), "completed");
    // 他モードは status も含め一切不変（述語が広すぎないことの固定）。
    assert_eq!(st("discord-x"), "active");
    assert_eq!(st("heartbeat-x"), "active");
    assert_eq!(st("autonomous-x"), "active");

    // 冪等: 2 回目は 0 件。
    assert_eq!(reconcile_orphaned_subtasks(&conn).unwrap(), 0);

    // set_session_status は任意の 1 セッションを終端値へ遷移させる。
    set_session_status(&conn, "discord-x", "completed").unwrap();
    assert_eq!(st("discord-x"), "completed");
    // 存在しない id は 0 行更新で無害（panic しない）。
    set_session_status(&conn, "does-not-exist", "completed").unwrap();
}

// 12. test_session_crud
#[test]
fn test_session_crud() {
    let conn = setup();

    let session = SessionRow {
        id: "session-1".to_string(),
        mode: "facilitated".to_string(),
        theme: "AI Ethics Discussion".to_string(),
        phase: "divergent".to_string(),
        turn_number: 0,
        status: "active".to_string(),
        participant_ids_json: r#"["agent-1","agent-2"]"#.to_string(),
        facilitator_id: Some("agent-1".to_string()),
        done_count: 0,
        max_turns: Some(10),
        metadata_json: None,
    };

    insert_session(&conn, &session).unwrap();

    let fetched = get_session(&conn, "session-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, "session-1");
    assert_eq!(fetched.mode, "facilitated");
    assert_eq!(fetched.theme, "AI Ethics Discussion");
    assert_eq!(fetched.phase, "divergent");
    assert_eq!(fetched.turn_number, 0);
    assert_eq!(fetched.status, "active");
    assert_eq!(fetched.facilitator_id, Some("agent-1".to_string()));
    assert_eq!(fetched.max_turns, Some(10));

    let all = list_sessions(&conn).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, "session-1");
}
