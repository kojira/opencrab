//! エージェントのメッセージ処理に関する共通ロジック。
//!
//! REST API (`api/sessions.rs`) と Discordゲートウェイ (`discord.rs`) の
//! 両方から利用される。

use std::sync::Arc;

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

    let role = identity
        .as_ref()
        .map(|i| i.role.clone())
        .unwrap_or_else(|| "discussant".to_string());

    let persona = soul
        .as_ref()
        .map(|s| s.persona_name.clone())
        .unwrap_or_default();

    let custom_traits = soul
        .as_ref()
        .and_then(|s| s.custom_traits_json.clone())
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

    let prompt = format!(
        "You are {agent_name} ({persona}), role: {role}.\n\
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
         Just reply with the message content directly.{skills_text}{character_section}"
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
    gateway_admin: Option<Arc<dyn opencrab_actions::GatewayAdmin>>,
) -> anyhow::Result<opencrab_core::EngineResult> {
    // Build workspace path for this agent.
    let ws_path = format!("{}/{}", state.workspace_base, agent_id);
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
        agent_id: agent_id.to_string(),
        agent_name: agent_name.to_string(),
        session_id: Some(session_id.to_string()),
        db: state.db.clone(),
        workspace: Arc::new(workspace),
        last_metrics_id: last_metrics_id.clone(),
        model_override: model_override.clone(),
        current_purpose: current_purpose.clone(),
        runtime_info: Arc::new(std::sync::Mutex::new(runtime_info)),
        gateway_admin,
    };
    let dispatcher = opencrab_actions::ActionDispatcher::new();
    let executor = opencrab_actions::BridgedExecutor::new(dispatcher, ctx);

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

    // Run SkillEngine with model_override for dynamic switching.
    let engine = opencrab_core::SkillEngine::new(
        Box::new(llm_client),
        Box::new(executor),
        5, // max iterations
    );

    engine
        .run_with_model_override(
            system_prompt,
            conversation,
            &state.default_model,
            Some(model_override),
        )
        .await
}
