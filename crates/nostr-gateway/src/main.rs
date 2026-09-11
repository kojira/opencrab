//! nostr-gateway 独立 binary。watch JSONL ⇄ V3 UDS。HTTP listen しない。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use opencrab_nostr_gateway::config::{decode_config_b64, Placement};
use opencrab_nostr_gateway::daemon::DaemonConfig;
use opencrab_nostr_gateway::harness::HarnessOverrides;
use opencrab_nostr_gateway::run::spawn_instance;
use opencrab_nostr_gateway::secret::take_watch_secret;

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
                .add_directive("opencrab_nostr_gateway=info".parse()?)
                .add_directive("opencrab_gate_client=info".parse()?)
                .add_directive("nostr_gateway=info".parse()?),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .context("usage: nostr-gateway daemon <config.json> | instance <placement.json>")?;
    if first == "daemon" {
        let path = args
            .next()
            .map(PathBuf::from)
            .context("usage: nostr-gateway daemon <config.json>")?;
        return opencrab_nostr_gateway::daemon::run(DaemonConfig::load(&path)?).await;
    }
    let path = if first == "instance" {
        args.next()
            .map(PathBuf::from)
            .context("usage: nostr-gateway instance <placement.json>")?
    } else {
        // Temporary CLI compatibility for existing isolated harnesses. This does not create a
        // second runtime path: both forms execute the same instance owner.
        PathBuf::from(first)
    };
    let secret = take_watch_secret().map(Arc::new);
    let place = Placement::load(&path)?;
    let socket = PathBuf::from(&place.core_socket);
    let nostaro_bin = PathBuf::from(&place.nostaro_bin);
    // QC ハーネス差し替えは env からのみ（既定 OFF＝production 挙動）。
    let overrides = HarnessOverrides::from_env();

    for inst in &place.instances {
        let bytes = decode_config_b64(&inst.config_b64)?;
        spawn_instance(
            socket.clone(),
            inst,
            &bytes,
            secret.clone(),
            nostaro_bin.clone(),
            overrides.clone(),
        )?;
    }

    tracing::info!("nostr-gateway running");
    std::future::pending::<()>().await;
    Ok(())
}
