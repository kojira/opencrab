//! エージェントのメッセージ処理に関する共通ロジック。
//!
//! REST API (`api/sessions.rs`) と Discordゲートウェイ (`discord.rs`) の
//! 両方から利用される。

use std::sync::Arc;

use opencrab_gateway::GatewayActions;
use opencrab_llm::pricing::PricingRegistry;

use crate::llm_adapter::{LlmRouterAdapter, MetricsContext};
use crate::AppState;

/// DBからエージェントのidentity/soul/skillsを読み込んでシステムプロンプトを構築する。
///
/// 返り値: (system_prompt, agent_name)
pub fn build_agent_context(
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_theme: &str,
) -> (String, String) {
    let identity = opencrab_db::queries::get_identity(conn, agent_id)
        .ok()
        .flatten();
    let soul = opencrab_db::queries::get_soul(conn, agent_id).ok().flatten();
    let skills = opencrab_db::queries::list_skills(conn, agent_id, true).unwrap_or_default();

    let agent_name = identity
        .as_ref()
        .map(|i| i.name.clone())
        .unwrap_or_else(|| agent_id.to_string());

    let persona = soul
        .as_ref()
        .map(|s| s.persona_name.clone())
        .unwrap_or_default();

    let custom_traits = soul
        .as_ref()
        .and_then(|s| s.personality.clone())
        .unwrap_or_default();

    let instructions = soul
        .as_ref()
        .map(|s| s.instructions.clone())
        .unwrap_or_default();

    let skills_text = if skills.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect();
        format!("\n\nYour skills:\n{}", list.join("\n"))
    };

    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");

    let character_section = if custom_traits.is_empty() {
        String::new()
    } else {
        format!("\n\n{custom_traits}")
    };

    let instructions_section = if instructions.is_empty() {
        String::new()
    } else {
        format!("\n\n## Instructions\n{instructions}")
    };

    let prompt = format!(
        "You are {agent_name} ({persona}).\n\
         Current date and time: {now}\n\
         Current discussion topic: {session_theme}\n\
         \n\
         You are an autonomous agent participating in a discussion. \
         Respond thoughtfully to the conversation. \
         You can use tools to search your history, learn from experience, \
         create new skills, and manage your workspace.\n\
         \n\
         The conversation history uses the format \"[speaker]: message\" for context, \
         but you must NOT include your own name prefix in your response. \
         Just reply with the message content directly.\n\
         \n\
         あなたは複数のアクションを順番に計画・実行できます。\
         例えば「Xを調べてYを設定する」という指示に対して、\
         1. execute_shell で情報収集、\
         2. 結果を解析、\
         3. add_allowed_command でコマンド追加、\
         4. create_my_skill でスキル作成、\
         のように、複数のアクションを連続して呼び出してください。\n\
         \n\
         ## Silent Reply\n\
         返答不要な場合は NO_REPLY とだけテキストで返してください（他のテキストと混在させない）:\n\
         - グループチャットで自分に関係ない会話の場合\n\
         - 他のBotが話している場合（Bot同士のループを防ぐ）\n\
         - 既に話が完結している場合\n\
         \n\
         {skills_text}{character_section}{instructions_section}"
    );

    (prompt, agent_name)
}

