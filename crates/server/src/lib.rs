use std::sync::{Arc, RwLock};

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub mod agent_log;
pub mod api;
pub mod config;
pub mod hot_reload;
pub mod llm_adapter;
pub mod memory_maintenance;
pub mod process;
pub mod skill_consolidation;

#[cfg(feature = "discord")]
mod agent_runner_impl;
pub mod transcript;

use opencrab_llm::router::LlmRouter;

/// ホットスワップ可能な LlmRouter の共有ハンドル。
///
/// ダッシュボードのプロバイダー設定変更時に、再起動なしでルーターを
/// 差し替えるために使う。読み手は `get()` でその時点のスナップショットを
/// 取得する（実行中のリクエストは古いルーターで完走する — 破壊的でない）。
#[derive(Clone)]
pub struct SharedLlmRouter(Arc<std::sync::RwLock<Arc<LlmRouter>>>);

impl SharedLlmRouter {
    pub fn new(router: LlmRouter) -> Self {
        Self(Arc::new(std::sync::RwLock::new(Arc::new(router))))
    }

    /// 現在のルーターのスナップショットを返す。
    pub fn get(&self) -> Arc<LlmRouter> {
        self.0.read().unwrap().clone()
    }

    /// ルーターを差し替える（プロバイダー設定変更時）。
    pub fn swap(&self, router: LlmRouter) {
        *self.0.write().unwrap() = Arc::new(router);
    }
}

