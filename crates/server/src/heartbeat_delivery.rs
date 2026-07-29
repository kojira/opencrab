//! ハートビート発話の transport 非依存な配送出口（段階3 PR-A / #246 の器）。
//!
//! ハートビートの `HeartbeatDecision::Speak(content)` は、これまで `discord_http` を
//! 直叩きして Discord 専用に発話していた。ここはその出口を **2 段構え**にする:
//!
//! 1. **登録簿（`state.gateways`）経由の非 Discord transport を先に試す。** 稼働中の
//!    transport が `text_delivery()` を提供していれば、そこへ `send_text` する。
//! 2. **どの非 Discord transport も配れなければ、既存の Discord 共有 http 経路を
//!    そのまま使う**（現行 `main.rs` の直叩きと同一の整形・宛先意味論・http）。
//!
//! ## なぜ Discord を登録簿走査から**あえて外す**か（移行段階の意図的な措置）
//!
//! ハートビート発話の現行 http ソースは**共有（config-based / TOML）Discord ゲートウェイ**
//! の `Http`（`main.rs` の `heartbeat_discord_http`）で、per-agent ゲートウェイが稼働して
//! いればそれで上書きされる。ところが**共有ゲートウェイは `state.gateways`（登録簿）に
//! 登録されていない**（登録されるのは per-agent の `DiscordGatewayManager` だけ）。
//! したがって登録簿の `gateway_actions_for()` から辿れる Discord の `Http` は per-agent
//! 分に限られ、共有ゲートウェイのみの構成では**発話が一切飛ばなくなる**＝挙動が変わる。
//!
//! これは #191 段階2 PR5 が `main.rs` に残した注記（「PR4 の capability が返すのは
//! `GatewayActions` であって生の HTTP ではなく、ここに当てると発話経路ごと書き換えに
//! なる＝挙動不変でなくなる。transport 中立化は heartbeat 側の課題として残す」）と同じ
//! 事実。そこで PR-A では **Discord は従来どおり legacy 共有 http 経路が担当**し、登録簿
//! 走査は非 Discord transport（PR-B 以降の Nostr など）の**差し込み口**としてだけ開ける。
//! これにより **Discord の挙動はバイト単位で不変**に保たれる。
//!
//! 「Discord も登録簿へ寄せて共有 http を無くす」統一は別 issue（フォローアップ）。

use opencrab_actions::{chunk_text, gateway_kinds, AgentGatewayRegistry};

/// ハートビート発話を配送する（段階3 PR-A / #246）。
///
/// 手順1（登録簿・非 Discord）で配れなければ手順2（Discord 共有 http・現行不変）へ落ちる。
/// 呼び出し側は本関数を `tokio::spawn` の中で `.await` し、発火 tick を塞がない
/// （fire-and-forget を維持。#178 系）。
///
/// 第 1 引数は `&AppState` ではなく `&AgentGatewayRegistry` を直接受ける（本関数が使うのは
/// `state.gateways` だけ。AppState 構築を避けて結線を単体テスト可能にするため / PR-C）。
pub(crate) async fn deliver_heartbeat_speech(
    gateways: &AgentGatewayRegistry,
    discord_http: &crate::DiscordHttpArc,
    agent_id: &str,
    channel_target: &str,
    content: &str,
) {
    // 手順1: 非 Discord の登録 transport を registry 経由で試す。
    if deliver_via_non_discord_registry(gateways, agent_id, channel_target, content).await {
        return;
    }
    // 手順2: 既存の Discord 共有 http 経路（現行 main.rs の直叩きと同一）。
    deliver_via_discord_shared_http(discord_http, agent_id, channel_target, content).await;
}

