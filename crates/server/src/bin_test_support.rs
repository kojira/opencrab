//! bin（`scheduler` / `intake_process`）専用のテスト土台。
//!
//! lib の `test_app_state`（`pub(crate)`）は別クレートの bin から見えないため、#899 §12.6
//! 「scheduler / intake 経路でも NO_REPLY のみは speech 保存しない」を検証するための最小
//! `AppState` をここで組む。フィールドは lib の `test_app_state` と同型（追随箇所は 1 つ）。
//!
//! `#[cfg(test)] mod bin_test_support;`（main.rs）で gate 済みのため、ここでは再指定しない。

use std::sync::Arc;

use async_trait::async_trait;
use opencrab_llm::message::{ChatRequest, ChatResponse, Choice, FinishReason, Message, Usage};
use opencrab_llm::router::LlmRouter;
use opencrab_llm::traits::{LlmProvider, ModelInfo};
use opencrab_server::{
    config, disconnected_heartbeat_config_rx, register_production_descriptors, subtask_registries,
    AppState, SharedLlmRouter,
};

/// 常に固定テキストを返す最小 mock。生成回数も数える。
pub(crate) struct FixedTextMock {
    text: String,
    calls: std::sync::Mutex<usize>,
}

impl FixedTextMock {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            calls: std::sync::Mutex::new(0),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl LlmProvider for FixedTextMock {
    fn name(&self) -> &str {
        "mock"
    }

    fn sends_max_output_tokens(&self) -> bool {
        false
    }

    async fn available_models(&self) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(vec![])
    }

    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        *self.calls.lock().unwrap() += 1;
        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: "mock-model".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant(&self.text),
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
        })
    }
}

/// mock provider を積んだ最小 `AppState`（in-memory DB・pricing 登録済み・agent 1 体 seed）。
///
/// `default_model = "mock:gpt-4o"` と pricing（`mock`/`gpt-4o`）を揃え、予算 envelope の
/// fail-loud を満たす。フィールドの並びは lib の `test_app_state` に追随させる。
pub(crate) fn app_state_with_agent(provider: Arc<dyn LlmProvider>, agent_id: &str) -> AppState {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_model_pricing(
            &conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: "mock".to_string(),
                model: "gpt-4o".to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: Some(200_000),
                max_output_tokens: Some(4_096),
            },
        )
        .expect("test model_pricing");
        opencrab_db::queries::upsert_agent(
            &conn,
            &opencrab_db::queries::AgentRow {
                agent_id: agent_id.to_string(),
                name: "BINTEST".to_string(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "p".to_string(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .expect("seed agent");
    }

    let mut router = LlmRouter::new();
    router.add_provider(provider);
    router.set_default_provider("mock");

    let timed_fire_router = opencrab_actions::TimedFireRouter::new();
    register_production_descriptors(&timed_fire_router);

    AppState {
        db,
        llm_router: SharedLlmRouter::new(router),
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
        evaluator: config::EvaluatorConfig::default(),
        skill_consolidation: config::SkillConsolidationConfig::default(),
        category_maintenance: config::CategoryMaintenanceConfig::default(),
        memory_organize: config::MemoryOrganizeConfig::default(),
        memory_declare: config::MemoryDeclareConfig::default(),
        memory_condense: config::MemoryCondenseConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: Arc::new(dashmap::DashMap::new()),
        intake: Arc::new(config::IntakeConfig::default()),
        intake_wake: Arc::new(tokio::sync::Notify::new()),
        mcp_manager: None,
        gateways: Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        subtask_registries: Arc::new(subtask_registries::SubtaskRegistries::new()),
        session_locks: Arc::new(opencrab_actions::SessionLocks::new()),
        timed_fire_router: Arc::new(timed_fire_router),
        progress_debounce: Arc::new(subtask_registries::ProgressDebounce::new()),
        subtask_notifiers: Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
        heartbeat_limits: config::HeartbeatLimits::default(),
        scheduler_wake: Arc::new(tokio::sync::Notify::new()),
        heartbeat_config_rx: disconnected_heartbeat_config_rx(
            opencrab_core::heartbeat::HeartbeatConfig::default(),
        ),
    }
}

/// 指定セッションの「`content='NO_REPLY'` の agent speech 行」数（#899 の観測点）。
pub(crate) fn count_no_reply_speech(state: &AppState, session_id: &str, agent_id: &str) -> usize {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
        .unwrap()
        .into_iter()
        .filter(|l| {
            l.log_type == "speech"
                && l.content == "NO_REPLY"
                && l.speaker_id.as_deref() == Some(agent_id)
        })
        .count()
}
