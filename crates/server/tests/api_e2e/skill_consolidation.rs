// ==================== Skill consolidation (sleep curation) ====================

fn state_with_consolidation(
    db: opencrab_db::Db,
    mock: Arc<MockLlmProvider>,
    cfg: opencrab_server::config::SkillConsolidationConfig,
) -> AppState {
    // #826: 予算 fail-loud のため既定 mock モデルを登録（呼び出し側の db は未登録のことがある）。
    register_mock_model_pricing(&db, "mock", "gpt-4o");
    let mut router = LlmRouter::new();
    router.add_provider(mock as Arc<dyn LlmProvider>);
    router.set_default_provider("mock");
    AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(router),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:gpt-4o".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio: 0.5,
        typed_history_enabled: false,
        typed_history_drop_directive: false,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: cfg,
        category_maintenance: opencrab_server::config::CategoryMaintenanceConfig::default(),
        memory_organize: opencrab_server::config::MemoryOrganizeConfig::default(),
        memory_declare: opencrab_server::config::MemoryDeclareConfig::default(),
        memory_condense: opencrab_server::config::MemoryCondenseConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: std::sync::Arc::new(dashmap::DashMap::new()),
        intake: std::sync::Arc::new(Default::default()),
        intake_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        mcp_manager: None,
        gateways: std::sync::Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        subtask_registries: std::sync::Arc::new(
            opencrab_server::subtask_registries::SubtaskRegistries::new(),
        ),
        session_locks: std::sync::Arc::new(opencrab_actions::SessionLocks::new()),
        subtask_notifiers: std::sync::Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: std::sync::Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
        heartbeat_limits: Default::default(),
        scheduler_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        heartbeat_config_rx: opencrab_server::disconnected_heartbeat_config_rx(Default::default()),
        timed_fire_router: std::sync::Arc::new(opencrab_actions::TimedFireRouter::new()),
        progress_debounce: std::sync::Arc::new(
            opencrab_server::subtask_registries::ProgressDebounce::new(),
        ),
    }
}

#[tokio::test]
async fn test_skill_consolidation_disabled_is_noop() {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let mock = Arc::new(MockLlmProvider::new());
    // enabled=false（既定）→ 何もせず false
    let state = state_with_consolidation(
        db,
        mock,
        opencrab_server::config::SkillConsolidationConfig::default(),
    );
    let ran = opencrab_server::skill_consolidation::maybe_run_skill_consolidation(&state, "a1")
        .await
        .unwrap();
    assert!(!ran);
}

#[tokio::test]
async fn test_skill_consolidation_curates_and_audits() {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let mock = Arc::new(MockLlmProvider::new());
    // 本人が「Old を retire、New を create」する判断を返す
    mock.push_text_response(
        r#"[{"name":"Old","action":"retire","reason":"もう使わない"},
            {"name":"New","action":"create","reason":"最近こう動きたい","description":"新スキル","guidance":"こうする"}]"#,
    );

    let cfg = opencrab_server::config::SkillConsolidationConfig {
        enabled: true,
        trigger_new_sessions: 1,
        time_cap_hours: 1,
        min_interval_secs: 0,
        include_archived_in_review: 3,
    };
    let state = state_with_consolidation(db.clone(), mock, cfg);

    {
        let conn = db.lock().unwrap();
        // エージェント + 既存スキル Old
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: "a1".into(),
                name: "A".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "Persona".into(),
                personality: Some("好奇心旺盛".into()),
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
        opencrab_db::queries::insert_skill(
            &conn,
            &opencrab_db::queries::SkillRow {
                id: "sk-old".into(),
                agent_id: "a1".into(),
                name: "Old".into(),
                description: "d".into(),
                situation_pattern: String::new(),
                guidance: "g".into(),
                source_type: "self_created".into(),
                source_context: None,
                file_path: None,
                effectiveness: None,
                usage_count: 0,
                is_active: true,
                permission: "\"agent\"".into(),
                archived: false,
                created_caller: None,
                agent_visible: false,
            },
        )
        .unwrap();
        // 過去に棚卸し済み（cold-start シードを回避してすぐ発火させる）
        opencrab_db::queries::set_last_skill_consolidation_at(
            &conn,
            "a1",
            "2020-01-01T00:00:00+00:00",
        )
        .unwrap();
        // 新規活動（トリガの母数）
        opencrab_db::queries::insert_session_log(
            &conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "a1".into(),
                session_id: "sess-1".into(),
                log_type: "speech".into(),
                content: "hi".into(),
                speaker_id: Some("a1".into()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    let ran = opencrab_server::skill_consolidation::maybe_run_skill_consolidation(&state, "a1")
        .await
        .unwrap();
    assert!(ran, "consolidation should have fired");

    let conn = db.lock().unwrap();
    // Old は archived（active から消える）、New は作成されて active
    let active = opencrab_db::queries::list_skills(&conn, "a1", true).unwrap();
    let names: Vec<_> = active.iter().map(|s| s.name.as_str()).collect();
    assert!(!names.contains(&"Old"), "Old should be retired: {names:?}");
    assert!(names.contains(&"New"), "New should be created: {names:?}");
    // 監査ログ層1（agent_logs, context=sleep）
    let logs = opencrab_db::queries::list_agent_logs(&conn, Some("a1"), None, 10).unwrap();
    assert!(
        logs.iter().any(|l| l.context == "sleep"),
        "sleep audit log missing"
    );
    // last_at が前進している
    let last = opencrab_db::queries::get_last_skill_consolidation_at(&conn, "a1").unwrap();
    assert!(last.is_some() && last.as_deref() != Some("2020-01-01T00:00:00+00:00"));
}