/// 稼働中の**非 Discord** transport へ登録簿経由で 1 通配る。配れたら（＝ある transport が
/// 担当したら）`true`。
///
/// **Discord 種別は意図的にスキップ**する（理由はモジュール doc）。現状 Nostr など他
/// transport は capability（`gateway_actions_for` / `text_delivery`）未実装なので、この
/// 走査は誰にも当たらず常に `false` を返す＝挙動は現行と不変。PR-B で Nostr が
/// `text_delivery()` を提供すればここに乗る。
async fn deliver_via_non_discord_registry(
    gateways: &AgentGatewayRegistry,
    agent_id: &str,
    target: &str,
    content: &str,
) -> bool {
    for kind in gateways.kinds() {
        // Discord は legacy 共有 http 経路（手順2）が担当する。ここで拾うと共有
        // ゲートウェイの http に到達できず挙動が変わる（モジュール doc / #191 段階2 PR5）。
        if kind == gateway_kinds::DISCORD {
            continue;
        }
        if !gateways.is_running(kind, agent_id) {
            continue;
        }
        let Some(gateway) = gateways.get(kind) else {
            continue;
        };
        let Some(actions) = gateway.gateway_actions_for(agent_id) else {
            continue;
        };
        let Some(delivery) = actions.text_delivery() else {
            continue;
        };
        // ある transport が引き受けた時点で**それに委ねる**。送信に失敗しても他 transport
        // や Discord へ流し直さない（別チャンネルへの二重発話を避ける）。
        //
        // transport の助言 `chunk_limit()` で content を分割してから 1 チャンクずつ送る。
        // 長文の Nostr 発話がリレーの 1 イベント上限を超えて publish ごと失敗するのを防ぐ
        // （peer_review が `build_part_messages` でやっているのと同じ発想。ただし heartbeat
        // 発話は `part X/N` framing を付けない生分割）。1 チャンクでも失敗したらそこで打ち
        // 切る（後続チャンクを送っても文脈が壊れるだけで、Discord へ流し直しもしない）。
        let limit = delivery.chunk_limit();
        let chunks = chunk_text(content, limit);
        for (i, chunk) in chunks.iter().enumerate() {
            match delivery.send_text(target, chunk).await {
                Ok(()) => {
                    tracing::info!(
                        agent_id = %agent_id,
                        kind,
                        target,
                        part = i + 1,
                        parts = chunks.len(),
                        "Heartbeat spoke via non-Discord transport"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        agent_id = %agent_id,
                        kind,
                        target,
                        part = i + 1,
                        parts = chunks.len(),
                        "Heartbeat send via non-Discord transport failed: {e}"
                    );
                    break;
                }
            }
        }
        return true;
    }
    false
}

