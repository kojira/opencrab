use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opencrab_core::heartbeat::HeartbeatConfig;
use opencrab_server::{config, config::AppConfig, AppState};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// Owned results of the pre-spawn startup phase.
pub(super) struct BootstrapContext {
    pub(super) cfg: AppConfig,
    pub(super) extgate: Arc<opencrab_extgate::ExtgateState>,
    pub(super) gate_socket: Option<std::path::PathBuf>,
    #[cfg(feature = "nostr")]
    pub(super) gate_socket_for_nostr: Option<String>,
    #[cfg(feature = "nostr")]
    pub(super) nostr_master_key: Option<opencrab_nostr::MasterKey>,
    #[cfg(feature = "nostr")]
    pub(super) start_nostr: bool,
    pub(super) heartbeat_config_tx: watch::Sender<HeartbeatConfig>,
    pub(super) heartbeat_config_rx: watch::Receiver<HeartbeatConfig>,
    pub(super) state: AppState,
}

fn attachment_inbox_path(database_path: &Path) -> PathBuf {
    database_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("attachments/inbox")
}

fn configure_attachment_inbox(
    extgate: &opencrab_extgate::ExtgateState,
    database_path: &Path,
) -> anyhow::Result<PathBuf> {
    let inbox = attachment_inbox_path(database_path);
    std::fs::create_dir_all(&inbox)?;
    #[cfg(unix)]
    std::fs::set_permissions(&inbox, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let inbox = inbox.canonicalize()?;
    extgate.set_attachment_inbox_root(inbox.clone());
    Ok(inbox)
}

/// Loads and validates startup configuration, scrubs secrets, recovers the DB,
/// recovers the database and constructs the initial application state.
/// No task is spawned before this function returns.
pub(super) fn initialize() -> anyhow::Result<BootstrapContext> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("opencrab=info".parse()?))
        .init();

    tracing::info!("Starting OpenCrab server...");

    // Load config from TOML (with env var expansion)
    let cfg = config::load_config("config/default.toml")?;

    // Nostr秘密はserver-owned lifecycleへ渡す前に環境から取り出し、親環境から消す。
    #[cfg(feature = "nostr")]
    let master_key_env = std::env::var("OPENCRAB_SECRET_MASTER_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    #[cfg(feature = "nostr")]
    std::env::remove_var("OPENCRAB_SECRET_MASTER_KEY");
    #[cfg(feature = "nostr")]
    let master_key_parsed: Option<anyhow::Result<opencrab_nostr::MasterKey>> = master_key_env
        .as_deref()
        .map(|encoded| opencrab_core::secret_box::parse_master_key(encoded).map(Arc::new));

    // DB初期化（本番はコネクションプール）
    let db = opencrab_db::Db::open(&cfg.database.path)?;
    {
        let mut conn = db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for extgate recover"))?;
        let now = opencrab_extgate::now_nanos();
        opencrab_extgate::recover_stale_deliveries(&mut conn, now)
            .map_err(|e| anyhow::anyhow!("extgate recover failed: {}", e.code.as_str()))?;
        // DI 拡張 §7.5: 残 sending の operation call も stale indeterminate にする（listener 前）。
        opencrab_extgate::recover_stale_calls(&mut conn, now).map_err(|e| {
            anyhow::anyhow!("extgate operation-call recover failed: {}", e.code.as_str())
        })?;
    }
    let gate_token = opencrab_extgate::OperatorToken::take_from_env();
    let gate_socket = opencrab_extgate::validate_listen_socket(&cfg.gate.listen_socket)?;
    #[cfg(feature = "nostr")]
    let gate_socket_for_nostr = gate_socket
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let extgate = Arc::new(opencrab_extgate::ExtgateState::new(db.clone(), gate_token));
    configure_attachment_inbox(&extgate, Path::new(&cfg.database.path))?;

    #[cfg(feature = "nostr")]
    let nostr_configured = {
        let conn = db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for Nostr configuration detection"))?;
        opencrab_db::queries::has_any_agent_nostr_config(&conn)?
    };
    #[cfg(feature = "nostr")]
    let nostr_enabled = db
        .lock()
        .map_err(|_| anyhow::anyhow!("db lock for enabled Nostr detection"))?
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_nostr_config WHERE enabled = 1)",
            [],
            |row| row.get::<_, bool>(0),
        )?;
    #[cfg(feature = "nostr")]
    if nostr_configured {
        if !matches!(
            opencrab_nostr::NostrIngress::parse(&cfg.gate.nostr_ingress),
            Some(opencrab_nostr::NostrIngress::V3)
        ) {
            anyhow::bail!("configured Nostr requires gate.nostr_ingress = v3");
        }
        if gate_socket.is_none() {
            anyhow::bail!("configured Nostr requires an absolute gate.listen_socket");
        }
    }
    #[cfg(feature = "nostr")]
    let mut nostr_master_key = match master_key_parsed {
        Some(Ok(key)) => Some(key),
        Some(Err(error)) => {
            if nostr_configured {
                emit_master_key_banner(&format!("OPENCRAB_SECRET_MASTER_KEY is invalid: {error}"));
            }
            None
        }
        None => {
            if nostr_configured {
                emit_master_key_banner("OPENCRAB_SECRET_MASTER_KEY is not set");
            }
            None
        }
    };
    #[cfg(feature = "nostr")]
    if let Some(key) = nostr_master_key.clone() {
        if let Some(reason) =
            opencrab_nostr::secret_migration::master_key_mismatch_reason(&db, &key)
        {
            emit_master_key_banner(&reason);
            nostr_master_key = None;
        }
    }
    #[cfg(feature = "nostr")]
    if nostr_enabled && nostr_master_key.is_none() {
        anyhow::bail!("enabled Nostr agent requires a valid OPENCRAB_SECRET_MASTER_KEY");
    }
    #[cfg(feature = "nostr")]
    let start_nostr = nostr_enabled;
    #[cfg(feature = "nostr")]
    if let Some(key) = &nostr_master_key {
        let report = opencrab_nostr::secret_migration::migrate_nostr_secrets_at_rest(
            &db,
            key,
            Path::new("data/agents"),
        );
        if report.changed_anything() {
            tracing::info!(?report, "Nostr secrets migrated at rest");
        }
    }

    // #553: 起動時リコンサイル。新プロセスの subtask registry（in-memory）は必ず空なので、
    // この時点で status='active' の subtask セッションは定義上すべて孤児（前プロセスと共に
    // 実行タスクが消滅済み）。「何分止まったら死」の判定を要せず 'interrupted' へ終端化する。
    // 述語 mode='subtask' は他モードに触れない（reconcile_orphaned_subtasks を参照）。
    match db.lock() {
        Ok(conn) => match opencrab_db::queries::reconcile_orphaned_subtasks(&conn) {
            Ok(n) if n > 0 => {
                tracing::info!(
                    reconciled = n,
                    "startup: 孤児化した active subtask を interrupted へ終端化した（#553）"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("startup subtask reconcile に失敗: {e}"),
        },
        Err(e) => tracing::warn!("startup subtask reconcile: db lock 取得に失敗: {e}"),
    }

    // Build LLM router from config + DB のダッシュボード設定オーバーライド
    let llm_overrides = {
        let conn = db.lock().unwrap();
        opencrab_db::queries::list_llm_provider_overrides(&conn).unwrap_or_default()
    };
    let effective_llm = config::apply_llm_overrides(&cfg.llm, &llm_overrides);
    let llm_router = config::build_llm_router(&effective_llm)?;

    let default_model = format!("{}:{}", cfg.llm.default_provider, cfg.llm.default_model);

    // NOTE: エージェント個別の許可コマンド（DB管理）はグローバル tools_config に
    // マージしない。全エージェントの許可が混ざり、あるエージェントの許可が他へ漏れるため。
    // 個別コマンドは実行時に run_agent_response 内でそのエージェント分だけ適用する。
    let tools_cfg = cfg.tools.clone();

    // ハートビートの初期設定と live G の watch チャネル。
    //
    // **AppState 構築より前に作る**のは、`get_my_heartbeat`（PR3）が 会話セッションの
    // ゲート理由（G=false）を本人へ見せるために live G を `AppState::heartbeat_config_rx` から
    // 読むため（scheduler が発火時に読むのと同一源・hot-reload 追従）。tx は config watcher へ、
    // rx は AppState と scheduler へ配る（受信端は clone 可能）。
    let (heartbeat_config_tx, heartbeat_config_rx) = watch::channel(HeartbeatConfig {
        interval_secs: cfg.agent.heartbeat_interval_secs,
        enabled: cfg.agent.heartbeat_enabled,
    });

    #[allow(unused_mut)]
    let mut state = AppState {
        db,
        llm_router: opencrab_server::SharedLlmRouter::new(llm_router),
        llm_config: Arc::new(cfg.llm.clone()),
        // 非ブロック dispatch の kill switch（`[subtask] auto_dispatch` / env 上書き）。
        subtask_auto_dispatch: cfg.subtask.auto_dispatch,
        // 純 TOML を保持する（DB オーバーライド適用前の土台）。API の GET は
        // DB 行が無いときこれを "toml" として返すため、リセット後に古い実効値を
        // TOML と誤表示しないよう effective ではなく cfg.voice を入れる（レビュー指摘）。
        voice_config: Arc::new(cfg.voice.clone()),
        voice_runtime: Arc::new(std::sync::Mutex::new(None)),
        workspace_base: cfg.agent.workspace_path.clone(),
        #[cfg(feature = "nostr")]
        nostr_master_key: nostr_master_key.clone(),
        tools_config: Arc::new(std::sync::RwLock::new(tools_cfg)),
        default_model,
        compaction_ratio: cfg.llm.compaction_ratio,
        evaluator: cfg.evaluator.clone(),
        skill_consolidation: cfg.skill_consolidation.clone(),
        category_maintenance: cfg.category_maintenance.clone(),
        memory_organize: cfg.memory_organize.clone(),
        memory_declare: cfg.memory_declare.clone(),
        memory_condense: cfg.memory_condense.clone(),
        loop_restart_enabled: cfg.agent.loop_restart_enabled,
        index_build_inflight: Arc::new(dashmap::DashMap::new()),
        intake: Arc::new(cfg.intake.clone()),
        mcp_manager: None,
        // 受信を持つ transport の登録簿（#191 段階2 PR2）。空で作り、各マネージャの
        // 生成箇所から後で `register` する（内部可変なので生成順を変えずに済む）。
        gateways: Arc::new(opencrab_actions::AgentGatewayRegistry::new()),
        subtask_registries: Arc::new(opencrab_server::subtask_registries::SubtaskRegistries::new()),
        // #588 Stage 2: プロセス全体で 1 つの per-session 直列化ロック。heartbeat・scheduler・
        // gateway受信ループが同じ実体を共有し、同一セッションのターンを直列化する。
        session_locks: Arc::new(opencrab_actions::SessionLocks::new()),
        progress_debounce: Arc::new(opencrab_server::subtask_registries::ProgressDebounce::new()),
        subtask_notifiers: Arc::new(dashmap::DashMap::new()),
        subtask_lifecycle_notifier: Arc::new(Mutex::new(None)),
        // 設定ファイル由来の通知先フォールバック（#157 S5）。**Discord 機能フラグの
        // 外**で 1 度だけ解決し、以降の利用者（gateway 非依存の管理ツール / lifecycle
        // 通知 / Discord gateway_actions）は全てこの 1 つの値を参照する。
        default_subtask_webhook: cfg.default_subtask_webhook(),
        // エージェントが自分で触るハートビート設定の境界（#247）。下限は運用者が
        // `[agent] heartbeat_min_interval_secs` で決める。
        heartbeat_limits: cfg.agent.heartbeat_limits(),
        // 中央スケジューラの起床通知（#437 / #439）。発火ターン完了・global config 変更に
        // 加え、set_my_heartbeat（PR3）からも鳴らして即時反映させる。
        scheduler_wake: Arc::new(tokio::sync::Notify::new()),
        // 受信箱消化ループの起床通知（#499）。webhook が新規イベントを積んだ直後に鳴らし、
        // ポーリング間隔を待たずに即消化させる（ポーリングは安全網として残す）。
        intake_wake: Arc::new(tokio::sync::Notify::new()),
        // live G を読む口（#394 / 設計 §13.1）。scheduler と同一の watch 源。
        heartbeat_config_rx: heartbeat_config_rx.clone(),
        // #588 TimedFire: 時刻発火の受け口レジストリ。各ゲートウェイのループが起動時に自分の
        // 受け口を登録し、scheduler が発火時に per-agent→共有で引く（空で作り後から register）。
        timed_fire_router: Arc::new(opencrab_actions::TimedFireRouter::new()),
    };

    Ok(BootstrapContext {
        cfg,
        extgate,
        gate_socket,
        #[cfg(feature = "nostr")]
        gate_socket_for_nostr,
        #[cfg(feature = "nostr")]
        nostr_master_key,
        #[cfg(feature = "nostr")]
        start_nostr,
        heartbeat_config_tx,
        heartbeat_config_rx,
        state,
    })
}

#[cfg(feature = "nostr")]
fn emit_master_key_banner(reason: &str) {
    tracing::error!(
        reason,
        "Nostr is disabled because its master key is unavailable or invalid"
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn attachment_inbox_is_derived_from_the_core_database_directory() {
        let database = std::path::Path::new("runtime/data/opencrab.db");
        assert_eq!(
            super::attachment_inbox_path(database),
            std::path::Path::new("runtime/data/attachments/inbox")
        );
    }

    #[test]
    fn startup_configures_a_private_core_owned_attachment_inbox() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("data/opencrab.db");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let db = opencrab_db::Db::open(database.to_str().unwrap()).unwrap();
        let extgate = opencrab_extgate::ExtgateState::new(
            db,
            opencrab_extgate::OperatorToken::from_bytes(""),
        );

        let inbox = super::configure_attachment_inbox(&extgate, &database).unwrap();

        assert_eq!(extgate.attachment_inbox_root().as_deref(), Some(&*inbox));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(inbox).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
