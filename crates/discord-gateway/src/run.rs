//! 1 instance の UDS client・受信ループ・say consumer を束ねる（設計 §0・§4.3・§5・§6）。
//!
//! Nostr gateway と違い timeline/bundle 車線は無い。1 channel = 1 binding、1 Message Create = 1 said。
//! 受信は serenity（または fake fixture）を 1 本回し、ack 済み binding の channel だけ said にする。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, LiveEvent, PostRefuse, SaidOutcome};
use opencrab_gate_client::{InvokeHandler, SayPolicy};

use crate::config::{config_digest, parse_instance_config, InstanceConfig, InstancePlacement};
use crate::harness::HarnessOverrides;
use crate::map::{address_of, map_message, parse_address, parse_event_line};
use crate::ops::{operation_declarations, DiscordInvokeHandler};
use crate::post::{deliver_say, SayDelivery};
use crate::receive::{run_fake_events_once, run_serenity_receive, OnLine};
use crate::transport::{DiscordTransport, DryRunTransport, SerenityTransport};

const RETRY: Duration = Duration::from_millis(200);

/// 1 instance を起動する。hello に能力宣言を載せて接続し、受信ループと say consumer を spawn する。
pub fn spawn_instance(
    socket: PathBuf,
    place: &InstancePlacement,
    config_bytes: &[u8],
    token: Option<Arc<String>>,
    overrides: HarnessOverrides,
) -> anyhow::Result<Arc<InstanceClient>> {
    if overrides.is_active() {
        tracing::warn!(
            fake_events = ?overrides.fake_events,
            dry_run = overrides.dry_run,
            instance = %place.instance_id,
            "QC harness overrides ACTIVE — this is NOT a production path"
        );
    }
    let cfg = parse_instance_config(config_bytes)?;
    let digest = config_digest(config_bytes);

    // transport: dry-run（REST を叩かずログ）か production（serenity REST・token 保持）。
    // say も invoke も同一 transport を通すので dry-run 分岐は 1 箇所。
    let transport: Arc<dyn DiscordTransport> = if overrides.dry_run {
        Arc::new(DryRunTransport)
    } else {
        let token = token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("production は bot token（env）が必須"))?;
        Arc::new(SerenityTransport::new(&token))
    };

    let invoke_handler: Arc<dyn InvokeHandler> =
        Arc::new(DiscordInvokeHandler::new(transport.clone()));

    let client = InstanceClient::spawn_with_operations(
        socket,
        place.instance_id.clone(),
        place.revision,
        cfg.self_bot_id.clone(),
        digest,
        SayPolicy::AcceptToLiveQueue,
        Some(operation_declarations()),
        invoke_handler,
    );

    supervise(
        client.clone(),
        place.addresses.clone(),
        cfg,
        transport,
        token,
        overrides,
    );
    Ok(client)
}

fn supervise(
    client: Arc<InstanceClient>,
    addresses: Vec<String>,
    cfg: InstanceConfig,
    transport: Arc<dyn DiscordTransport>,
    token: Option<Arc<String>>,
    overrides: HarnessOverrides,
) {
    // 受信ループ（1 本）: fixture か serenity。ack 済み binding の channel だけ said にする。
    let on_line = build_on_line(client.clone(), cfg.agent_id.clone(), cfg.self_bot_id.clone());
    tokio::spawn(async move {
        if let Some(fixture) = overrides.fake_events {
            if let Err(e) = run_fake_events_once(&fixture, on_line).await {
                tracing::error!(error = %e, "fake events failed");
            }
        } else if let Some(token) = token {
            if let Err(e) = run_serenity_receive(&token, on_line).await {
                tracing::error!(error = %e, "serenity receive ended");
            }
        } else {
            tracing::error!("no fake_events fixture and no token; receive loop idle");
        }
    });

    // say consumer（address ごと）: core の say を channel の通常投稿として配送する。
    for address in addresses {
        spawn_say_consumer(
            client.clone(),
            address,
            transport.clone(),
            cfg.agent_id.clone(),
        );
    }
}

fn build_on_line(client: Arc<InstanceClient>, agent_id: String, self_bot_id: String) -> OnLine {
    Arc::new(move |line: String| {
        let client = client.clone();
        let agent_id = agent_id.clone();
        let self_bot_id = self_bot_id.clone();
        tokio::spawn(async move {
            handle_incoming(&client, &agent_id, &self_bot_id, &line).await;
        });
    })
}

/// 受信 1 件を said へ。自分の投稿と非 ack channel は core へ送らない（§4.3・§5.1）。
async fn handle_incoming(
    client: &InstanceClient,
    agent_id: &str,
    self_bot_id: &str,
    line: &str,
) {
    let Some(msg) = parse_event_line(line) else {
        return;
    };
    // map_message が自分の投稿・非 snowflake を落とす。author は Discord 認証済み sender（#848）。
    let Some(mapped) = map_message(&msg, self_bot_id) else {
        return;
    };
    let address = address_of(agent_id, &msg);
    // 購読集合: core が bind ack した channel だけを said にする（wire discovery を追加しない）。
    if client.binding_for_address(&address).await.is_none() {
        tracing::debug!(%address, "message outside ack'd binding; discarded (no core frame)");
        return;
    }
    match client
        .post_said_with_author(&address, &mapped.origin, &mapped.author_id, &mapped.text, &[])
        .await
    {
        Ok(SaidOutcome::Accepted { seq }) => {
            tracing::info!(%address, seq, "said accepted")
        }
        Ok(SaidOutcome::NotAdmitted) => tracing::info!(%address, "said not admitted"),
        Ok(SaidOutcome::Disconnected) => tracing::info!(%address, "said disconnected"),
        Ok(SaidOutcome::WireErr { code, detail }) => {
            tracing::warn!(%address, code, ?detail, "said wire err")
        }
        Err(PostRefuse::NotReady) => tracing::info!(%address, "said dropped; binding not ready"),
        Err(PostRefuse::Busy) => tracing::info!(%address, "said refused; binding busy"),
    }
}

fn spawn_say_consumer(
    client: Arc<InstanceClient>,
    address: String,
    transport: Arc<dyn DiscordTransport>,
    agent_id: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // address → channel snowflake（say はこの channel への通常投稿）。
        let channel = parse_address(&agent_id, &address).map(|(_, ch)| ch);
        loop {
            match client.next_live(&address).await {
                Some(LiveEvent::Message { text, .. }) => {
                    // DI-16: say は通常発言。reply_origin は使わない（明示 reply 能力が返信を担う）。
                    let Some(channel) = &channel else {
                        tracing::warn!(%address, "say dropped; address has no channel component");
                        continue;
                    };
                    match deliver_say(&transport, channel, &text).await {
                        SayDelivery::Posted => tracing::info!(%address, "say posted"),
                        SayDelivery::Failed(e) => {
                            tracing::warn!(%address, error = %e, "say post failed")
                        }
                    }
                }
                Some(LiveEvent::Error { .. }) | None => {
                    tokio::time::sleep(RETRY).await;
                }
                // Activity / CompletedNoReply は投稿対象ではない。
                Some(_) => {}
            }
        }
    })
}
