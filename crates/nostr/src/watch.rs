//! セッションの `session_watches` を実行する新機構（載せ替え工程 5-a / §4）。
//!
//! ゲートはイベントの形だけを見る（誰かを見ない）。
//! 対話系は即時転送、タイムラインは `interval_secs` で束ねて core の inbound 1 口へ。
//! core が `Debounce { interval_secs }` を返したら、その間隔で flush する（権限毎）。
//! 既存 `inbound_kind_label` は変えない（現行 `nostr-{agent}` のラベルを維持）。

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use opencrab_actions::{
    decide_watch_turn, delivery_effect, plan_watch_inbound, prepare_session_inbound,
    start_session_turn, watch_author_standing, CallerIdentity, DeliveryEffect, InboundIdentity,
    NormalizedInbound, NormalizedInboundEvent, RunRequest, WatchAllowSets, WatchInboundDrop,
    WatchTurnDecision,
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

/// core が返した権限毎間隔で抱える対話系（`Debounce { interval_secs }`）。
///
/// 同じ間隔の件は 1 つの窓に入る。flush 期限は最初の 1 件が入った瞬間 + その間隔。
#[derive(Debug, Default)]
pub struct DebounceHold {
    buckets: BTreeMap<u64, DebounceBucket>,
}

#[derive(Debug)]
struct DebounceBucket {
    events: Vec<NostrEvent>,
    due: tokio::time::Instant,
}

impl DebounceHold {
    pub fn push(&mut self, event: NostrEvent, interval_secs: u64) {
        self.push_at(event, interval_secs, tokio::time::Instant::now());
    }

    pub fn push_at(&mut self, event: NostrEvent, interval_secs: u64, now: tokio::time::Instant) {
        self.buckets
            .entry(interval_secs)
            .or_insert_with(|| DebounceBucket {
                events: Vec::new(),
                due: now + Duration::from_secs(interval_secs),
            })
            .events
            .push(event);
    }

    pub fn next_due(&self) -> Option<tokio::time::Instant> {
        self.buckets.values().map(|b| b.due).min()
    }

    pub fn intervals(&self) -> Vec<u64> {
        self.buckets.keys().copied().collect()
    }

    pub fn take_ready(&mut self, now: tokio::time::Instant) -> Vec<(u64, Vec<NostrEvent>)> {
        let keys: Vec<u64> = self
            .buckets
            .iter()
            .filter(|(_, b)| b.due <= now)
            .map(|(&k, _)| k)
            .collect();
        keys.into_iter()
            .filter_map(|k| self.buckets.remove(&k).map(|b| (k, b.events)))
            .collect()
    }

    pub fn take_all(&mut self) -> Vec<(u64, Vec<NostrEvent>)> {
        std::mem::take(&mut self.buckets)
            .into_iter()
            .map(|(k, b)| (k, b.events))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.buckets.values().map(|b| b.events.len()).sum()
    }
}

/// Nostr watch 用の [`InboundIdentity`]。Discord の DM / whitelist は適用しない
/// （#698 と `plan_watch_inbound` が許可を決める）。
pub struct NostrWatchIdentity<'a, R: NostrAgentRunner> {
    pub runner: &'a R,
    pub agent_id: &'a str,
}

impl<R: NostrAgentRunner> InboundIdentity for NostrWatchIdentity<'_, R> {
    fn resolve_caller(
        &self,
        sender_id: &str,
        _agent_ids: &[String],
        _owner_id: &str,
    ) -> CallerIdentity {
        self.runner.resolve_nostr_caller(self.agent_id, sender_id)
    }

    fn dm_allowed_any(&self, _sender_id: &str, _agent_ids: &[String], _owner_id: &str) -> bool {
        true
    }

    fn dm_allowed(&self, _sender_id: &str, _agent_id: &str, _owner_id: &str) -> bool {
        true
    }

    fn is_channel_whitelisted_for_agent(&self, _channel_id: &str, _agent_id: &str) -> bool {
        true
    }
}

