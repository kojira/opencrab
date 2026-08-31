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

    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: discord-gateway <placement.json>")?;
    // bot token を env から 1 回だけ受け、直後に process env から消す（設計 §1.3・§5）。
    let token = take_bot_token().map(Arc::new);
    let place = Placement::load(&path)?;
    let socket = PathBuf::from(&place.core_socket);
    // QC ハーネス差し替えは env からのみ（既定 OFF＝production 挙動）。
    let overrides = HarnessOverrides::from_env();

    if token.is_none() && !overrides.dry_run {
        anyhow::bail!("DISCORD_BOT_TOKEN が未設定（production は token 必須・dry-run 以外）");
    }

    for inst in &place.instances {
        let bytes = decode_config_b64(&inst.config_b64)?;
        spawn_instance(
            socket.clone(),
            inst,
            &bytes,
            token.clone(),
            overrides.clone(),
        )?;
    }

    tracing::info!("discord-gateway running");
    std::future::pending::<()>().await;
    Ok(())
}
