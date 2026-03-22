use std::sync::{Arc, Mutex, RwLock};

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub mod api;
pub mod config;
pub mod hot_reload;
pub mod llm_adapter;
pub mod process;

#[cfg(feature = "discord")]
mod agent_runner_impl;

use opencrab_llm::router::LlmRouter;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub llm_router: Arc<LlmRouter>,
    pub workspace_base: String,
    pub default_model: String,
    pub tools_config: Arc<RwLock<opencrab_actions::tools::ToolsConfig>>,
    #[cfg(feature = "discord")]
    pub discord_manager: Option<Arc<opencrab_discord::DiscordGatewayManager<AppState>>>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        // エージェント管理
        .route("/api/agents", get(api::agents::list_agents).post(api::agents::create_agent))
        .route("/api/agents/{id}", get(api::agents::get_agent).delete(api::agents::delete_agent))
        .route("/api/agents/{id}/soul", get(api::agents::get_soul).put(api::agents::update_soul))
        .route("/api/agents/{id}/identity", get(api::agents::get_identity).put(api::agents::update_identity))
        // ペルソナプリセット
        .route("/api/agents/{id}/soul/presets", get(api::agents::list_soul_presets).post(api::agents::create_soul_preset))
        .route("/api/agents/{id}/soul/presets/{preset_id}", axum::routing::delete(api::agents::delete_soul_preset))
        .route("/api/agents/{id}/soul/presets/{preset_id}/apply", post(api::agents::apply_soul_preset))
        // スキル管理
        .route("/api/agents/{id}/skills", get(api::skills::list_skills).post(api::skills::add_skill))
        .route("/api/agents/{id}/skills/{skill_id}", put(api::skills::update_skill))
        .route("/api/agents/{id}/skills/{skill_id}/toggle", post(api::skills::toggle_skill))
        .route("/api/agents/{id}/skills/{skill_id}/archive", post(api::skills::archive_skill))
        .route("/api/agents/{id}/skills/{skill_id}/restore", post(api::skills::restore_skill))
        .route("/api/agents/{id}/skills/merge", post(api::skills::merge_skills))
        .route("/api/agents/{id}/skills/duplicates", get(api::skills::list_duplicates))
        .route("/api/agents/{id}/skills/unused", get(api::skills::list_unused))
        // 記憶管理
        .route("/api/agents/{id}/memory/curated", get(api::memory::list_curated_memory))
        .route("/api/agents/{id}/memory/search", post(api::memory::search_memory))
        .route("/api/agents/{id}/memory/index", get(api::agents::get_memory_index_status).post(api::agents::trigger_memory_index_build).delete(api::agents::delete_memory_index))
        .route("/api/agents/{id}/memory/index/config", put(api::agents::update_memory_index_config))
        .route("/api/agents/{id}/memory/index/rebuild", post(api::agents::rebuild_memory_index))
        .route("/api/agents/{id}/memory/index/merge", post(api::agents::merge_memory_index_topics))
        // セッション管理
        .route("/api/sessions", get(api::sessions::list_sessions).post(api::sessions::create_session))
        .route("/api/sessions/{id}", get(api::sessions::get_session))
        .route("/api/sessions/{id}/messages", post(api::sessions::send_message))
        .route("/api/sessions/{id}/logs", get(api::sessions::list_session_logs))
        .route("/api/sessions/{id}/mentor", post(api::sessions::send_mentor_instruction))
        // アナリティクス
        .route("/api/agents/{id}/analytics", get(api::analytics::get_metrics_summary))
        .route("/api/agents/{id}/analytics/detail", get(api::analytics::get_metrics_detail))
        // ワークスペース管理
        .route("/api/agents/{id}/workspace", get(api::workspace::list_workspace))
        .route("/api/agents/{id}/workspace/{*path}", get(api::workspace::read_file).put(api::workspace::write_file))
        // Discord per-agent config (always available; gateway ops require discord feature)
        .route(
            "/api/agents/{id}/discord",
            get(api::agents::get_discord_config)
                .put(api::agents::update_discord_config)
                .delete(api::agents::delete_discord_config),
        )
        .route("/api/agents/{id}/discord/start", post(api::agents::start_discord_gateway))
        .route("/api/agents/{id}/discord/stop", post(api::agents::stop_discord_gateway))
        // Co-Agent管理
        .route("/api/agents/{id}/co-agents", get(api::co_agents::list_co_agents).post(api::co_agents::add_co_agent))
        .route("/api/agents/{id}/co-agents/{co_agent_id}", axum::routing::patch(api::co_agents::update_co_agent).delete(api::co_agents::delete_co_agent))
        // チャンネル設定
        .route("/api/agents/{id}/channel-configs", get(api::channel_configs::list_channel_configs).put(api::channel_configs::upsert_channel_config))
        .route("/api/agents/{id}/channel-configs/{channel_id}", delete(api::channel_configs::delete_channel_config))
        .route("/api/agents/{id}/trusted-users", get(api::trusted_users::list_trusted_users).post(api::trusted_users::add_trusted_user))
        .route("/api/agents/{id}/trusted-users/{user_id}", axum::routing::patch(api::trusted_users::update_trusted_user).delete(api::trusted_users::delete_trusted_user))
        // エージェントメッセージ
        .route("/api/agents/{id}/messages", post(api::agents_messages::send_agent_message))
        // 許可コマンド管理
        .route("/api/agents/{id}/allowed-commands", get(api::allowed_commands::list_allowed_commands).post(api::allowed_commands::add_allowed_command))
        .route("/api/agents/{id}/allowed-commands/{command}", axum::routing::delete(api::allowed_commands::remove_allowed_command))
        // LLMログ
        .route("/api/agents/{id}/llm-logs", get(api::llm_logs::list_llm_logs))
        // インポート
        .route("/api/import/scan", post(api::import::scan_workspace_handler))
        .route("/api/import/execute", post(api::import::execute_import_handler))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health_check() -> &'static str {
    "ok"
}
