// ==================== MockLlmProvider ====================

/// A mock LLM provider that returns pre-queued responses.
struct MockLlmProvider {
    responses: Mutex<VecDeque<ChatResponse>>,
    /// 受け取った各リクエストの system prompt（連結）。何ターン目にどんな system prompt で
    /// 呼ばれたかを検証する（subtask 完了 resume の注入マーカーの確認に使う）。
    system_prompts: Mutex<Vec<String>>,
}

impl MockLlmProvider {
    fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            system_prompts: Mutex::new(Vec::new()),
        }
    }

    /// これまでに受け取ったリクエストの system prompt 一覧（呼ばれた順）。
    fn system_prompts(&self) -> Vec<String> {
        self.system_prompts.lock().unwrap().clone()
    }

    fn push_text_response(&self, text: &str) {
        let response = ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant(text),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            created: 0,
        };
        self.responses.lock().unwrap().push_back(response);
    }

    fn push_tool_call_response(&self, tool_calls: Vec<ToolCall>) {
        let mut msg = Message {
            role: Role::Assistant,
            content: None,
            name: None,
            function_call: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        };
        let _ = &mut msg; // suppress unused_mut

        let response = ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: msg,
                finish_reason: Some(FinishReason::ToolCalls),
            }],
            usage: Usage::default(),
            created: 0,
        };
        self.responses.lock().unwrap().push_back(response);
    }

    /// content（本文テキスト）と tool_calls を **同時に** 返す 1 生成。text+tool 併記で
    /// on_tool_call フックが content つきで発火する経路を通す（#899 ガードの検証用）。
    fn push_text_and_tool_call_response(&self, text: &str, tool_calls: Vec<ToolCall>) {
        let msg = Message {
            role: Role::Assistant,
            content: Some(MessageContent::Text(text.to_string())),
            name: None,
            function_call: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        };
        let response = ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: msg,
                finish_reason: Some(FinishReason::ToolCalls),
            }],
            usage: Usage::default(),
            created: 0,
        };
        self.responses.lock().unwrap().push_back(response);
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockLlmProvider {
    fn name(&self) -> &str {
        "mock"
    }

    // #676: このモックは max_tokens を無視するので「送らない」を宣言し、run 経路の
    // 出力上限モデル登録（fail loud）の対象外にする。上限の解決/ゲートは context_budget /
    // skill_engine の専用テスト、および明示 register する API ゲートテストで担保する。
    fn sends_max_output_tokens(&self) -> bool {
        false
    }

    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }

    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        // キューが尽きていても記録は残す（何回どんな system prompt で呼ばれたかが証拠）。
        let system = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .filter_map(|m| m.text_content())
            .collect::<Vec<_>>()
            .join("\n");
        self.system_prompts.lock().unwrap().push(system);

        let mut queue = self.responses.lock().unwrap();
        queue
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("MockLlmProvider: no more queued responses"))
    }
}

// ==================== LLM-integrated helpers ====================

/// #826: 予算 envelope の fail-loud を満たすため、テスト mock モデルの
/// `context_window` / `max_output_tokens` を `model_pricing` に登録する。
fn register_mock_model_pricing(db: &opencrab_db::Db, provider: &str, model: &str) {
    let conn = db.lock().unwrap();
    opencrab_db::queries::upsert_model_pricing(
        &conn,
        &opencrab_db::queries::ModelPricingRow {
            provider: provider.to_string(),
            model: model.to_string(),
            input_price_per_1m: 0.0,
            output_price_per_1m: 0.0,
            context_window: Some(200_000),
            max_output_tokens: Some(4_096),
        },
    )
    .expect("test model_pricing");
}

/// Create test app with a MockLlmProvider registered in the LlmRouter.
/// Returns (Router, opencrab_db::Db, Arc<MockLlmProvider>).
fn create_test_app_with_llm() -> (Router, opencrab_db::Db, Arc<MockLlmProvider>) {
    let (app, db, mock, _state) = create_test_app_with_state();
    (app, db, mock)
}

/// `create_test_app_with_llm` の `AppState` も返す版（#169）。
/// dispatch registry のような「ハンドラとテストが共有するランタイム状態」を検証するのに使う。
fn create_test_app_with_state() -> (Router, opencrab_db::Db, Arc<MockLlmProvider>, AppState) {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    // #826: 予算 envelope は fail-loud。既定 mock モデルの context_window / max_output_tokens を
    // model_pricing に登録しておかないと、ターンを回すテストが予算計算で弾かれる。
    register_mock_model_pricing(&db, "mock", "gpt-4o");

    let mock = Arc::new(MockLlmProvider::new());
    let mut router = LlmRouter::new();
    router.add_provider(mock.clone() as Arc<dyn LlmProvider>);
    router.set_default_provider("mock");

    let state = AppState {
        db: db.clone(),
        llm_router: opencrab_server::SharedLlmRouter::new(router),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir()
            .join("opencrab_test")
            .to_string_lossy()
            .to_string(),
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
    let app = create_router(state.clone());
    (app, db, mock, state)
}

/// Create a named agent with a specific persona via the API.
async fn create_test_agent_named(app: Router, name: &str, persona: &str) -> (String, Router) {
    let (_, resp) = send_request(
        app.clone(),
        "POST",
        "/api/agents",
        Some(serde_json::json!({
            "name": name,
            "persona_name": persona
        })),
    )
    .await;
    let agent_id = resp["id"].as_str().unwrap().to_string();
    (agent_id, app)
}

