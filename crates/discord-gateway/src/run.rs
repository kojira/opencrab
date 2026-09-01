//! 1 instance の UDS client・受信ループ・say consumer を束ねる（設計 §0・§4.3・§5・§6）。
//!
//! Nostr gateway と違い timeline/bundle 車線は無い。1 channel = 1 binding、1 Message Create = 1 said。
//! 受信は serenity（または fake fixture）を 1 本回し、ack 済み binding の channel だけ said にする。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, LiveEvent, PostRefuse, SaidOutcome};
use opencrab_gate_client::{InvokeHandler, SayPolicy};

use crate::config::{
    config_digest, parse_instance_config, InstanceConfig, InstancePlacement, SystemReactions,
};
use crate::harness::HarnessOverrides;
use crate::map::{address_of, map_message, parse_address, parse_event_line, parse_origin};
use crate::ops::{operation_declarations, DiscordInvokeHandler};
use crate::post::{deliver_say, SayDelivery};
use crate::receive::{run_fake_events_once, run_serenity_receive, OnLine};
use crate::transport::{DiscordTransport, DryRunTransport, SerenityTransport, TransportOutcome};

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
    // 👀 は受信時ではなく say consumer 側（activity started）で付けるので、受信は transport/
    // reactions を持たない（R2）。
    let on_line = build_on_line(
        client.clone(),
        cfg.agent_id.clone(),
        cfg.self_bot_id.clone(),
    );
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
            cfg.system_reactions.clone(),
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

/// system reaction を発端メッセージ（origin anchor）へ best-effort で付ける。👀（受理）と
/// ❌（配送失敗）はこちら——発端に付けるのが正（照合: 旧 message_loop も同じ側）。
async fn react_system(transport: &Arc<dyn DiscordTransport>, origin: &str, emoji: &str) {
    let Some((channel, message)) = parse_origin(origin) else {
        tracing::debug!(%origin, "skip system reaction: origin not a discord anchor");
        return;
    };
    react_system_on(transport, &channel, &message, emoji).await;
}

/// (channel, message) を直接指定して system reaction を付ける（失敗は warn のみ・非致命）。
/// 🏁（完了）は**自分が投稿した say のメッセージ**へ付けるため、origin anchor ではなく
/// transport の create_message 応答から得た自分の message id をここへ渡す（owner 裁定 row 345）。
/// legacy `add_reaction_non_fatal` と同じく、付与失敗が turn 処理を巻き込まないようにする。
async fn react_system_on(
    transport: &Arc<dyn DiscordTransport>,
    channel: &str,
    message: &str,
    emoji: &str,
) {
    match transport.add_system_reaction(channel, message, emoji).await {
        TransportOutcome::Ok(_) => {}
        TransportOutcome::Rejected => {
            tracing::warn!(
                channel,
                message,
                emoji,
                "system reaction rejected (non-fatal)"
            )
        }
        TransportOutcome::Indeterminate => {
            tracing::warn!(
                channel,
                message,
                emoji,
                "system reaction outcome unknown (non-fatal)"
            )
        }
    }
}

/// 受信 1 件を said へ。自分の投稿と非 ack channel は core へ送らない（§4.3・§5.1）。
async fn handle_incoming(client: &InstanceClient, agent_id: &str, self_bot_id: &str, line: &str) {
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
        .post_said_with_author(
            &address,
            &mapped.origin,
            &mapped.author_id,
            &mapped.text,
            &[],
        )
        .await
    {
        Ok(SaidOutcome::Accepted { seq }) => {
            // 👀 はここ（受理・推論前）では付けない。オーナー確定仕様: LLM がこのメッセージを
            // ターン文脈に含めた（読んだ）時点で付ける。record-only は読まれるまで付けない。
            // 実際の付与は say consumer が activity started(origin) を受けた時点で行う（R2）。
            tracing::info!(%address, seq, "said accepted");
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
    reactions: SystemReactions,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // address → channel snowflake（say はこの channel への通常投稿）。
        let channel = parse_address(&agent_id, &address).map(|(_, ch)| ch);
        loop {
            match client.next_live(&address).await {
                Some(LiveEvent::Message { text, reply_origin }) => {
                    // DI-16: say は通常発言（reply target は暗黙設定しない）。reply_origin が発端
                    // メッセージ（即時ターンの Single）を指すときだけ system reaction を駆動する
                    // （bundle/曖昧 None は対象外・#869 のゲート踏襲）。付け先は記号ごとに異なる:
                    //   🏁 完了 → **自分が投稿した say のメッセージ**（owner 裁定 row 345・#869 の
                    //             発端付けを是正。分割投稿ならこの say の id ＝ 直近の自分の発言）。
                    //   ❌ 失敗 → 発端メッセージ（自分の発言は生まれていない・照合: 旧実装も発端）。
                    let Some(channel) = &channel else {
                        tracing::warn!(%address, "say dropped; address has no channel component");
                        continue;
                    };
                    match deliver_say(&transport, channel, &text).await {
                        SayDelivery::Posted { message_id } => {
                            tracing::info!(%address, "say posted");
                            if reply_origin.is_some() {
                                if let Some(own) = &message_id {
                                    // 🏁: 自分が投稿した say へ「このターンはもう続きの処理をしない」。
                                    // 分割投稿なら message_id は **最後のチャンク**（deliver_say が
                                    // 返す）＝直近の自分の発言なので付け先が正しい。
                                    react_system_on(&transport, channel, own, &reactions.completed)
                                        .await;
                                } else {
                                    tracing::debug!(
                                        %address,
                                        "skip 🏁: posted say has no message id"
                                    );
                                }
                            }
                        }
                        SayDelivery::Failed(e) => {
                            tracing::warn!(%address, error = %e, "say post failed");
                            if let Some(origin) = &reply_origin {
                                // ❌: 発端メッセージへの返信配送が失敗した（失敗サイン）。
                                react_system(&transport, origin, &reactions.failed).await;
                            }
                        }
                    }
                }
                Some(LiveEvent::Activity { state, origin, .. }) => {
                    // 👀: LLM がこの発端メッセージをターン文脈に含めた時点（started+origin）で付ける。
                    // record-only/held は started へ来ないので「読まれるまで付かない」が保たれる（R2）。
                    if state == "started" {
                        if let Some(origin) = &origin {
                            react_system(&transport, origin, &reactions.accepted).await;
                        }
                    }
                }
                Some(LiveEvent::CompletedNoReply { reply_origin }) => {
                    // 🤐: ターンが沈黙（say 無し）で終えた。裁定A で core が ended を say の後に出す
                    // ため、返信ターンでは立たず真の沈黙ターンだけに立つ。発端（即時ターンの Single）が
                    // 分かるときだけその発端メッセージへ NO_REPLY サインを付ける（None は付けない）。
                    if let Some(origin) = &reply_origin {
                        react_system(&transport, origin, &reactions.no_reply).await;
                    }
                }
                Some(LiveEvent::TurnFailed { reply_origin }) => {
                    // ❌: ターン失敗（エンジン/プロバイダ失敗）を発端メッセージへ可視化する（R3）。
                    // エラー本文はチャンネルへ出さない（wire にも載っていない・#668）。
                    react_system(&transport, &reply_origin, &reactions.failed).await;
                }
                Some(LiveEvent::Error { .. }) | None => {
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    })
}