/// core が 1 件を即応するか。ゲートは呼ばない。
pub fn evaluate_watch_item(
    policy_json: &str,
    author_pubkey: &str,
    caller: &CallerIdentity,
    kind_label: &str,
    watch_interval_secs: u64,
    owner: &HashSet<String>,
    followees: &HashSet<String>,
) -> anyhow::Result<WatchTurnDecision> {
    let author_key = follow_key(author_pubkey);
    let standing = watch_author_standing(&author_key, owner, followees);
    decide_watch_turn(
        policy_json,
        standing,
        caller,
        kind_label,
        watch_interval_secs,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// 許可集合と inbound 1 口。落とす / 通す / caller。
pub fn admit_watch_item<R: NostrAgentRunner>(
    runner: &R,
    agent_id: &str,
    session_id: &str,
    author_pubkey: &str,
    allow: &WatchAllowSets<'_>,
) -> Result<CallerIdentity, WatchInboundDrop> {
    let identity = NostrWatchIdentity { runner, agent_id };
    let event = NormalizedInboundEvent {
        sender_id: author_pubkey,
        channel_id: session_id,
        guild_id: "nostr",
    };
    let plan = plan_watch_inbound(
        &identity,
        &event,
        "",
        &[agent_id.to_string()],
        &follow_key(author_pubkey),
        allow,
    )?;
    Ok(plan.caller)
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
    let result = start_session_turn(
        runner,
        opencrab_actions::TranscriptSource::Nostr,
        inbound,
        |raw| raw.to_string(),
        |conversation| {
            let (base_prompt, agent_name) = runner.build_agent_context(&agent_id, &caller);
            let system_prompt = format!("{base_prompt}\n\n{prompt_suffix}");
            let mut req = RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                system_prompt,
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
        Some(r) => delivery_effect(r),
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
mod tests {
    use super::*;
    use opencrab_actions::AGREED_IMMEDIATE_KINDS;

    fn ev(kind: u32, tags: Vec<Vec<String>>) -> NostrEvent {
        NostrEvent {
            id: "id1".into(),
            pubkey: "aa".repeat(32),
            npub: None,
            note_id: Some("note1x".into()),
            author_name: None,
            created_at: 1,
            kind,
            content: "hi".into(),
            tags,
        }
    }

    const SELF: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn existing_inbound_kind_label_does_not_call_repost() {
        // 現行ラベルは kind 6 をリポストにしない（純増・既存挙動）。
        let mut e = ev(6, vec![]);
        assert_eq!(e.inbound_kind_label(), "メンション");
        e.tags = vec![vec!["e".into(), "x".into()]];
        assert_eq!(e.inbound_kind_label(), "リプライ");
    }

    #[test]
    fn classify_interactive_immediate() {
        let p_self = vec![vec!["p".into(), SELF.to_string()]];
        let p_e_self = vec![
            vec!["e".into(), "note".into()],
            vec!["p".into(), SELF.to_string()],
        ];
        assert_eq!(
            classify_watch_event(&ev(7, vec![]), SELF, true),
            WatchForward::Immediate {
                label: "リアクション"
            }
        );
        assert_eq!(
            classify_watch_event(&ev(6, vec![]), SELF, true),
            WatchForward::Immediate {
                label: "リポスト"
            }
        );
        assert_eq!(
            classify_watch_event(&ev(16, vec![]), SELF, true),
            WatchForward::Immediate {
                label: "リポスト"
            }
        );
        assert_eq!(
            classify_watch_event(&ev(1, p_e_self), SELF, true),
            WatchForward::Immediate {
                label: "リプライ"
            }
        );
        assert_eq!(
            classify_watch_event(&ev(1, p_self.clone()), SELF, true),
            WatchForward::Immediate {
                label: "メンション"
            }
        );
        assert_eq!(
            classify_watch_event(&ev(30023, p_self), SELF, true),
            WatchForward::Immediate { label: "長文" }
        );
        assert_eq!(
            classify_watch_event(
                &ev(30023, vec![vec!["e".into(), SELF.to_string()]]),
                SELF,
                true
            ),
            WatchForward::Immediate { label: "長文" }
        );
        assert_eq!(
            classify_watch_event(
                &ev(
                    30023,
                    vec![vec![
                        "e".into(),
                        "noteid".into(),
                        String::new(),
                        "reply".into(),
                        SELF.to_string()
                    ]]
                ),
                SELF,
                true
            ),
            WatchForward::Immediate { label: "長文" }
        );
    }

    #[test]
    fn classify_timeline_bundle() {
        let other = ev(1, vec![vec!["p".into(), "cc".repeat(32)]]);
        assert_eq!(
            classify_watch_event(&other, SELF, true),
            WatchForward::Bundle {
                label: "メンション"
            }
        );
        assert_eq!(
            classify_watch_event(&ev(30023, vec![]), SELF, true),
            WatchForward::Bundle { label: "長文" }
        );
        assert_eq!(
            classify_watch_event(
                &ev(30023, vec![vec!["e".into(), "cc".repeat(32)]]),
                SELF,
                true
            ),
            WatchForward::Bundle { label: "長文" }
        );
        let dm = ev(4, vec![]);
        assert_eq!(classify_watch_event(&dm, SELF, true), WatchForward::Discard);
    }

    #[test]
    fn mention_only_watch_treats_kind1_as_mention() {
        let e = ev(1, vec![]);
        assert_eq!(
            classify_watch_event(&e, SELF, false),
            WatchForward::Immediate {
                label: "メンション"
            }
        );
    }

    #[test]
    fn bundle_flush_keeps_order() {
        let mut b = TimelineBundle::default();
        b.push(ev(1, vec![]));
        let mut e2 = ev(1, vec![]);
        e2.id = "id2".into();
        b.push(e2);
        assert_eq!(b.len(), 2);
        let taken = b.take();
        assert_eq!(taken[0].id, "id1");
        assert_eq!(taken[1].id, "id2");
        assert!(b.is_empty());
    }

    #[test]
    fn watch_filter_rejects_garbage() {
        assert!(parse_watch_filter("").is_err());
        assert!(parse_watch_filter("[]").is_err());
        assert!(parse_watch_filter("{}").is_ok());
    }

    #[test]
    fn watch_config_rejects_non_positive_interval() {
        let w = SessionWatchRow {
            id: 1,
            session_id: "nostr-a".into(),
            agent_id: "a".into(),
            interval_secs: 0,
            filter_json: "{}".into(),
            created_at: "t".into(),
        };
        assert!(watch_subscribe_config(&w, vec![]).is_err());
    }

    /// mock E2E: 束ね発火 / 即時転送 / ポリシー判定を 1 本のハーネスで固定する。
    struct MockE2E {
        self_pk: String,
        beyond: bool,
        interval: u64,
        policy: String,
        owner: HashSet<String>,
        followees: HashSet<String>,
        callers: std::collections::HashMap<String, CallerIdentity>,
        allow_extra: HashSet<String>,
        bundle: TimelineBundle,
        debounce: DebounceHold,
        pub turns: Vec<String>,
        pub dropped: Vec<String>,
        pub prepares: Vec<String>,
        pub relays: Vec<String>,
    }

    impl MockE2E {
        fn new() -> Self {
            Self {
                self_pk: SELF.into(),
                beyond: true,
                interval: 60,
                policy: "{}".into(),
                owner: HashSet::new(),
                followees: HashSet::new(),
                callers: std::collections::HashMap::new(),
                allow_extra: HashSet::new(),
                bundle: TimelineBundle::default(),
                debounce: DebounceHold::default(),
                turns: Vec::new(),
                dropped: Vec::new(),
                prepares: Vec::new(),
                relays: Vec::new(),
            }
        }

        fn prepare(&mut self, event: &NostrEvent) {
            self.prepares.push(event.id.clone());
            self.relays.push(event.id.clone());
        }

        fn feed(&mut self, event: NostrEvent) {
            match classify_watch_event(&event, &self.self_pk, self.beyond) {
                WatchForward::Discard => {
                    self.dropped.push(format!("dm:{}", event.id));
                }
                WatchForward::Bundle { label } => {
                    self.bundle.push(event);
                    let _ = label;
                }
                WatchForward::Immediate { label } => {
                    let key = follow_key(&event.pubkey);
                    let allow_followees = &self.followees;
                    let allow_owner = &self.owner;
                    let empty = HashSet::new();
                    let allow = WatchAllowSets {
                        followees: allow_followees,
                        owner: allow_owner,
                        co_agents: &empty,
                        trusted_users: &self.allow_extra,
                    };
                    if !allow.is_allowed(&key) {
                        self.dropped.push(format!("allow:{}", event.id));
                        return;
                    }
                    let caller = self
                        .callers
                        .get(&event.pubkey)
                        .cloned()
                        .unwrap_or(CallerIdentity::Agent);
                    let standing = watch_author_standing(&key, &self.owner, &self.followees);
                    let d =
                        decide_watch_turn(&self.policy, standing, &caller, label, self.interval)
                            .unwrap();
                    match d {
                        WatchTurnDecision::Immediate => {
                            self.prepare(&event);
                            self.turns.push(format!("immediate:{label}:{}", event.id));
                        }
                        WatchTurnDecision::Debounce { interval_secs } => {
                            self.debounce.push(event, interval_secs);
                        }
                    }
                }
            }
        }

        fn flush_bundle(&mut self) {
            let evs = self.bundle.take();
            if evs.is_empty() {
                return;
            }
            for e in &evs {
                self.prepare(e);
            }
            let ids: Vec<_> = evs.iter().map(|e| e.id.as_str()).collect();
            self.turns.push(format!("bundle:{}", ids.join(",")));
        }

        fn flush_debounce(&mut self) {
            let groups = self.debounce.take_all();
            if groups.is_empty() {
                return;
            }
            let mut ids = Vec::new();
            for (interval, evs) in groups {
                for e in &evs {
                    self.prepare(e);
                    ids.push(format!("{}:{}:{interval}", watch_kind_label(e), e.id));
                }
            }
            self.turns.push(format!("debounce:{}", ids.join(",")));
        }
    }

    #[test]
    fn e2e_bundle_fire() {
        let mut h = MockE2E::new();
        h.feed(ev(1, vec![vec!["p".into(), "cc".repeat(32)]]));
        let mut e2 = ev(1, vec![]);
        e2.id = "id2".into();
        h.feed(e2);
        assert!(h.turns.is_empty(), "束ねは flush まで発火しない");
        assert_eq!(h.bundle.len(), 2);
        h.flush_bundle();
        assert_eq!(h.turns, vec!["bundle:id1,id2".to_string()]);
    }

    #[test]
    fn e2e_immediate_transfer_owner_reply() {
        let mut h = MockE2E::new();
        let owner_pk = "aa".repeat(32);
        h.owner.insert(follow_key(&owner_pk));
        h.callers.insert(owner_pk.clone(), CallerIdentity::Owner);
        let reply = ev(
            1,
            vec![
                vec!["e".into(), "note".into()],
                vec!["p".into(), SELF.into()],
            ],
        );
        h.feed(reply);
        assert_eq!(h.turns, vec!["immediate:リプライ:id1".to_string()]);
        assert_eq!(h.prepares, vec!["id1".to_string()]);
        assert_eq!(h.relays, vec!["id1".to_string()]);
        assert!(h.bundle.is_empty());
        h.flush_debounce();
        h.flush_bundle();
        assert_eq!(
            h.prepares,
            vec!["id1".to_string()],
            "即時は handle 時のみ prepare（flush で二重にしない）"
        );
        assert_eq!(h.relays, vec!["id1".to_string()]);
        assert!(AGREED_IMMEDIATE_KINDS.contains(&"リプライ"));
    }

    #[test]
    fn e2e_policy_owner_repost_debounces_on_empty_policy() {
        let mut h = MockE2E::new();
        let owner_pk = "aa".repeat(32);
        h.owner.insert(follow_key(&owner_pk));
        h.callers.insert(owner_pk.clone(), CallerIdentity::Owner);
        h.feed(ev(6, vec![]));
        assert!(h.turns.is_empty());
        assert_eq!(h.debounce.len(), 1);
        assert_eq!(h.debounce.intervals(), vec![60]);
        assert!(h.prepares.is_empty(), "束ね経路は handle で prepare しない");
        assert!(h.relays.is_empty());
        h.flush_debounce();
        assert_eq!(h.turns, vec!["debounce:リポスト:id1:60".to_string()]);
        assert_eq!(h.prepares, vec!["id1".to_string()]);
        assert_eq!(h.relays, vec!["id1".to_string()]);
    }

    #[test]
    fn e2e_policy_unallowed_is_dropped() {
        let mut h = MockE2E::new();
        h.feed(ev(7, vec![]));
        assert_eq!(h.dropped, vec!["allow:id1".to_string()]);
        assert!(h.turns.is_empty());
        assert!(h.prepares.is_empty());
    }

    #[test]
    fn e2e_policy_debounce_uses_class_interval_not_watch_interval() {
        let mut h = MockE2E::new();
        h.interval = 60;
        h.policy = serde_json::json!({
            "Owner": { "debounce_secs": 0, "immediate": ["リプライ"] },
            "CoAgent": { "debounce_secs": 0, "immediate": ["リプライ"] },
            "TrustedUser": { "debounce_secs": 120, "immediate": [] },
            "Agent": { "debounce_secs": 300, "immediate": [] },
        })
        .to_string();
        let author = "aa".repeat(32);
        h.allow_extra.insert(follow_key(&author));
        h.callers.insert(author, CallerIdentity::Agent);
        h.feed(ev(6, vec![]));
        assert!(h.turns.is_empty());
        assert_eq!(
            h.debounce.intervals(),
            vec![300],
            "core が返した権限毎間隔を捨てない"
        );
        assert_ne!(h.debounce.intervals(), vec![h.interval]);
        assert!(h.prepares.is_empty());
        h.flush_debounce();
        assert_eq!(h.turns, vec!["debounce:リポスト:id1:300".to_string()]);
        assert_eq!(h.prepares, vec!["id1".to_string()]);
        assert_eq!(h.relays, vec!["id1".to_string()]);
    }

    #[test]
    fn debounce_hold_flushes_each_privilege_interval() {
        let mut hold = DebounceHold::default();
        let now = tokio::time::Instant::now();
        let mut fast = ev(6, vec![]);
        fast.id = "fast".into();
        let mut slow = ev(6, vec![]);
        slow.id = "slow".into();
        hold.push_at(fast, 30, now);
        hold.push_at(slow, 300, now);
        assert_eq!(hold.intervals(), vec![30, 300]);
        let at_watch = hold.take_ready(now + Duration::from_secs(60));
        assert_eq!(at_watch.len(), 1);
        assert_eq!(at_watch[0].0, 30);
        assert_eq!(at_watch[0].1[0].id, "fast");
        assert_eq!(hold.intervals(), vec![300]);
        let later = hold.take_ready(now + Duration::from_secs(300));
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].0, 300);
        assert_eq!(later[0].1[0].id, "slow");
        assert!(hold.is_empty());
    }
}
