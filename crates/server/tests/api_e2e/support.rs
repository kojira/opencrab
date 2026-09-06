/// Create test app using the REAL server router (same as production).
fn create_test_app() -> Router {
    create_test_app_with_db().0
}

/// `create_test_app` と同じアプリを作り、同じ DB ハンドルも返す。
///
/// API を経由せずに行を書き込みたいテスト（入口の正規化が入る前に保存された
/// レガシー行の再現）で使う。
fn create_test_app_with_db() -> (Router, opencrab_db::Db) {
    let (state, db) = create_test_state(0.5);
    (create_router(state), db)
}

/// 既定ヘルパと同じ `AppState` を組むが、`compaction_ratio` だけ引数で差し替える。
/// compaction_ratio を state から読んでいる経路（例: model_pricing 一覧 API）を
/// 恒真にならない値で検証するために使う。
fn create_test_state(compaction_ratio: f64) -> (AppState, opencrab_db::Db) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let state = AppState {
        db: db.clone(),
        llm_router: opencrab_server::SharedLlmRouter::new(LlmRouter::new()),
        // 既定を明示的に置かない（default_provider = ""）。テストの base は provider 未定義で、
        // provider を DB オーバーライドで足す reload 経路（build_llm_router）が、コード既定の
        // sentinel "openai" と食い違って default_provider 検証で弾かれるのを避ける（#660）。
        // 空 default_provider は「既定なし＝未設定」として検証をスキップする正規の状態。
        llm_config: Arc::new(toml::from_str("default_provider = \"\"").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        #[cfg(feature = "nostr")]
        nostr_master_key: None,
        default_model: "mock:test".to_string(),
        tools_config: Arc::new(std::sync::RwLock::new(
            opencrab_actions::tools::ToolsConfig::default(),
        )),
        compaction_ratio,
        typed_history_enabled: false,
        typed_history_drop_directive: false,
        evaluator: opencrab_server::config::EvaluatorConfig::default(),
        skill_consolidation: opencrab_server::config::SkillConsolidationConfig::default(),
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
    };
    (state, db)
}

// ==================== Helper ====================

async fn send_request(
    app: Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let body = match body {
        Some(json) => Body::from(serde_json::to_vec(&json).unwrap()),
        None => Body::empty(),
    };

    let mut builder = Request::builder().uri(uri);
    builder = match method {
        "GET" => builder.method("GET"),
        "POST" => builder.method("POST"),
        "PUT" => builder.method("PUT"),
        "PATCH" => builder.method("PATCH"),
        "DELETE" => builder.method("DELETE"),
        _ => panic!("unsupported method"),
    };

    if method == "POST" || method == "PUT" || method == "PATCH" {
        builder = builder.header("content-type", "application/json");
    }

    let req = builder.body(body).unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&body_bytes).unwrap_or(serde_json::json!(body_bytes.to_vec()));
    (status, json)
}

/// Create an agent via API and return its ID.
async fn create_test_agent(app: Router) -> (String, Router) {
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": "Test Agent",
            "persona_name": "TestPersona"
        })),
    )
    .await;
    let agent_id = resp["id"].as_str().unwrap().to_string();
    (agent_id, app)
}

