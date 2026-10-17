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

fn d1080_agent(conn: &Connection, agent_id: &str, model: Option<&str>) {
    upsert_agent(
        conn,
        &AgentRow {
            agent_id: agent_id.to_string(),
            name: agent_id.to_string(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".to_string(),
            personality: None,
            instructions: String::new(),
            model: model.map(str::to_string),
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

fn d1080_session(conn: &Connection, id: &str, participants: &str) {
    insert_session(
        conn,
        &SessionRow {
            id: id.to_string(),
            mode: "generic".to_string(),
            theme: id.to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: participants.to_string(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

/// D-1080: a session override wins over the agent model and the server default, sessions of the
/// same agent stay independent, and a reset session follows the (changed) agent default.
#[test]
fn session_model_overrides_are_isolated_and_reset_follows_agent_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d1080.db");
    {
        let conn = Connection::open(&path).unwrap();
        crate::schema::initialize(&conn).unwrap();
        d1080_agent(&conn, "a1", Some("prov:agent-m"));
        d1080_session(&conn, "sess-a", r#"["a1"]"#);
        d1080_session(&conn, "sess-b", r#"["a1"]"#);
        assert_eq!(get_session_model(&conn, "sess-a").unwrap(), None);
        assert!(set_session_model(&conn, "sess-a", Some("prov:m1")).unwrap());
        assert!(set_session_model(&conn, "sess-b", Some("prov:m2")).unwrap());
    }
    // A fresh connection sees the persisted overrides.
    let conn = Connection::open(&path).unwrap();
    crate::schema::initialize(&conn).unwrap();
    let eff = |s: &str| effective_model_for_session(&conn, "a1", s, "srv:default").unwrap();
    assert_eq!(eff("sess-a"), "prov:m1");
    assert_eq!(eff("sess-b"), "prov:m2");

    assert!(set_session_model(&conn, "sess-a", None).unwrap());
    d1080_agent(&conn, "a1", Some("prov:agent-changed"));
    assert_eq!(get_session_model(&conn, "sess-a").unwrap(), None);
    assert_eq!(eff("sess-a"), "prov:agent-changed");
    assert_eq!(eff("sess-b"), "prov:m2");
    assert_eq!(
        get_agent(&conn, "a1").unwrap().unwrap().model.as_deref(),
        Some("prov:agent-changed")
    );

    d1080_agent(&conn, "a1", None);
    assert_eq!(eff("sess-a"), "srv:default");
}

/// D-1080: a missing session or a failed lookup is an error, never the default model.
#[test]
fn session_model_lookup_failures_are_errors() {
    let conn = setup();
    d1080_agent(&conn, "a1", Some("prov:agent-m"));
    assert!(effective_model_for_session(&conn, "a1", "missing", "srv:default").is_err());
    assert!(get_session_model(&conn, "missing").is_err());
    assert!(!set_session_model(&conn, "missing", Some("prov:m1")).unwrap());

    d1080_session(&conn, "sess-a", r#"["a1"]"#);
    conn.execute_batch("ALTER TABLE sessions RENAME COLUMN model TO model_gone")
        .unwrap();
    assert!(effective_model_for_session(&conn, "a1", "sess-a", "srv:default").is_err());
}

/// D-1081: a realtime turn resolves realtime slot > session override > agent model > server
/// default; the default-profile chain ignores the realtime slot.
#[test]
fn realtime_model_chain_falls_through_session_agent_and_default() {
    let conn = setup();
    d1080_agent(&conn, "a1", Some("prov:agent-m"));
    d1080_session(&conn, "sess-a", r#"["a1"]"#);
    d1080_session(&conn, "sess-b", r#"["a1"]"#);
    let rt = |s: &str| effective_realtime_model_for_session(&conn, "a1", s, "srv:default").unwrap();
    let def = |s: &str| effective_model_for_session(&conn, "a1", s, "srv:default").unwrap();

    assert_eq!(get_session_realtime_model(&conn, "sess-a").unwrap(), None);
    assert!(set_session_realtime_model(&conn, "sess-a", Some("prov:light")).unwrap());
    assert!(set_session_model(&conn, "sess-a", Some("prov:session-m")).unwrap());
    assert_eq!(
        get_session_realtime_model(&conn, "sess-a").unwrap().as_deref(),
        Some("prov:light")
    );
    assert_eq!(rt("sess-a"), "prov:light");
    assert_eq!(def("sess-a"), "prov:session-m");
    assert_eq!(rt("sess-b"), "prov:agent-m");

    assert!(set_session_realtime_model(&conn, "sess-a", None).unwrap());
    assert_eq!(rt("sess-a"), "prov:session-m");
    assert!(set_session_model(&conn, "sess-a", None).unwrap());
    assert_eq!(rt("sess-a"), "prov:agent-m");
    d1080_agent(&conn, "a1", None);
    assert_eq!(rt("sess-a"), "srv:default");
}

/// D-1081: a missing session or a failed realtime lookup is an error, never a fallback.
#[test]
fn realtime_model_lookup_failures_are_errors() {
    let conn = setup();
    d1080_agent(&conn, "a1", Some("prov:agent-m"));
    assert!(effective_realtime_model_for_session(&conn, "a1", "missing", "srv:default").is_err());
    assert!(get_session_realtime_model(&conn, "missing").is_err());
    assert!(!set_session_realtime_model(&conn, "missing", Some("prov:light")).unwrap());

    d1080_session(&conn, "sess-a", r#"["a1"]"#);
    conn.execute_batch("ALTER TABLE sessions RENAME COLUMN realtime_model TO realtime_gone")
        .unwrap();
    assert!(effective_realtime_model_for_session(&conn, "a1", "sess-a", "srv:default").is_err());
}
