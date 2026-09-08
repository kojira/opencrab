use anyhow::Context as _;
use opencrab_server::discord_supervisor::{supervise, GatewayChildSpawner, SupervisorConfig};

pub(super) fn ignite(
    start_nostr: bool,
    db: &opencrab_db::Db,
    database_path: &str,
    core_socket: Option<&str>,
    secret_provider: Option<&opencrab_nostr::MainKeyProvider>,
    gateway_bin: &std::path::Path,
    nostaro_bin: &std::path::Path,
) -> anyhow::Result<Option<tokio::sync::watch::Sender<bool>>> {
    if !start_nostr {
        return Ok(None);
    }
    let core_socket = core_socket.context("Nostr V3 requires gate.listen_socket")?;
    let secret_provider = secret_provider.context("Nostr V3 secret provider is unavailable")?;
    let placement_dir = std::path::Path::new(database_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("gate")
        .join("nostr");
    std::fs::create_dir_all(&placement_dir)
        .with_context(|| format!("placement dir 作成失敗: {}", placement_dir.display()))?;
    let plans = {
        let conn = db
            .lock()
            .map_err(|_| anyhow::anyhow!("db lock for Nostr V3 ignition"))?;
        opencrab_server::nostr_provision::load_nostr_placement_plans(&conn)?
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let supervisor_cfg = SupervisorConfig::default();
    for plan in plans {
        let secret = secret_provider(&plan.agent_id)?;
        let placement = serde_json::json!({
            "core_socket": core_socket,
            "nostaro_bin": nostaro_bin,
            "instances": [{
                "instance_id": plan.instance_id,
                "revision": plan.revision,
                "address": plan.address,
                "config_b64": plan.config_b64,
            }],
        });
        let path = placement_dir.join(format!("{}.json", plan.agent_id));
        std::fs::write(&path, serde_json::to_vec_pretty(&placement)?)
            .with_context(|| format!("placement 書き出し失敗: {}", path.display()))?;
        let spawner = std::sync::Arc::new(GatewayChildSpawner::new_nostr(
            gateway_bin.to_path_buf(),
            path,
            secret.to_string(),
            plan.agent_id.clone(),
        ));
        tokio::spawn(supervise(
            spawner,
            supervisor_cfg.clone(),
            shutdown_rx.clone(),
        ));
        tracing::info!(
            agent_id = %plan.agent_id,
            bin = %gateway_bin.display(),
            "nostr-gateway supervisor started; secret injected only in child env"
        );
    }
    Ok(Some(shutdown_tx))
}
