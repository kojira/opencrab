use std::path::{Path, PathBuf};
use std::sync::Arc;

use opencrab_server::{config, config::AppConfig, AppState};
use tracing_subscriber::EnvFilter;

/// Owned results of the pre-spawn startup phase.
pub(super) struct BootstrapContext {
    pub(super) cfg: AppConfig,
    pub(super) extgate: Arc<opencrab_extgate::ExtgateState>,
    pub(super) gate_socket: Option<std::path::PathBuf>,
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
    let gate_socket = opencrab_extgate::validate_listen_socket(&cfg.gate.listen_socket)?;
    let extgate = Arc::new(opencrab_extgate::ExtgateState::new(db.clone()));
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
    let llm_router = config::build_effective_llm_router(&db, &cfg.llm)?;

    // NOTE: エージェント個別の許可コマンド（DB管理）はグローバル tools 設定に
    // マージしない。全エージェントの許可が混ざり、あるエージェントの許可が他へ漏れるため。
    // 個別コマンドは実行時に run_agent_response 内でそのエージェント分だけ適用する。
    let state = AppState::new(db, llm_router, cfg.clone());

    Ok(BootstrapContext {
        cfg,
        extgate,
        gate_socket,
        state,
    })
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
        let extgate = opencrab_extgate::ExtgateState::new(db);

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
        let extgate = opencrab_extgate::ExtgateState::new(db);

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
