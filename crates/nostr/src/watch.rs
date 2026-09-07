//! セッションの `session_watches` を実行する新機構（載せ替え工程 5-a / §4）。
//!
//! ゲートはイベントの形だけを見る（誰かを見ない）。
//! 対話系は即時転送、タイムラインは `interval_secs` で束ねて core の inbound 1 口へ。
//! 権限毎デバウンスは core の [`opencrab_actions::PrivilegeFire`]（バッファと時限）。
//! 既存 `inbound_kind_label` は変えない（現行 `nostr-{agent}` のラベルを維持）。

use std::sync::Arc;

use opencrab_actions::{
    accept_inbound, delivery_effect, prepare_session_inbound, start_session_turn, CallerIdentity,
    DeliveryEffect, InboundDrop, InboundLookups, InboundWork, NormalizedInbound,
    NormalizedInboundEvent, RunRequest, WatchAccept,
};
use opencrab_db::queries::SessionWatchRow;
use opencrab_gateway::GatewayActions;

use crate::actions::NostrGatewayActions;
use crate::cli::NostaroCli;
use crate::config::{NostrConfig, NostrFilter};
use crate::event::NostrEvent;
use crate::identity::NostrIdentityAdmin;
use crate::pubkey::follow_key;
use crate::runner::NostrAgentRunner;
use crate::session::NostrSessionRuntime;

/// ゲートの機械的振り分け（§4.2 / §4.3 / §4.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchForward {
    Discard,
    Immediate { label: &'static str },
    Bundle { label: &'static str },
}

/// watch 行の `filter_json` を読む。壊れていたらエラー（空に置き換えない）。
pub fn parse_watch_filter(filter_json: &str) -> anyhow::Result<NostrFilter> {
    let value: serde_json::Value = serde_json::from_str(filter_json)
        .map_err(|e| anyhow::anyhow!("session_watches.filter_json が読めない: {e}"))?;
    if !value.is_object() {
        anyhow::bail!("session_watches.filter_json は JSON object が必須");
    }
    serde_json::from_value(value).map_err(|e| {
        anyhow::anyhow!("session_watches.filter_json が NostrFilter として読めない: {e}")
    })
}

/// watch 行 + 接続リレーから購読設定を組む。`interval_secs` が 1 未満なら起動エラー。
pub fn watch_subscribe_config(
    watch: &SessionWatchRow,
    relays: Vec<String>,
) -> anyhow::Result<NostrConfig> {
    if watch.interval_secs <= 0 {
        anyhow::bail!(
            "session_watches.id={} の interval_secs が正の整数ではない（既定値は使わない）",
            watch.id
        );
    }
    let filter = parse_watch_filter(&watch.filter_json)?;
    Ok(NostrConfig { relays, filter })
}

/// p タグが当人（自 pubkey）を指すか。表記は hex / npub どちらでも同じ鍵。
pub fn p_tag_is_self(event: &NostrEvent, self_pubkey: &str) -> bool {
    let self_key = follow_key(self_pubkey);
    event.tags.iter().any(|t| {
        t.first().map(|s| s == "p").unwrap_or(false)
            && t.get(1).is_some_and(|p| follow_key(p) == self_key)
    })
}

fn has_e_tag(event: &NostrEvent) -> bool {
    event
        .tags
        .iter()
        .any(|t| t.first().map(|s| s == "e").unwrap_or(false))
}

/// e タグが当人（自 pubkey）を指すか。1 欄目または後続欄（NIP-22 の pubkey）を見る。
pub fn e_tag_is_self(event: &NostrEvent, self_pubkey: &str) -> bool {
    let self_key = follow_key(self_pubkey);
    event.tags.iter().any(|t| {
        t.first().map(|s| s == "e").unwrap_or(false)
            && t.iter().skip(1).any(|v| follow_key(v) == self_key)
    })
}

/// watch 経路の機械的ラベル（現行 `inbound_kind_label` とは別。リポストを足す）。
pub fn watch_kind_label(event: &NostrEvent) -> &'static str {
    if event.is_dm() {
        return "DM";
    }
    if event.kind == 7 {
        return "リアクション";
    }
    if event.kind == 6 || event.kind == 16 {
        return "リポスト";
    }
    if event.kind == 30023 {
        return "長文";
    }
    if has_e_tag(event) {
        return "リプライ";
    }
    "メンション"
}

