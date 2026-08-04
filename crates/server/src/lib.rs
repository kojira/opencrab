use std::sync::{Arc, RwLock};

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

pub mod agent_heartbeat;
pub mod agent_log;
pub mod agent_management;
pub mod agent_nostr_relay;
pub mod agent_runtime_impl;
pub mod api;
pub mod caller_identity;
pub mod config;
pub mod heartbeat_instructions;
pub mod hot_reload;
pub mod llm_adapter;
pub mod llm_log_archive;
pub mod memory_declare;
pub mod memory_maintenance;
pub mod memory_organize;
pub mod nostr_runner_impl;
pub mod peer_review;
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
    /// evaluator（契約に対する独立 rubric 評価）の設定。
    /// #291 で対話ターンからの呼び出しを撤去したため現在は未参照。
    /// スリープ側へ評価を移す配線（別 issue）で使う。
    pub evaluator: config::EvaluatorConfig,
    /// スリープ時スキル棚卸し（自己 curation ループ）の設定。
    pub skill_consolidation: config::SkillConsolidationConfig,
    /// 記憶カテゴリ層（#313/#344）の sleep 中自動割当の設定。既定オフ（#345）。
    pub category_maintenance: config::CategoryMaintenanceConfig,
    /// スリープ整理ラン（#313 段階3 / #361）の設定。既定オフ（opt-in / #346）。
    pub memory_organize: config::MemoryOrganizeConfig,
    /// スリープ宣言ラン（#384 / #376 段階2）の設定。既定オフ（opt-in / #346）。
    pub memory_declare: config::MemoryDeclareConfig,
    /// ループ再起動 v1（#52）: 反復上限停止 + active タスク残存時の1回自動再実行。
    pub loop_restart_enabled: bool,
    /// エージェント単位のインデックスビルド in-flight フラグ（post-run トリガーと
    /// メンテナンスループの二重 LLM 支出防止）。
    pub index_build_inflight: memory_maintenance::IndexBuildInflight,
    pub mcp_manager: Option<SharedMcpManager>,
    /// 受信を持つ transport の per-agent ライフサイクル登録簿（#191 段階2）。
    ///
    /// **Discord / Nostr の名指しフィールドはもう無い**（PR4 で撤去）。共通操作
    /// （起動 / 停止 / 生存確認）も transport 固有の操作（ツール実行の実体・鍵の
    /// 払い出し）も、すべてここから種別名（[`opencrab_actions::gateway_kinds`]）で
    /// 引く。後者は既定 `None` の capability accessor
    /// （`gateway_actions_for` / `key_provisioning`）で、`GatewayActions` の
    /// `a2ui_surface` / `text_delivery` と同じ流儀。
    /// 未登録の種別は生存確認が **false**（共有ゲートウェイが処理を続ける側）。
    ///
    /// **内部可変**（[`opencrab_actions::AgentGatewayRegistry`] が中で `RwLock` を持つ）。
    /// マネージャの生成順は仕様であり（Discord のマネージャは共有ゲートウェイへ渡す
    /// state clone より前、Nostr はルータ構築の直前）、不変フィールドにすると
    /// 「全マネージャが state 構築前に揃っていること」を要求してその順序と衝突する。
    /// 後から登録できる形にして順序依存を構造的に消す（`voice_runtime` /
    /// `subtask_lifecycle_notifier` と同じ流儀）。
    ///
    /// **MCP は入れない。** `crates/mcp` は受信を持たず transport ではない（道具の
    /// 供給者で、注入は深さ 0 限定）。`mcp_manager` は名指しのまま残す。
    pub gateways: Arc<opencrab_actions::AgentGatewayRegistry>,
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
    /// エージェント単位ハートビート設定の境界値（#247）。
    ///
    /// エージェント自身が触るツール（`get_my_heartbeat` / `set_my_heartbeat`）が
    /// 参照する。下限は設定ファイル（`[agent] heartbeat_min_interval_secs`）由来で、
    /// 運用者が費用と負荷の許容範囲を決める。
    pub heartbeat_limits: config::HeartbeatLimits,
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
        category_maintenance: config::CategoryMaintenanceConfig::default(),
        memory_organize: config::MemoryOrganizeConfig::default(),
        memory_declare: config::MemoryDeclareConfig::default(),
        loop_restart_enabled: false,
        index_build_inflight: Arc::new(dashmap::DashMap::new()),
        mcp_manager: None,
        gateways: Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        web_gateway: Arc::new(opencrab_web_gateway::WebGateway::new()),
        subtask_registries: Arc::new(subtask_registries::SubtaskRegistries::new()),
        progress_debounce: Arc::new(subtask_registries::ProgressDebounce::new()),
        subtask_notifiers: Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: Arc::new(std::sync::Mutex::new(None)),
        default_subtask_webhook: None,
        heartbeat_limits: config::HeartbeatLimits::default(),
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
        // Nostr 受信 → Discord 転記先の per-agent 設定（issue #252 段階 B）
        .route(
            "/api/agents/{id}/nostr-relay",
            get(api::nostr_relay::get_nostr_relay_config)
                .put(api::nostr_relay::update_nostr_relay_config),
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

/// transport 登録簿（#191 段階2 PR2）が、実物のマネージャを載せて期待どおり働くこと。
///
/// 偽実装ではなく `DiscordGatewayManager` / `NostrGatewayManager` を入れる。生成は
/// ネットワークに出ない（実際の接続は `start` を呼んだときだけ）ので、
/// 「本物がトレイトオブジェクトとして成立するか」をここで押さえられる。
#[cfg(test)]
mod gateway_registry_tests {
    use super::*;
    use opencrab_actions::gateway_kinds;

    /// state を clone しても登録簿は**同じ 1 つ**を指す。
    ///
    /// これが成り立たないと「共有ゲートウェイへ渡した clone からは専用ゲートウェイが
    /// 見えない」ことになり、内部可変にして後から登録する意味が無くなる（#40 の
    /// 二重処理防止が壊れる）。
    #[test]
    fn registry_is_shared_across_state_clones() {
        let state = test_app_state();
        let clone = state.clone();
        assert!(Arc::ptr_eq(&state.gateways, &clone.gateways));

        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(nostr);
        assert_eq!(clone.gateways.kinds(), vec![gateway_kinds::NOSTR]);
    }

    /// 実物のマネージャを登録順どおりに載せられる（Discord → Nostr）。
    #[cfg(feature = "discord")]
    #[test]
    fn real_managers_register_in_startup_order() {
        let state = test_app_state();
        let discord = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(discord);
        state.gateways.register(nostr);

        assert_eq!(
            state.gateways.kinds(),
            vec![gateway_kinds::DISCORD, gateway_kinds::NOSTR],
            "登録順 = main の起動順。PR5 の走査がこの順を再現する"
        );
    }

    /// **起動処理が実際に取る形**を実物のマネージャで再現する（#191 段階2 PR5）。
    ///
    /// `main` は復元位置を 2 つ持つ（Discord = 共有ゲートウェイ起動後 / Nostr = ルータ
    /// 構築の直前）。各位置で「登録済みかつ未復元の分だけ」を走査するので、走る対象は
    /// 移設前の `manager.restore_from_db()` と 1 対 1 になる。**1 回に畳むと Discord の
    /// 復元が後ろへずれ、直後の heartbeat 用 HTTP クライアント取得が空振りする。**
    ///
    /// DB は空なので復元は 1 件も起動しない（＝実ネットワークに出ない）。ここで見たいのは
    /// 「どのマネージャが・どの位置で・何回走査に拾われるか」。
    #[cfg(feature = "discord")]
    #[tokio::test]
    async fn startup_sweep_restores_each_manager_at_its_own_point() {
        let state = test_app_state();

        // 位置 1: Discord だけが登録済み。
        let discord = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        state.gateways.register(discord);
        assert_eq!(
            state.gateways.restore_pending().await,
            vec![gateway_kinds::DISCORD]
        );

        // 位置 2: Nostr を登録してから走査。Discord は**もう拾わない**。
        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(nostr);
        assert_eq!(
            state.gateways.restore_pending().await,
            vec![gateway_kinds::NOSTR],
            "2 度目の走査が Discord を巻き込むと、稼働中の接続を張り直してしまう"
        );

        // 復元は起動時 1 回だけ（周期的な自己修復は持たない）。
        assert!(state.gateways.restore_pending().await.is_empty());
    }

    /// **Discord を落とした構成**では位置 1 の走査ごと消え、残る 1 回が Nostr を復元する。
    #[cfg(not(feature = "discord"))]
    #[tokio::test]
    async fn startup_sweep_restores_nostr_without_discord() {
        let state = test_app_state();
        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(nostr);

        assert_eq!(
            state.gateways.restore_pending().await,
            vec![gateway_kinds::NOSTR]
        );
        assert!(state.gateways.restore_pending().await.is_empty());
    }

    /// 生存確認は「稼働していない / 未登録」のどちらでも false に倒れる。
    ///
    /// これはルーティング判定（専用ゲートウェイに任せるか、共有側が続けるか）なので、
    /// 未登録で true に倒すと二重処理、panic させると停止する。
    #[test]
    fn is_running_falls_back_to_false() {
        let state = test_app_state();
        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(nostr);

        assert!(
            !state.gateways.is_running(gateway_kinds::NOSTR, "crab"),
            "起動していないエージェントは false"
        );
        assert!(
            !state.gateways.is_running(gateway_kinds::DISCORD, "crab"),
            "未登録の種別も false（共有側が処理を続ける）"
        );
        assert!(
            !state.gateways.is_running("mcp", "crab"),
            "MCP は登録簿に入れない（受信を持たない）"
        );
    }

    /// トレイト経由で起動を呼べる。設定行が無ければ `Err`（panic しない）。
    #[tokio::test]
    async fn start_through_trait_errors_without_db_config() {
        let state = test_app_state();
        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(nostr);

        let gw = state.gateways.get(gateway_kinds::NOSTR).unwrap();
        assert!(
            gw.start("no-such-agent").await.is_err(),
            "設定を DB から読む契約なので、行が無ければ Err"
        );
        // 停止と全停止は稼働ゼロでも安全に呼べる。
        gw.stop("no-such-agent").await;
        gw.shutdown_all().await;
    }

    /// Discord も同じ契約で扱える（`start` は DB から読み、行が無ければ `Err`）。
    ///
    /// ここまで来れば実ネットワークには出ない（接続は設定行があるときだけ）。
    #[cfg(feature = "discord")]
    #[tokio::test]
    async fn discord_start_through_trait_errors_without_db_config() {
        let state = test_app_state();
        let discord = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        state.gateways.register(discord);

        let gw = state.gateways.get(gateway_kinds::DISCORD).unwrap();
        let err = gw.start("no-such-agent").await.unwrap_err().to_string();
        assert!(
            err.contains("no-such-agent"),
            "どのエージェントか分かること: {err}"
        );
        gw.stop("no-such-agent").await;
        gw.shutdown_all().await;
    }

    // ------------------------------------------------------------------
    // 起動条件のガード（#191 段階2 PR3）
    //
    // 呼び出しを登録簿経由に差し替えるにあたり、REST ハンドラが**呼び出しの手前**で
    // 行っていた起動条件の判定を、各実装の `start` の中へ持ち上げた。ガードが効いて
    // いないと「無効にしたはずの設定 / 空白だけの資格情報でも起動する」穴が開くので、
    // ここで固定する。
    //
    // どのテストも**実ネットワークに出ない**。ガードは接続を試みる手前で弾くため、
    // 弾けていなければ実際の接続失敗（別種のエラー）になり、`is_start_declined` の
    // assert が落ちる。
    // ------------------------------------------------------------------

    #[cfg(feature = "discord")]
    fn put_discord_config(state: &AppState, agent_id: &str, token: &str, enabled: bool) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent_discord_config(
            &conn,
            &opencrab_db::queries::AgentDiscordConfigRow {
                agent_id: agent_id.to_string(),
                bot_token: token.to_string(),
                owner_discord_id: "111111111111111111".to_string(),
                enabled,
            },
        )
        .unwrap();
    }

    /// **無効化された設定では起動しない。**
    ///
    /// 移設前は `PATCH /api/agents/{id}/discord` が
    /// `opencrab_discord::gateway_will_start(enabled, token)` を呼び出しの手前で見ていた。
    /// 同じ述語・同じ引数（同一の DB 行の `enabled` と `bot_token`）を `start` の中へ
    /// 移しただけなので、判定は移設前と 1 対 1 で一致する。
    #[cfg(feature = "discord")]
    #[tokio::test]
    async fn discord_start_declines_when_config_is_disabled() {
        let state = test_app_state();
        let discord = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        state.gateways.register(discord);
        // トークンはあるが enabled=0（「停止したはず」の設定）。
        put_discord_config(&state, "crab", "bot-token-looks-real", false);

        let gw = state.gateways.get(gateway_kinds::DISCORD).unwrap();
        let err = gw.start("crab").await.unwrap_err();
        assert!(
            opencrab_actions::is_start_declined(&err),
            "無効な設定で起動を試みている（ガードが落ちている）: {err}"
        );
        assert!(
            !state.gateways.is_running(gateway_kinds::DISCORD, "crab"),
            "弾かれたのにゲートウェイが登録されている"
        );
    }

    /// **空白だけのトークンでは起動しない**（`gateway_will_start` は trim して判定する）。
    #[cfg(feature = "discord")]
    #[tokio::test]
    async fn discord_start_declines_on_blank_token() {
        let state = test_app_state();
        let discord = Arc::new(opencrab_discord::DiscordGatewayManager::new(state.clone()));
        state.gateways.register(discord);
        put_discord_config(&state, "crab", " \t\n", true);

        let gw = state.gateways.get(gateway_kinds::DISCORD).unwrap();
        let err = gw.start("crab").await.unwrap_err();
        assert!(
            opencrab_actions::is_start_declined(&err),
            "空白だけのトークンで起動を試みている: {err}"
        );
        assert!(!state.gateways.is_running(gateway_kinds::DISCORD, "crab"));
    }

    /// **鍵が未設定の Nostr 設定では起動しない。**
    ///
    /// 移設前は `PUT /api/agents/{id}/nostr` が「鍵が無ければ 400」を呼び出しの手前で
    /// 返していた（`POST /nostr/start` にはその判定が無く、素通りしていた）。判定を
    /// `start_agent_gateway` の単一チョークポイントへ置き直したので、どの呼び出し口
    /// からでも同じように弾かれる。
    #[tokio::test]
    async fn nostr_start_declines_without_secret_key() {
        let state = test_app_state();
        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(nostr);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::upsert_agent_nostr_config(
                &conn,
                &opencrab_db::queries::AgentNostrConfigRow {
                    agent_id: "agent-191-pr3".to_string(),
                    secret_key: "  ".to_string(),
                    relays_json: "[]".to_string(),
                    filter_json: r#"{"authors":["npub1abc"],"keywords":[],"kinds":[1]}"#
                        .to_string(),
                    enabled: true,
                },
            )
            .unwrap();
        }

        let gw = state.gateways.get(gateway_kinds::NOSTR).unwrap();
        let err = gw.start("agent-191-pr3").await.unwrap_err();
        assert!(
            opencrab_actions::is_start_declined(&err),
            "鍵が無いのに起動を試みている: {err}"
        );
        assert!(!state
            .gateways
            .is_running(gateway_kinds::NOSTR, "agent-191-pr3"));
    }

    /// **Nostr の `start` は DB の `enabled` を見ない。**
    ///
    /// ハンドラ側の方針が「起動が成功してから `enabled=true`」なので、`PUT /nostr` は
    /// **わざと `enabled=false` の行を書いてから** `start` を呼ぶ。ここに Discord と同じ
    /// 有効フラグのガードを足すと、その正しい経路が毎回自分のガードに弾かれて Nostr が
    /// 二度と起動しなくなる（無効化ではなく**機能停止**）。
    ///
    /// `enabled=false` の行で `start` を呼び、返ってくるのが**秘密鍵の拒否**であることを
    /// 見る。有効フラグのガードが先に弾いていればこの文言にはならないので、「`enabled=false`
    /// を素通りして資格情報の検査まで到達している」ことが分かる。
    ///
    /// [#271/#278] 以前はここで「フィルタが無制限（author も keyword も無い）」の拒否文言を
    /// 見ていた。新 nostaro では `watch` が mention-only 既定で自分宛だけを購読するため
    /// **空フィルタは洪水ではなく最も狭い購読**で、そのガード自体が無くなった。テストの意図
    /// （`enabled` を見ずに検査へ到達する）はそのままに、到達を確かめる対象を今も残っている
    /// 資格情報ガードへ移した。鍵の拒否は設定ファイルを書き出す**手前**なので、実プロセスも
    /// ファイルシステムも触らないという性質も変わらない。
    #[tokio::test]
    async fn nostr_start_does_not_look_at_the_enabled_flag() {
        let state = test_app_state();
        let nostr = Arc::new(opencrab_nostr::NostrGatewayManager::new(state.clone()));
        state.gateways.register(nostr);
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::upsert_agent_nostr_config(
                &conn,
                &opencrab_db::queries::AgentNostrConfigRow {
                    agent_id: "agent-191-pr3".to_string(),
                    // 空白だけの nsec = 資格情報ガードに弾かれる（起動は試みられる）。
                    secret_key: "  ".to_string(),
                    relays_json: "[]".to_string(),
                    filter_json: r#"{"authors":[],"keywords":[],"kinds":[1]}"#.to_string(),
                    // `PUT /nostr` が start を呼ぶ瞬間の状態そのもの。
                    enabled: false,
                },
            )
            .unwrap();
        }

        let gw = state.gateways.get(gateway_kinds::NOSTR).unwrap();
        let err = gw.start("agent-191-pr3").await.unwrap_err();
        assert!(
            err.to_string().contains("秘密鍵"),
            "enabled=false の行が資格情報の検査より手前で弾かれている\
             （enabled を見るガードを足すと PUT /nostr が通らなくなる）: {err}"
        );
        assert!(
            !state
                .gateways
                .is_running(gateway_kinds::NOSTR, "agent-191-pr3"),
            "弾かれたのに稼働してはいけない"
        );
    }
}
