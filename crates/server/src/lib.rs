use std::sync::{Arc, RwLock};

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub mod agent_log;
pub mod agent_management;
pub mod api;
pub mod config;
pub mod heartbeat_instructions;
pub mod hot_reload;
pub mod llm_adapter;
pub mod memory_maintenance;
pub mod nostr_runner_impl;
pub mod process;
pub mod skill_consolidation;
pub mod subtask_registries;
pub mod subtask_spawn;
pub mod system_actions;
pub mod web_runner_impl;
pub mod webhook_targets;

#[cfg(feature = "discord")]
mod agent_runner_impl;
pub mod transcript;

/// per-agent Nostr sub-gateway マネージャの共有ハンドル。
pub type SharedNostrManager = Arc<opencrab_nostr::NostrGatewayManager<AppState>>;

/// per-agent MCP 接続マネージャの共有ハンドル。
pub type SharedMcpManager = Arc<opencrab_mcp::McpClientManager>;

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
    /// per-agent Nostr sub-gateway マネージャ（main で構築してセットされる）。
    pub nostr_manager: Option<SharedNostrManager>,
    pub mcp_manager: Option<SharedMcpManager>,
    /// web gateway ランタイム（#154）: SSE 配送 / per-session 直列化 / dispatch registry。
    /// 実体は独立クレート `opencrab-web-gateway`（#190）。
    pub web_gateway: Arc<opencrab_web_gateway::WebGateway>,
    /// 非ブロック dispatch（#152 S3a）の subtask registry 置き場（#169）。
    /// REST は session_id キー、heartbeat は agent_id キーで貸し借りし、
    /// dispatcher と `cancel_subtask`（#161）が同一 registry を見るようにする。
    pub subtask_registries: Arc<subtask_registries::SubtaskRegistries>,
    /// `report_progress`（#175 S1）のデバウンス世代カウンタ。
    ///
    /// `SystemGatewayActions` は run ごとに作り直されるため、そのフィールドに置くと
    /// デバウンスが毎回リセットされ全ての進捗報告が発火する。プロセス寿命の共有状態
    /// である `AppState` に置いて、gateway の生成を跨いで間引きが効くようにする。
    pub progress_debounce: Arc<subtask_registries::ProgressDebounce>,
    /// 走行中サブタスクの lifecycle 通知口（subtask_id → 通知口 / #175 S3・S4）。
    ///
    /// `spawn_subtask`（server 側）が insert し、決着・停止で remove する。registry と
    /// 対で共有し、Discord の `cancel_subtask` もここから引いて中断を通知する。
    pub subtask_notifiers: opencrab_actions::subtask_notify::SubtaskNotifiers,
    /// サブタスク lifecycle 通知の実装（未設定なら通知しない / #175 S4）。
    ///
    /// Discord webhook 実装は起動時にここへ差し込む。`AppState` は clone されて
    /// 各所へ配られるため、後から差し替えられるよう内部可変にしている
    /// （`voice_runtime` と同じ流儀）。
    pub subtask_lifecycle_notifier: Arc<
        std::sync::Mutex<
            Option<Arc<dyn opencrab_actions::subtask_notify::SubtaskLifecycleNotifier>>,
        >,
    >,
    /// 非ブロック dispatch の kill switch（`[subtask] auto_dispatch` / 既定 true）。
    /// `false` にすると全ツールが inline 実行に戻る（#152 導入前の挙動）。
    pub subtask_auto_dispatch: bool,
    /// 設定ファイル由来の**通知先フォールバック**（#157 S5）。
    ///
    /// 通知先の解決は「明示指定 → DB の scope 別既定（tool>agent>global）→ ここ」の順で、
    /// DB 行が皆無のときだけ効く（`WebhookSource::EnvConfig`）。以前この値は Discord
    /// 起動ブロックのローカル変数にしか無く、gateway 非依存層からは到達できなかった。
    /// 管理ツール（`get_default_subtask_webhook` 等）を合成層へ移すにあたり、
    /// **transport の機能フラグに依存しない形**でここへ持ち上げた
    /// （`config::AppConfig::default_subtask_webhook`）。
    pub default_subtask_webhook: Option<opencrab_actions::webhook_target::WebhookConfig>,
}