/// 形だけ。誰か（owner / followee）は見ない。
pub fn classify_watch_event(
    event: &NostrEvent,
    self_pubkey: &str,
    watches_beyond_self_mentions: bool,
) -> WatchForward {
    if event.is_dm() {
        return WatchForward::Discard;
    }
    if event.kind == 7 {
        return WatchForward::Immediate {
            label: "リアクション",
        };
    }
    if event.kind == 6 || event.kind == 16 {
        return WatchForward::Immediate {
            label: "リポスト"
        };
    }
    let to_self = p_tag_is_self(event, self_pubkey);
    if event.kind == 30023 {
        // Q15: 長文は束ね側。e/p が当人宛なら即時。
        return if to_self || e_tag_is_self(event, self_pubkey) {
            WatchForward::Immediate { label: "長文" }
        } else {
            WatchForward::Bundle { label: "長文" }
        };
    }
    // kind 1 ほか。mention-only 購読で届いた e 無しは現行どおり自分宛メンション。
    if to_self {
        return if has_e_tag(event) {
            WatchForward::Immediate {
                label: "リプライ"
            }
        } else {
            WatchForward::Immediate {
                label: "メンション",
            }
        };
    }
    if !watches_beyond_self_mentions && !has_e_tag(event) {
        return WatchForward::Immediate {
            label: "メンション",
        };
    }
    WatchForward::Bundle {
        label: watch_kind_label(event),
    }
}

/// タイムライン束ねバッファ（1 watch 1 本）。
#[derive(Debug, Default)]
pub struct TimelineBundle {
    events: Vec<NostrEvent>,
}

impl TimelineBundle {
    pub fn push(&mut self, event: NostrEvent) {
        self.events.push(event);
    }

    pub fn take(&mut self) -> Vec<NostrEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }
}

/// watch イベントの束を core の inbound 1 口へ投げる。
///
/// Discord の DM / whitelist は見ない（#698 の許可集合が決める）。
#[allow(clippy::too_many_arguments)]
pub fn accept_watch_events<R: NostrAgentRunner, T: Send + 'static>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    events: &[NostrEvent],
    labels: &[&str],
    watch: Option<WatchAccept<'_, T>>,
    take_hold: impl FnMut(usize) -> T,
    on_admitted: impl FnMut(usize, &opencrab_actions::AdmittedInbound),
    on_run: impl FnMut(usize, &opencrab_actions::AdmittedInbound, &[usize]),
) -> Result<(), InboundDrop> {
    debug_assert_eq!(events.len(), labels.len());
    let keys: Vec<String> = events.iter().map(|e| follow_key(&e.pubkey)).collect();
    let works: Vec<InboundWork<'_>> = events
        .iter()
        .enumerate()
        .map(|(i, e)| InboundWork {
            event: NormalizedInboundEvent {
                sender_id: &e.pubkey,
                channel_id: session_id,
                guild_id: "nostr",
            },
            has_content: true,
            kind_label: labels[i],
            author_key: &keys[i],
        })
        .collect();
    let resolve =
        |sender: &str, _: &[String], _: &str| runner.resolve_nostr_caller(agent_id, sender);
    let sid = session_id.to_string();
    let dm_any = |sender: &str, _: &[String], owner: &str| sender == owner;
    let dm_one = |sender: &str, _: &str, owner: &str| sender == owner;
    let wl = move |channel: &str, aid: &str| {
        channel == sid || channel == crate::session::nostr_session_id(aid)
    };
    let owner_owned = watch
        .as_ref()
        .and_then(|w| w.owner.iter().next())
        .cloned()
        .unwrap_or_default();
    let lookups = InboundLookups {
        resolve_caller: &resolve,
        dm_allowed_any: &dm_any,
        dm_allowed: &dm_one,
        channel_whitelisted: &wl,
    };
    accept_inbound(
        &works,
        &owner_owned,
        &[agent_id.to_string()],
        &lookups,
        watch,
        take_hold,
        on_admitted,
        on_run,
    )
}

