//! discord-gateway 独立 binary。serenity Gateway ⇄ V3 UDS。HTTP listen しない。
//! 1 process = exact 1 agent（設計 §0）。bot token は env のみ、起動直後に process env から消す。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use opencrab_discord_gateway::config::{decode_config_b64, Placement};
use opencrab_discord_gateway::harness::HarnessOverrides;
use opencrab_discord_gateway::run::spawn_instance;
use opencrab_discord_gateway::secret::take_bot_token;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("runtime")?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("opencrab_discord_gateway=info".parse()?)
                .add_directive("opencrab_gate_client=info".parse()?),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .context("usage: discord-gateway daemon <config.json> | instance <placement.json>")?;
    if first == "daemon" {
        let path = args
            .next()
            .map(PathBuf::from)
            .context("usage: discord-gateway daemon <config.json>")?;
        return opencrab_discord_gateway::daemon::run(
            opencrab_discord_gateway::daemon::DaemonConfig::load(&path)?,
        )
        .await;
    }
    let path = if first == "instance" {
        args.next()
            .map(PathBuf::from)
            .context("usage: discord-gateway instance <placement.json>")?
    } else {
        PathBuf::from(first)
    };
    // bot token を env から 1 回だけ受け、直後に process env から消す（設計 §1.3・§5）。
    let token = take_bot_token().map(Arc::new);
    let raw_placement: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let control = raw_placement
        .get("control_socket")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let start_nonce = raw_placement
        .get("start_nonce")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let place = Placement::load(&path)?;
    let socket = PathBuf::from(&place.core_socket);
    let attachment_spool_root = place.attachment_spool_root.as_deref().map(PathBuf::from);
    // QC ハーネス差し替えは env からのみ（既定 OFF＝production 挙動）。
    let overrides = HarnessOverrides::from_env();

    if token.is_none() && !overrides.dry_run {
        anyhow::bail!("DISCORD_BOT_TOKEN が未設定（production は token 必須・dry-run 以外）");
    }

    let inst = place
        .instances
        .first()
        .context("validated placement has no instance")?;
    let bytes = decode_config_b64(&inst.config_b64)?;
    let client = spawn_instance(
        socket,
        inst,
        &bytes,
        token,
        overrides,
        attachment_spool_root,
    )?;
    if let (Some(control), Some(nonce)) = (control, start_nonce) {
        let addresses = inst.addresses.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            loop {
                if client.connection_live().await {
                    let mut all_bound = true;
                    for address in &addresses {
                        if client.remembered_binding(address).await.is_none() {
                            all_bound = false;
                            break;
                        }
                    }
                    if all_bound {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            if let Ok(mut stream) = tokio::net::UnixStream::connect(control).await {
                let message = serde_json::json!({"nonce":nonce,"pid":std::process::id()});
                let _ = stream.write_all(message.to_string().as_bytes()).await;
            }
        });
    }

    tracing::info!("discord-gateway running");
    std::future::pending::<()>().await;
    Ok(())
}