/// セッションログから会話文字列を構築する。
pub fn build_conversation_string(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> String {
    let logs =
        opencrab_db::queries::list_session_logs_by_session(conn, session_id).unwrap_or_default();

    if logs.is_empty() {
        return "No messages yet.".to_string();
    }

    let mut parts = Vec::new();
    for log in &logs {
        let speaker = log
            .speaker_id
            .as_deref()
            .unwrap_or(&log.agent_id);
        parts.push(format!("[{}]: {}", speaker, log.content));
    }

    parts.join("\n")
}

/// エージェントにメッセージを処理させ、応答テキストを返す。
///
/// SkillEngine + BridgedExecutor + LlmRouterAdapter のフルパイプラインを実行する。
pub async fn run_agent_response(
    state: &AppState,
    agent_id: &str,
    agent_name: &str,
    session_id: &str,
    system_prompt: &str,
    conversation: &str,
    gateway: &str,
    gateway_actions: Option<Arc<dyn GatewayActions>>,
    caller: opencrab_actions::CallerIdentity,
    image_urls: &[String],
    depth: u32,
    on_first_response: Option<Box<dyn FnOnce(String) + Send>>,
) -> anyhow::Result<opencrab_core::EngineResult> {
    // Build workspace path for this agent.
    let ws_path = state.workspace_base.replace("{agent_id}", agent_id);
    std::fs::create_dir_all(&ws_path).ok();
    let workspace =
        opencrab_core::workspace::Workspace::from_root(std::path::Path::new(&ws_path))?;

    // Create BridgedExecutor with ActionContext.
    let last_metrics_id = Arc::new(std::sync::Mutex::new(None));
    let model_override = Arc::new(std::sync::Mutex::new(None));
    let current_purpose =
        Arc::new(std::sync::Mutex::new("conversation".to_string()));

    let runtime_info = opencrab_actions::RuntimeInfo {
        default_model: state.default_model.clone(),
        active_model: model_override.lock().unwrap().clone(),
        available_providers: state.llm_router.provider_names().into_iter().map(String::from).collect(),
        gateway: gateway.to_string(),
    };

    let ctx = opencrab_actions::ActionContext {
        caller,
        agent_id: agent_id.to_string(),
        agent_name: agent_name.to_string(),
        session_id: Some(session_id.to_string()),
        db: state.db.clone(),
        workspace: Arc::new(workspace),
        last_metrics_id: last_metrics_id.clone(),
        model_override: model_override.clone(),
        current_purpose: current_purpose.clone(),
        runtime_info: Arc::new(std::sync::Mutex::new(runtime_info)),
    };
    let mut dispatcher = opencrab_actions::ActionDispatcher::new();
    let tools_cfg = state.tools_config.read().unwrap().clone();
    opencrab_actions::register_tools_from_config(&tools_cfg, &mut dispatcher);
    let executor = {
        let bridged = opencrab_actions::BridgedExecutor::new(dispatcher, ctx)
            .with_depth(depth);
        match gateway_actions {
            Some(ga) => bridged.with_gateway_actions(ga),
            None => bridged,
        }
    };

    // Create LlmRouterAdapter with metrics recording.
    let metrics_ctx = MetricsContext {
        db: state.db.clone(),
        agent_id: agent_id.to_string(),
        session_id: Some(session_id.to_string()),
        pricing: PricingRegistry::default(),
        last_metrics_id: last_metrics_id.clone(),
        current_purpose: current_purpose.clone(),
    };
    let llm_client = LlmRouterAdapter::new(state.llm_router.clone()).with_metrics(metrics_ctx);

    // Main engine: 30 iterations max. Sub-engines: unlimited (timeout-controlled).
    let max_iterations = if depth == 0 { 30 } else { usize::MAX };
    let mut engine = opencrab_core::SkillEngine::new(
        Box::new(llm_client),
        Box::new(executor),
        max_iterations,
    );

    // LLMログ記録のコールバックを設定
    let log_db = state.db.clone();
    let log_agent_id = agent_id.to_string();
    let log_session_id = session_id.to_string();
    engine.set_log_callback(move |request, response| {
        let log_row = opencrab_db::queries::LlmLogRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: log_agent_id.clone(),
            session_id: Some(log_session_id.clone()),
            model: Some(request.model.clone()),
            prompt: serde_json::to_string(&request).unwrap_or_default(),
            response: serde_json::to_string(response).unwrap_or_default(),
            tool_calls: if response.tool_calls.is_empty() {
                None
            } else {
                serde_json::to_string(&response.tool_calls).ok()
            },
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Ok(conn) = log_db.lock() {
            let _ = opencrab_db::queries::insert_llm_log(&conn, &log_row);
        }
    });

    // Set optional first-response callback (for immediate Discord acknowledgment).
    if let Some(cb) = on_first_response {
        engine.set_on_first_response(cb);
    }

    // Collect image_urls: merge passed-in args with any stored in the latest user log metadata_json.
    let merged_image_urls: Vec<String> = {
        let mut urls: Vec<String> = image_urls.to_vec();
        if let Ok(conn) = state.db.lock() {
            if let Ok(logs) = opencrab_db::queries::list_session_logs_by_session(&conn, session_id) {
                if let Some(latest_user_log) = logs.iter().rev().find(|log| {
                    log.log_type == "speech"
                        && log.speaker_id.as_deref().map(|s| s != agent_id).unwrap_or(true)
                }) {
                    if let Some(ref meta_json) = latest_user_log.metadata_json {
                        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(meta_json) {
                            if let Some(arr) = meta["image_urls"].as_array() {
                                for v in arr {
                                    if let Some(s) = v.as_str() {
                                        let url = s.to_string();
                                        if !urls.contains(&url) {
                                            urls.push(url);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !urls.is_empty() {
            tracing::debug!(
                session_id = %session_id,
                count = urls.len(),
                "run_agent_response: merging image_urls for LLM"
            );
        }
        urls
    };

    let result = engine
        .run_with_model_override(
            system_prompt,
            conversation,
            &state.default_model,
            Some(model_override),
            &merged_image_urls,
        )
        .await;

    // インデックス自動構築チェック（バックグラウンド）
    {
        let index_db = state.db.clone();
        let index_agent_id = agent_id.to_string();
        let index_llm_router = state.llm_router.clone();
        let index_model = state.default_model.clone();
        tokio::spawn(async move {
            let (unindexed, config) = {
                let Ok(conn) = index_db.lock() else { return };
                let unindexed = opencrab_db::queries::get_unindexed_log_count(&conn, &index_agent_id)
                    .unwrap_or(0);
                let config = opencrab_db::queries::get_memory_index_config(&conn, &index_agent_id)
                    .unwrap_or_else(|_| opencrab_db::queries::AgentMemoryIndexConfig {
                        agent_id: index_agent_id.clone(),
                        batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                        threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                        updated_at: String::new(),
                    });
                (unindexed, config)
            };
            if unindexed < config.threshold {
                return;
            }
            tracing::info!(
                agent_id = %index_agent_id,
                unindexed = unindexed,
                threshold = config.threshold,
                batch_size = config.batch_size,
                "Starting background memory index build"
            );
            let llm_adapter = LlmRouterAdapter::new(index_llm_router);
            let _ = opencrab_core::memory_index::IndexBuilder::build_incremental(
                &index_db,
                &index_agent_id,
                &llm_adapter,
                &index_model,
                config.batch_size as usize,
            )
            .await;
        });
    }

    result
}