#[derive(Clone)]
pub struct AppState {
    pub db: opencrab_db::Db,
    pub llm_router: SharedLlmRouter,
    /// 起動時に読んだ TOML の [llm] 設定（DB オーバーライド適用前の土台）。
    /// プロバイダー設定変更時のルーター再構築に使う。
    pub llm_config: Arc<config::LlmConfig>,
    /// 起動時に読んだ TOML の [voice] 設定（DB オーバーライド適用前の土台）。
    pub voice_config: Arc<opencrab_voice::VoiceConfig>,
    /// 稼働中の VC 対話ランタイム（discord 起動後にセットされる）。
    /// プロバイダー設定変更を再起動なしで反映するために使う。
    pub voice_runtime: Arc<std::sync::Mutex<Option<Arc<dyn opencrab_voice::VoiceRuntime>>>>,
    pub workspace_base: String,
    pub default_model: String,
    pub tools_config: Arc<RwLock<opencrab_actions::tools::ToolsConfig>>,
    /// コンパクション比率: context_window のうち会話履歴に使う割合 (0.0-1.0, デフォルト 0.5)。
    pub compaction_ratio: f64,
    /// verify 段（evaluator）の設定。
    pub evaluator: config::EvaluatorConfig,
    /// スリープ時スキル棚卸し（自己 curation ループ）の設定。
    pub skill_consolidation: config::SkillConsolidationConfig,
    /// ループ再起動 v1（#52）: 反復上限停止 + active タスク残存時の1回自動再実行。
    pub loop_restart_enabled: bool,
    /// エージェント単位のインデックスビルド in-flight フラグ（post-run トリガーと
    /// メンテナンスループの二重 LLM 支出防止）。
    pub index_build_inflight: memory_maintenance::IndexBuildInflight,
    #[cfg(feature = "discord")]
    pub discord_manager: Option<Arc<opencrab_discord::DiscordGatewayManager<AppState>>>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/api/health", get(api_health_check))
        // エージェント管理
        .route(
            "/api/agents",
            get(api::agents::list_agents).post(api::agents::create_agent),
        )
        .route(
            "/api/agents/{id}",
            get(api::agents::get_agent)
                .put(api::agents::put_agent)
                .patch(api::agents::patch_agent)
                .delete(api::agents::delete_agent),
        )
        // オンボーディング（初回セットアップ進捗の集約）
        .route("/api/setup/status", get(api::setup::get_setup_status))
        .route(
            "/api/agents/{id}/skills/seed-standard",
            post(api::setup::seed_standard_skills),
        )
        .route("/api/llm/model-choices", get(api::llm::model_choices))
        // プロバイダー設定（ダッシュボード編集 + ホットリロード）
        .route("/api/llm/providers", get(api::providers::list_providers))
        .route(
            "/api/llm/providers/reload",
            post(api::providers::reload_providers),
        )
        // codex 診断（サーバーが使う codex のパス/バージョン）
        .route(
            "/api/llm/codex/diagnostics",
            get(api::providers::codex_diagnostics),
        )
        .route(
            "/api/llm/providers/{name}",
            put(api::providers::update_provider),
        )
        .route(
            "/api/llm/providers/{name}/override",
            delete(api::providers::delete_provider_override),
        )
        .route(
            "/api/voice/config",
            get(api::providers::get_voice_config)
                .put(api::providers::update_voice_config)
                .delete(api::providers::delete_voice_config),
        )
        // ペルソナプリセット
        .route(
            "/api/agents/{id}/soul/presets",
            get(api::agents::list_soul_presets).post(api::agents::create_soul_preset),
        )
        .route(
            "/api/agents/{id}/soul/presets/{preset_id}",
            axum::routing::delete(api::agents::delete_soul_preset),
        )
        .route(
            "/api/agents/{id}/soul/presets/{preset_id}/apply",
            post(api::agents::apply_soul_preset),
        )
        // スキル管理
        .route(
            "/api/agents/{id}/skills",
            get(api::skills::list_skills).post(api::skills::add_skill),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}",
            put(api::skills::update_skill),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}/toggle",
            post(api::skills::toggle_skill),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}/archive",
            post(api::skills::archive_skill),
        )
        .route(
            "/api/agents/{id}/skills/{skill_id}/restore",
            post(api::skills::restore_skill),
        )
        .route(
            "/api/agents/{id}/skills/unused",
            get(api::skills::list_unused),
        )
        // 記憶管理
        .route(
            "/api/agents/{id}/memory/curated",
            get(api::memory::list_curated_memory),
        )
        .route(
            "/api/agents/{id}/memory/curated/{entry_id}",
            axum::routing::delete(api::memory::delete_curated_memory_entry),
        )
        .route(
            "/api/agents/{id}/memory/search",
            post(api::memory::search_memory),
        )
        .route(
            "/api/agents/{id}/memory/index",
            get(api::agents::get_memory_index_status)
                .post(api::agents::trigger_memory_index_build)
                .delete(api::agents::delete_memory_index),
        )
        .route(
            "/api/agents/{id}/memory/index/tree",
            get(api::memory::get_memory_index_tree),
        )
        .route(
            "/api/agents/{id}/memory/index/config",
            put(api::agents::update_memory_index_config),
        )
        .route(
            "/api/agents/{id}/memory/index/rebuild",
            post(api::agents::rebuild_memory_index),
        )
        .route(
            "/api/agents/{id}/daily-log-index/status",
            get(api::daily_log_index::get_status),
        )
        .route(
            "/api/agents/{id}/daily-log-index/rebuild",
            post(api::daily_log_index::rebuild),
        )
        .route(
            "/api/agents/{id}/daily-log-index/run",
            post(api::daily_log_index::run),
        )
        .route(
            "/api/agents/{id}/memory/index/merge",
            post(api::agents::merge_memory_index_topics),
        )
        // セッション管理
        .route(
            "/api/sessions",
            get(api::sessions::list_sessions).post(api::sessions::create_session),
        )
        .route("/api/sessions/{id}", get(api::sessions::get_session))
        .route(
            "/api/sessions/{id}/messages",
            post(api::sessions::send_message),
        )
        .route(
            "/api/sessions/{id}/logs",
            get(api::sessions::list_session_logs),
        )
        .route(
            "/api/sessions/{id}/mentor",
            post(api::sessions::send_mentor_instruction),
        )
        // アナリティクス
        .route(
            "/api/agents/{id}/analytics",
            get(api::analytics::get_metrics_summary),
        )
        .route(
            "/api/agents/{id}/analytics/detail",
            get(api::analytics::get_metrics_detail),
        )
        // ワークスペース管理
        .route(
            "/api/agents/{id}/workspace",
            get(api::workspace::list_workspace),
        )
        .route(
            "/api/agents/{id}/workspace/{*path}",
            get(api::workspace::read_file).put(api::workspace::write_file),
        )
        // Discord per-agent config (always available; gateway ops require discord feature)
        .route(
            "/api/agents/{id}/discord",
            get(api::agents::get_discord_config)
                .put(api::agents::update_discord_config)
                .patch(api::agents::patch_discord_config)
                .delete(api::agents::delete_discord_config),
        )
        .route(
            "/api/agents/{id}/discord/start",
            post(api::agents::start_discord_gateway),
        )
        .route(
            "/api/agents/{id}/discord/stop",
            post(api::agents::stop_discord_gateway),
        )
        // Co-Agent管理
        .route(
            "/api/agents/{id}/co-agents",
            get(api::co_agents::list_co_agents).post(api::co_agents::add_co_agent),
        )
        .route(
            "/api/agents/{id}/co-agents/{co_agent_id}",
            axum::routing::patch(api::co_agents::update_co_agent)
                .delete(api::co_agents::delete_co_agent),
        )
        // チャンネル設定
        .route(
            "/api/agents/{id}/channel-configs",
            get(api::channel_configs::list_channel_configs)
                .put(api::channel_configs::upsert_channel_config),
        )
        .route(
            "/api/agents/{id}/channel-configs/{channel_id}",
            delete(api::channel_configs::delete_channel_config),
        )
        .route(
            "/api/agents/{id}/trusted-users",
            get(api::trusted_users::list_trusted_users).post(api::trusted_users::add_trusted_user),
        )
        .route(
            "/api/agents/{id}/trusted-users/{user_id}",
            axum::routing::patch(api::trusted_users::update_trusted_user)
                .delete(api::trusted_users::delete_trusted_user),
        )
        // エージェントメッセージ
        .route(
            "/api/agents/{id}/messages",
            post(api::agents_messages::send_agent_message),
        )
        // 許可コマンド管理
        .route(
            "/api/agents/{id}/allowed-commands",
            get(api::allowed_commands::list_allowed_commands)
                .post(api::allowed_commands::add_allowed_command),
        )
        .route(
            "/api/agents/{id}/allowed-commands/{command}",
            axum::routing::delete(api::allowed_commands::remove_allowed_command),
        )
        // LLMログ
        .route(
            "/api/agents/{id}/llm-logs",
            get(api::llm_logs::list_llm_logs),
        )
        .route(
            "/api/agents/{id}/llm-logs/stats",
            get(api::llm_logs::llm_logs_stats),
        )
        // インポート
        .route(
            "/api/import/scan",
            post(api::import::scan_workspace_handler),
        )
        .route(
            "/api/import/execute",
            post(api::import::execute_import_handler),
        )
        .route(
            "/api/agents/{id}/import/sync/status",
            get(api::import_sync::get_sync_status),
        )
        .route(
            "/api/agents/{id}/import/sync",
            post(api::import_sync::execute_import_sync),
        )
        .route(
            "/api/agents/{id}/import/sync/history",
            get(api::import_sync::get_import_sync_history),
        )
        .route(
            "/api/system/log-level",
            get(api::system::get_log_level_handler).patch(api::system::patch_log_level_handler),
        )
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health_check() -> &'static str {
    "ok"
}

async fn api_health_check() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({"status": "ok"}))
}
