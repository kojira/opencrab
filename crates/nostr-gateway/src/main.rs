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
    let nostaro_bin = PathBuf::from(&place.nostaro_bin);
    // QC ハーネス差し替えは env からのみ（既定 OFF＝production 挙動）。
    let overrides = HarnessOverrides::from_env();
    let gateway_store_path = place
        .gateway_store_path
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| {
            overrides
                .dry_run
                .then(|| path.with_extension("emissions.db"))
        })
        .context("gateway_store_path is required")?;
    let emission_ledger = Arc::new(opencrab_gate_client::emission::EmissionLedger::open(
        &gateway_store_path,
    )?);

    let mut readiness = Vec::new();
    for inst in &place.instances {
        let bytes = decode_config_b64(&inst.config_b64)?;
        let client = spawn_instance(
            socket.clone(),
            inst,
            &bytes,
            secret.clone(),
            nostaro_bin.clone(),
            overrides.clone(),
            Arc::clone(&emission_ledger),
        )?;
        readiness.push((client, inst.address.clone()));
    }
    if let (Some(control), Some(nonce)) = (control, start_nonce) {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            loop {
                let mut ready = true;
                for (client, address) in &readiness {
                    if !client.connection_live().await
                        || client.remembered_binding(address).await.is_none()
                    {
                        ready = false;
                        break;
                    }
                }
                if ready {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            if let Ok(mut stream) = tokio::net::UnixStream::connect(control).await {
                let message = serde_json::json!({"nonce":nonce,"pid":std::process::id()});
                let _ = stream.write_all(message.to_string().as_bytes()).await;
            }
        });
    }

    tracing::info!("nostr-gateway running");
    std::future::pending::<()>().await;
    Ok(())
}