/// 既存の Discord 共有 http 経路。**現行 `main.rs` の `HeartbeatDecision::Speak` 直叩きを
/// そのまま移設**したもの（整形・宛先意味論・http・ログ文言・fail-safe すべて不変）。
async fn deliver_via_discord_shared_http(
    discord_http: &crate::DiscordHttpArc,
    agent_id: &str,
    channel_target: &str,
    content: &str,
) {
    let channel_id_u64: Option<u64> = channel_target.parse().ok();
    let http_opt = discord_http.lock().unwrap().clone();
    if let (Some(_http), Some(_ch_id)) = (http_opt.clone(), channel_id_u64) {
        #[cfg(feature = "discord")]
        {
            use serenity::builder::CreateMessage;
            use serenity::model::id::ChannelId;
            let ch = ChannelId::new(_ch_id);
            if let Err(e) = ch
                .send_message(&_http, CreateMessage::new().content(content))
                .await
            {
                tracing::error!(agent_id = %agent_id, channel_id = %channel_target, "Heartbeat send_speech failed: {e}");
            } else {
                tracing::info!(agent_id = %agent_id, channel_id = %channel_target, "Heartbeat spoke: {}", content);
            }
        }
        #[cfg(not(feature = "discord"))]
        {
            tracing::info!(agent_id = %agent_id, channel_id = %channel_target, "Heartbeat Speak (discord disabled): {}", content);
        }
    } else {
        tracing::debug!(
            agent_id = %agent_id,
            "Heartbeat Speak: no Discord http or invalid channel_id"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    use opencrab_actions::AgentGatewayLifecycle;
    use opencrab_core::text_delivery::TextDelivery;
    use opencrab_gateway::{
        GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
    };

    /// 記録された送信（target, text）の共有ログ。
    type CallLog = Arc<Mutex<Vec<(String, String)>>>;

    /// 送信を記録するだけの配送口（ネットワークに出ない）。`chunk_limit` は注入可能。
    struct SpyDelivery {
        calls: CallLog,
        chunk_limit: usize,
    }

    #[async_trait]
    impl TextDelivery for SpyDelivery {
        fn validate_target(&self, _target: &str) -> Result<(), String> {
            Ok(())
        }
        fn mention(&self, user_id: &str) -> String {
            format!("@{user_id}")
        }
        fn chunk_limit(&self) -> usize {
            self.chunk_limit
        }
        async fn send_text(&self, target: &str, text: &str) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push((target.to_string(), text.to_string()));
            Ok(())
        }
    }

    /// transport が返すツール実行の実体。`text_delivery()` だけ意味を持つ。
    struct FakeActions {
        delivery: Option<Arc<dyn TextDelivery>>,
    }

    #[async_trait]
    impl GatewayActions for FakeActions {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                error: Some("unused in tests".to_string()),
            }
        }
        fn text_delivery(&self) -> Option<Arc<dyn TextDelivery>> {
            self.delivery.clone()
        }
    }

    /// ネットワークに出ない偽マネージャ。稼働 agent と配送口を注入する。
    struct FakeGateway {
        kind: &'static str,
        running: Vec<String>,
        delivery: Option<Arc<dyn TextDelivery>>,
    }

    #[async_trait]
    impl AgentGatewayLifecycle for FakeGateway {
        fn kind(&self) -> &'static str {
            self.kind
        }
        async fn start(&self, _agent_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self, _agent_id: &str) {}
        fn is_running(&self, agent_id: &str) -> bool {
            self.running.iter().any(|a| a == agent_id)
        }
        async fn restore_all(&self) {}
        async fn shutdown_all(&self) {}
        fn gateway_actions_for(&self, _agent_id: &str) -> Option<Arc<dyn GatewayActions>> {
            Some(Arc::new(FakeActions {
                delivery: self.delivery.clone(),
            }))
        }
    }

    fn spy() -> (CallLog, Arc<dyn TextDelivery>) {
        spy_with_limit(2000)
    }

    fn spy_with_limit(chunk_limit: usize) -> (CallLog, Arc<dyn TextDelivery>) {
        let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
        let delivery: Arc<dyn TextDelivery> = Arc::new(SpyDelivery {
            calls: calls.clone(),
            chunk_limit,
        });
        (calls, delivery)
    }

    /// 稼働中の非 Discord transport（配送口あり）へ、正しい target/content で 1 回配る。
    #[tokio::test]
    async fn delivers_to_a_running_non_discord_transport_once() {
        let (calls, delivery) = spy();
        let registry = AgentGatewayRegistry::new();
        registry.register(Arc::new(FakeGateway {
            kind: gateway_kinds::NOSTR,
            running: vec!["crab".to_string()],
            delivery: Some(delivery),
        }));

        let handled =
            deliver_via_non_discord_registry(&registry, "crab", "note-target", "こんにちは").await;

        assert!(handled, "稼働中の非 Discord transport が引き受ける");
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("note-target".to_string(), "こんにちは".to_string())],
            "target と content がそのまま 1 回だけ渡る"
        );
    }

    /// **Discord 種別は稼働中でも登録簿走査からスキップ**（＝手順2 の共有 http へ落ちる）。
    /// これが「Discord をバイト不変に保つ」ための核心。
    #[tokio::test]
    async fn skips_discord_kind_even_when_running() {
        let (calls, delivery) = spy();
        let registry = AgentGatewayRegistry::new();
        registry.register(Arc::new(FakeGateway {
            kind: gateway_kinds::DISCORD,
            running: vec!["crab".to_string()],
            delivery: Some(delivery),
        }));

        let handled = deliver_via_non_discord_registry(&registry, "crab", "123456789", "hi").await;

        assert!(
            !handled,
            "Discord は登録簿走査では拾わない（共有 http 経路が担当）"
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "Discord の配送口は登録簿経由では呼ばれない"
        );
    }

    /// 非 Discord transport が居ない/稼働していない → 手順2 へフォールバックする（false）。
    #[tokio::test]
    async fn falls_through_when_no_non_discord_transport_delivers() {
        let registry = AgentGatewayRegistry::new();
        // 稼働していない Nostr（別 agent のみ稼働）は拾わない。
        registry.register(Arc::new(FakeGateway {
            kind: gateway_kinds::NOSTR,
            running: vec!["other".to_string()],
            delivery: Some(spy().1),
        }));

        assert!(
            !deliver_via_non_discord_registry(&registry, "crab", "t", "c").await,
            "稼働していなければ手順2 へ落ちる"
        );
        // 空の登録簿でも安全に false。
        let empty = AgentGatewayRegistry::new();
        assert!(!deliver_via_non_discord_registry(&empty, "crab", "t", "c").await);
    }

    /// 稼働中でも `text_delivery()` が無い transport は拾わない（手順2 へ落ちる）。
    #[tokio::test]
    async fn ignores_running_transport_without_a_delivery() {
        let registry = AgentGatewayRegistry::new();
        registry.register(Arc::new(FakeGateway {
            kind: gateway_kinds::NOSTR,
            running: vec!["crab".to_string()],
            delivery: None,
        }));
        assert!(!deliver_via_non_discord_registry(&registry, "crab", "t", "c").await);
    }

    /// Discord 共有 http フォールバックは http 無し/不正 channel でもログのみ・panic しない
    /// （現行 else 分岐と同じ fail-safe）。
    #[tokio::test]
    async fn discord_fallback_is_fail_safe_without_http() {
        // http 無し。
        let none_http: crate::DiscordHttpArc = Arc::new(std::sync::Mutex::new(None));
        deliver_via_discord_shared_http(&none_http, "crab", "123456789", "hi").await;
        // 不正な channel（数値でない）。
        deliver_via_discord_shared_http(&none_http, "crab", "not-a-number", "hi").await;
    }

    /// (e) 長文は transport の `chunk_limit()` で分割され、複数回 `send_text` される。
    /// 各チャンクは上限以下で、連結すると元の content に戻る（無損失分割）。
    #[tokio::test]
    async fn long_content_is_split_by_chunk_limit_into_multiple_sends() {
        let (calls, delivery) = spy_with_limit(5);
        let registry = AgentGatewayRegistry::new();
        registry.register(Arc::new(FakeGateway {
            kind: gateway_kinds::NOSTR,
            running: vec!["crab".to_string()],
            delivery: Some(delivery),
        }));

        // 13 文字（マルチバイト混在）→ 上限 5 で 3 チャンク（5 + 5 + 3）。
        let content = "あいうえおかきくけこさしす";
        let handled =
            deliver_via_non_discord_registry(&registry, "crab", "note-target", content).await;
        assert!(handled);

        let sent = calls.lock().unwrap().clone();
        assert_eq!(sent.len(), 3, "上限 5・13 文字 = 3 チャンク送信");
        for (target, chunk) in &sent {
            assert_eq!(target, "note-target", "全チャンク同じ宛先");
            assert!(
                chunk.chars().count() <= 5,
                "各チャンクは chunk_limit 以下: {chunk:?}"
            );
        }
        let joined: String = sent.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(joined, content, "分割は無損失（連結で元に戻る）");
    }

    /// (f) 結線: `deliver_heartbeat_speech` は手順1（非 Discord registry）が引き受けたら
    /// **手順2（Discord 共有 http）へ進まない**。稼働中の Nostr spy が content を受け取り、
    /// 早期 return するので Discord へ流れない（別チャンネル二重発話を避ける核心）。
    #[tokio::test]
    async fn deliver_heartbeat_speech_stops_after_step1_handles_it() {
        let (calls, delivery) = spy();
        let registry = AgentGatewayRegistry::new();
        registry.register(Arc::new(FakeGateway {
            kind: gateway_kinds::NOSTR,
            running: vec!["crab".to_string()],
            delivery: Some(delivery),
        }));
        // http は None。もし手順2 へ流れても panic はしないが、ここでは手順1 が
        // 引き受けるので Discord には一切触れない。
        let none_http: crate::DiscordHttpArc = Arc::new(std::sync::Mutex::new(None));

        deliver_heartbeat_speech(&registry, &none_http, "crab", "note-target", "自律発話").await;

        assert_eq!(
            *calls.lock().unwrap(),
            vec![("note-target".to_string(), "自律発話".to_string())],
            "手順1 が content を配送し、手順2 へは進まない"
        );
    }

    /// (f) 逆側: 稼働中の非 Discord transport が居なければ手順2（Discord 共有 http）へ落ちる。
    /// http 無しでも panic せず、registry 側 spy には何も渡らない。
    #[tokio::test]
    async fn deliver_heartbeat_speech_falls_through_to_discord_when_step1_declines() {
        let (calls, _delivery) = spy();
        // 空 registry（手順1 は誰も引き受けない）。
        let registry = AgentGatewayRegistry::new();
        let none_http: crate::DiscordHttpArc = Arc::new(std::sync::Mutex::new(None));

        deliver_heartbeat_speech(&registry, &none_http, "crab", "123456789", "hi").await;

        assert!(
            calls.lock().unwrap().is_empty(),
            "手順1 が居なければ registry spy には渡らない（手順2 の Discord 経路へ）"
        );
    }
}
