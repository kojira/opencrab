use std::path::{Path, PathBuf};
use std::sync::Arc;

use opencrab_server::{config, config::AppConfig, AppState};
use tracing_subscriber::EnvFilter;

/// Owned results of the pre-spawn startup phase.
pub(super) struct BootstrapContext {
    pub(super) cfg: AppConfig,
    pub(super) extgate: Arc<opencrab_extgate::ExtgateState>,
    pub(super) gate_socket: Option<std::path::PathBuf>,
    /// Present only when `[gate_admin]` is configured. Gateways provision over the ordinary gate
    /// socket (D-1098); the separate admin socket is kept only for existing deployments.
    pub(super) gate_admin: Option<GateAdmin>,
    pub(super) state: AppState,
}

pub(super) struct GateAdmin {
    pub(super) listener: tokio::net::UnixListener,
    pub(super) cleanup: opencrab_extgate::admin_socket::SocketCleanup,
    pub(super) router: axum::Router,
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

fn attachment_outbox_path(database_path: &Path) -> PathBuf {
    database_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("attachments/outbox")
}

/// Creates the private core-owned outbox for declared workspace files, removes
/// copies left by a previous process (no call survives a restart) and gives
/// extgate the outbox root plus the agent workspace template.
fn configure_attachment_outbox(
    extgate: &opencrab_extgate::ExtgateState,
    database_path: &Path,
    workspace_base: &str,
) -> anyhow::Result<PathBuf> {
    let outbox = attachment_outbox_path(database_path);
    std::fs::create_dir_all(&outbox)?;
    #[cfg(unix)]
    std::fs::set_permissions(&outbox, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let outbox = outbox.canonicalize()?;
    let removed = opencrab_extgate::outbound_files::sweep_attachment_outbox(&outbox)?;
    if removed > 0 {
        tracing::info!(removed, "startup: removed stale outbound attachment copies");
    }
    extgate.set_attachment_outbox_root(outbox.clone());
    extgate.set_workspace_base(workspace_base.to_string());
    Ok(outbox)
}

#[cfg(test)]
thread_local! {
    static INJECTED_GATE_ADMIN_STARTUP_FAULT: std::cell::RefCell<Option<&'static str>> = const { std::cell::RefCell::new(None) };
}

fn inject_gate_admin_startup_fault(stage: &'static str) -> anyhow::Result<()> {
    #[cfg(test)]
    if INJECTED_GATE_ADMIN_STARTUP_FAULT.with(|fault| *fault.borrow() == Some(stage)) {
        anyhow::bail!("injected gate-admin {stage} startup failure");
    }
    let _ = stage;
    Ok(())
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

    // DB初期化（本番はコネクションプール）
    inject_gate_admin_startup_fault("migration")?;
    let db = opencrab_db::Db::open(&cfg.database.path)?;

    // D-1098: `[gate_admin]` is optional. Without it core starts with only the gate socket, and
    // gateways provision their instances there. When it is configured (existing deployments),
    // its manifest/bootstrap and private socket are prepared before any listener starts.
    let gate_admin_configured = !cfg.gate_admin.listen_socket.is_empty()
        || !cfg.gate_admin.bootstrap_credential_file.is_empty();
    let prepared_gate_admin = if gate_admin_configured {
        Some(prepare_gate_admin(&cfg, &db)?)
    } else {
        None
    };

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
    let gate_socket = opencrab_extgate::validate_listen_socket(&cfg.gate.listen_socket)?;
    let extgate = Arc::new(opencrab_extgate::ExtgateState::new_protected(db.clone()));
    let gate_admin = match prepared_gate_admin {
        Some((listener, cleanup)) => {
            inject_gate_admin_startup_fault("router")?;
            Some(GateAdmin {
                listener,
                cleanup,
                router: opencrab_extgate::admin_router(extgate.clone()),
            })
        }
        None => None,
    };
    configure_attachment_inbox(&extgate, Path::new(&cfg.database.path))?;
    configure_attachment_outbox(
        &extgate,
        Path::new(&cfg.database.path),
        &cfg.agent.workspace_path,
    )?;

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
        workspace_base: cfg.agent.workspace_path.clone(),
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
        subtask_registries: Arc::new(opencrab_server::subtask_registries::SubtaskRegistries::new()),
        // #588 Stage 2: プロセス全体で 1 つの per-session 直列化ロック。heartbeat・scheduler・
        // gateway受信ループが同じ実体を共有し、同一セッションのターンを直列化する。
        session_locks: Arc::new(opencrab_actions::SessionLocks::new()),
        progress_debounce: Arc::new(opencrab_server::subtask_registries::ProgressDebounce::new()),
        // 中央スケジューラの起床通知（#437 / #439）。発火ターン完了と schedule CRUD
        // から鳴らして即時反映させる。
        scheduler_wake: Arc::new(tokio::sync::Notify::new()),
        // 受信箱消化ループの起床通知（#499）。webhook が新規イベントを積んだ直後に鳴らし、
        // ポーリング間隔を待たずに即消化させる（ポーリングは安全網として残す）。
        intake_wake: Arc::new(tokio::sync::Notify::new()),
        // #588 TimedFire: 時刻発火の受け口レジストリ。各ゲートウェイのループが起動時に自分の
        // 受け口を登録し、scheduler が発火時に per-agent→共有で引く（空で作り後から register）。
        timed_fire_router: Arc::new(opencrab_actions::TimedFireRouter::new()),
    };

    Ok(BootstrapContext {
        cfg,
        extgate,
        gate_socket,
        gate_admin,
        state,
    })
}

fn prepare_gate_admin(
    cfg: &AppConfig,
    db: &opencrab_db::Db,
) -> anyhow::Result<(
    tokio::net::UnixListener,
    opencrab_extgate::admin_socket::SocketCleanup,
)> {
    if cfg.gate_admin.listen_socket.is_empty()
        || cfg.gate_admin.bootstrap_credential_file.is_empty()
    {
        anyhow::bail!("[gate_admin] needs both listen_socket and bootstrap_credential_file");
    }
    if cfg.gate_admin.listen_socket == cfg.gate.listen_socket {
        anyhow::bail!("[gate_admin].listen_socket must be distinct from [gate].listen_socket");
    }
    let service_euid = unsafe { libc::geteuid() };
    inject_gate_admin_startup_fault("bootstrap")?;
    let manifest = opencrab_extgate::gate_admin_security::read_manifest(
        Path::new(&cfg.gate_admin.bootstrap_credential_file),
        service_euid,
    )?;
    {
        let mut conn = db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for gate-admin bootstrap"))?;
        opencrab_extgate::gate_admin_security::bootstrap(
            &mut conn,
            &manifest,
            opencrab_extgate::now_nanos(),
        )?;
    }
    drop(manifest);
    inject_gate_admin_startup_fault("socket")?;
    let prepared_admin = opencrab_extgate::admin_socket::prepare_admin_socket(
        Path::new(&cfg.gate_admin.listen_socket),
        service_euid,
    )?;
    Ok(prepared_admin.into_parts())
}

#[cfg(test)]
mod tests {
    #[test]
    fn gate_admin_startup_fault_injection_covers_every_pre_listener_stage() {
        use std::cell::Cell;

        let source = include_str!("bootstrap.rs");
        for failed_stage in ["migration", "bootstrap", "socket", "router"] {
            assert!(
                source.contains(&format!(
                    "inject_gate_admin_startup_fault(\"{failed_stage}\")"
                )),
                "missing pre-listener fault injection for {failed_stage}"
            );
            super::INJECTED_GATE_ADMIN_STARTUP_FAULT
                .with(|fault| *fault.borrow_mut() = Some(failed_stage));
            let runtime_accepts = Cell::new(0);
            let public_accepts = Cell::new(0);
            let startup = (|| -> anyhow::Result<()> {
                for stage in ["migration", "bootstrap", "socket", "router"] {
                    super::inject_gate_admin_startup_fault(stage)?;
                }
                runtime_accepts.set(runtime_accepts.get() + 1);
                public_accepts.set(public_accepts.get() + 1);
                Ok(())
            })();
            super::INJECTED_GATE_ADMIN_STARTUP_FAULT.with(|fault| *fault.borrow_mut() = None);
            assert!(startup.is_err(), "{failed_stage} fault must stop startup");
            assert_eq!(runtime_accepts.get(), 0);
            assert_eq!(public_accepts.get(), 0);
        }
    }

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
        let extgate = opencrab_extgate::ExtgateState::new_protected(db);

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

    #[test]
    fn attachment_outbox_is_derived_from_the_core_database_directory() {
        let database = std::path::Path::new("runtime/data/opencrab.db");
        assert_eq!(
            super::attachment_outbox_path(database),
            std::path::Path::new("runtime/data/attachments/outbox")
        );
    }

    #[test]
    fn startup_configures_a_private_outbox_and_sweeps_stale_copies() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("data/opencrab.db");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let stale_dir = temp.path().join("data/attachments/outbox/instance-1");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("stale.bin"), b"left over").unwrap();
        let db = opencrab_db::Db::open(database.to_str().unwrap()).unwrap();
        let extgate = opencrab_extgate::ExtgateState::new_protected(db);

        let outbox =
            super::configure_attachment_outbox(&extgate, &database, "ws/{agent_id}").unwrap();

        assert_eq!(extgate.attachment_outbox_root().as_deref(), Some(&*outbox));
        assert_eq!(extgate.workspace_base().as_deref(), Some("ws/{agent_id}"));
        assert!(!stale_dir.join("stale.bin").exists(), "stale copy swept");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(outbox).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