impl AppState {
    /// サブタスク lifecycle 通知の実装を返す（未設定なら何もしない Noop）。
    pub fn subtask_lifecycle_notifier(
        &self,
    ) -> Arc<dyn opencrab_actions::subtask_notify::SubtaskLifecycleNotifier> {
        self.subtask_lifecycle_notifier
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| Arc::new(opencrab_actions::NoopLifecycleNotifier))
    }
}

/// 最小構成の `AppState`（in-memory DB、LLM プロバイダ 0 件、gateway マネージャ無し）。
///
/// crate 内のユニットテスト共用。`AppState` にフィールドが増えたときの追随箇所を
/// 1 つに保つ（テストごとの構造体リテラル複製を避ける）。
#[cfg(test)]
pub(crate) fn test_app_state() -> AppState {
    let conn = opencrab_db::init_memory().unwrap();
    AppState {
        db: opencrab_db::Db::from_connection(conn),
        llm_router: SharedLlmRouter::new(LlmRouter::new()),
        llm_config: Arc::new(toml::from_str("").unwrap()),
        subtask_auto_dispatch: true,
        voice_config: Arc::new(Default::default()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: std::env::temp_dir().to_string_lossy().to_string(),
        default_model: "mock:test".to_string(),
        tools_config: Arc::new(RwLock::new(opencrab_actions::tools::ToolsConfig::default())),
        compaction_ratio: 0.5,
        evaluator: config::EvaluatorConfig::default(),
        skill_consolidation: config::SkillConsolidationConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: Arc::new(dashmap::DashMap::new()),
        #[cfg(feature = "discord")]
        discord_manager: None,
        nostr_manager: None,
        mcp_manager: None,
        web_gateway: Arc::new(opencrab_web_gateway::WebGateway::new()),
        subtask_registries: Arc::new(subtask_registries::SubtaskRegistries::new()),
        progress_debounce: Arc::new(subtask_registries::ProgressDebounce::new()),
        subtask_notifiers: Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
    }
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
        .route(
            "/api/agents/{id}/sleep-logs",
            get(api::sleep::get_sleep_logs),
        )
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
        // cursor 診断（サーバーが使う cursor CLI のパス/バージョン）
        .route(
            "/api/llm/cursor/diagnostics",
            get(api::providers::cursor_diagnostics),
        )
        // acp 診断（起動バイナリ/引数/解決パス）
        .route(
            "/api/llm/acp/diagnostics",
            get(api::providers::acp_diagnostics),
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
            "/api/llm/providers/{name}/test",
            post(api::providers::test_provider_endpoint),
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
        // Nostr sub-gateway per-agent config
        .route(
            "/api/agents/{id}/nostr",
            get(api::nostr::get_nostr_config)
                .put(api::nostr::update_nostr_config)
                .delete(api::nostr::delete_nostr_config),
        )
        .route(
            "/api/agents/{id}/nostr/generate",
            post(api::nostr::generate_nostr_key),
        )
        .route(
            "/api/agents/{id}/nostr/start",
            post(api::nostr::start_nostr_gateway),
        )
        .route(
            "/api/agents/{id}/nostr/stop",
            post(api::nostr::stop_nostr_gateway),
        )
        // MCP サーバ per-agent 設定
        .route(
            "/api/agents/{id}/mcp",
            get(api::mcp::list_mcp_servers).put(api::mcp::put_mcp_server),
        )
        .route(
            "/api/agents/{id}/mcp/{name}",
            axum::routing::delete(api::mcp::delete_mcp_server),
        )
        .route(
            "/api/agents/{id}/mcp/{name}/enabled",
            post(api::mcp::set_mcp_enabled),
        )
        .route(
            "/api/agents/{id}/mcp/{name}/test",
            post(api::mcp::test_mcp_server),
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
        // web gateway（#154）: ダッシュボードからの会話 + SSE 配送。
        // ルート定義とハンドラは独立クレート側にあり、ここは取り付けるだけ（#190 S4）。
        .merge(opencrab_web_gateway::routes::<AppState>())
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