/// watch ターンを inbound 口で起動し、配送 effect を返す。
#[allow(clippy::too_many_arguments)]
pub async fn run_watch_turn<R: NostrAgentRunner>(
    runner: &R,
    cli: &NostaroCli,
    admin: &Arc<dyn NostrIdentityAdmin>,
    runtime: &Arc<NostrSessionRuntime>,
    inbound: &NormalizedInbound<'_>,
    caller: CallerIdentity,
    reply_target: &str,
    prompt_suffix: &str,
    trigger_message_id: Option<&str>,
) -> DeliveryEffect {
    let actions: Arc<dyn GatewayActions> =
        Arc::new(NostrGatewayActions::new(cli.clone()).with_admin(admin.clone()));
    let registry = runtime.registry_for(inbound.session_id);
    let agent_id = inbound.agent_id.to_string();
    let session_id = inbound.session_id.to_string();
    let reply_target = reply_target.to_string();
    let prompt_suffix = prompt_suffix.to_string();
    let trigger = trigger_message_id.map(str::to_string);
    // 予算計上（fail-loud）は実 request と同じ system_prompt で行う必要があるため、
    // closure の外で先に組む。nostr は会話へ runtime context を前置しない（wrap は素通し）。
    let (base_prompt, agent_name) = runner.build_agent_context(&agent_id, &caller);
    let system_prompt = format!("{base_prompt}\n\n{prompt_suffix}");
    let result = start_session_turn(
        runner,
        opencrab_actions::TranscriptSource::Nostr,
        inbound,
        &system_prompt,
        "",
        |raw| raw.to_string(),
        |conversation| {
            let mut req = RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                system_prompt.clone(),
                conversation,
                "nostr",
                caller.clone(),
            )
            .with_gateway_actions(actions.clone())
            .with_dispatch(
                Some(registry.clone()),
                Arc::new(crate::sink::NostrResponder::new(
                    runner.clone(),
                    cli.clone(),
                    runtime.clone(),
                    admin.clone(),
                    &agent_id,
                )),
            )
            .with_reply_target(reply_target.clone())
            .with_live_inbound_scope(opencrab_actions::LiveInboundScope::OnlySpeaker(
                inbound.sender_id.to_string(),
            ));
            if let Some(id) = trigger.as_deref() {
                req = req.with_trigger_message_id(id.to_string());
            }
            req
        },
    )
    .await;
    match result {
        Some(r) => delivery_effect(
            r,
            opencrab_actions::DeliveryContext {
                session_id: &session_id,
                agent_id: &agent_id,
                origin: "nostr",
            },
        ),
        None => DeliveryEffect::Empty,
    }
}

/// 記録 + ターン起動の前段（ensure → record）。
pub fn prepare_watch_inbound<R: NostrAgentRunner>(
    runner: &R,
    session_id: &str,
    agent_id: &str,
    event: &NostrEvent,
    recorded_text: &str,
) -> bool {
    let inbound = NormalizedInbound {
        session_id,
        agent_id,
        sender_id: &event.pubkey,
        sender_name: &event.author_label(),
        avatar_url: None,
        channel_id: None,
        pubkey: Some(&event.pubkey),
        text: recorded_text,
        image_urls: &[],
        external_id: &event.id,
    };
    prepare_session_inbound(
        runner,
        opencrab_actions::TranscriptSource::Nostr,
        &inbound,
        "Nostr",
        "{}",
        "nostr",
    )
}

/// DeliveryEffect に応じて outbound を記録する（機構は publish しない / #588）。
pub fn apply_watch_effect<R: NostrAgentRunner>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    reply_target: &str,
    effect: &DeliveryEffect,
) {
    match effect {
        DeliveryEffect::Text { body, .. } => {
            let recorded = if reply_target.is_empty() {
                body.clone()
            } else {
                format!(
                    "{body}\n{anchor}",
                    anchor = crate::event::outbound_reply_anchor(reply_target)
                )
            };
            runner.record_outbound_reply(
                opencrab_actions::TranscriptSource::Nostr,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id,
                    session_id,
                    channel_id: None,
                    text: &recorded,
                    context: None,
                },
            );
        }
        DeliveryEffect::Failed { error } => {
            tracing::error!(agent_id, session_id, error = %error, "watch turn failed");
        }
        DeliveryEffect::NoReply | DeliveryEffect::Empty => {}
    }
}

/// 束ね本文を 1 ターンの文脈に載せる（Q17: 本体の退避を 1 件ずつ流用）。
pub fn recorded_watch_text<R: NostrAgentRunner>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    event: &NostrEvent,
) -> String {
    opencrab_actions::sanitize_tool_result_for_log(
        "nostr_inbound",
        &event.inbound_text(),
        session_id,
        &event.id,
        runner.agent_workspace_root(agent_id).as_deref(),
    )
}

pub fn watch_prompt_suffix(event: &NostrEvent, label: &str) -> String {
    format!(
        "[Nostr] {author} さんの投稿への応答です。\n\
         - 送信者: {author_key}（pubkey={pubkey}）\n\
         - 対象ノート: {target}\n\
         - 種別: kind:{kind}（{label}）\n\
         返信するなら nostr_reply(target=\"{target}\") を使ってください（target は返信先ノート）。\
         種別的に本文返信が不自然なもの（リアクション等）や、返信不要なら \
         NO_REPLY とだけ答えてください。",
        author = event.author_label(),
        author_key = event.author_key(),
        pubkey = event.pubkey,
        target = event.reply_target(),
        kind = event.kind,
        label = label,
    )
}

pub fn watch_bundle_prompt_suffix(events: &[NostrEvent]) -> String {
    format!(
        "[Nostr] タイムライン watch の束ね（{} 件）です。窓内を 1 ターンの文脈に載せています。\
         返信するなら最後の対象ノートへ nostr_reply を使ってください。不要なら NO_REPLY とだけ答えてください。",
        events.len()
    )
}

#[cfg(test)]
mod tests;
