//! web の時刻発火（#628 / #627）: 発火先 descriptor と受け口（sink）。
//!
//! これが「transport を 1 つ足す = そのモジュールだけ」の実証（#628 段階7）。web を発火先に
//! するために触るのはこの crate だけで、db も heartbeat_fire も scheduler も触らない
//! （descriptor が自分の性質と ID 書式を名乗り、中核は登録簿へ問い合わせる）。
//!
//! # web には常駐ループが無い（設計前提が web だけ崩れる点）
//!
//! Discord sink は `LoopEvent` を、Nostr sink は `ResponseJob` を既存ループへ 1 本流すだけだが、
//! web には受信ループが無い。したがって [`WebTimedFireSink`] は**自分で駆動する**:
//!
//! - **必ず `tokio::spawn`**（同期呼び出しにすると `fire_timed_turn` が scheduler を塞ぐ。
//!   `TimedFireSink` の「非ブロック」契約違反になる）。
//! - **必ず [`run_and_deliver_serialized`] 経由**（直呼びすると per-session 直列化が外れ、
//!   ハートビートが live inbound と競合して二重回答になる）。[`crate::sink`] と同じく、生の
//!   応答生成 `run_and_deliver` へは兄弟モジュールの可視性規則で到達できない（#177）。
//! - **`req.prompt` を `system_prompt_suffix` として渡す**（discord / nostr と同じ扱い。会話ログに
//!   「発言」として残さない）。

use opencrab_actions::{FireTarget, TransportFire, TransportFireEnv};

use crate::gateway::WEB_SESSION_PREFIX;
use crate::respond::run_and_deliver_serialized;
use crate::runner::WebAgentRunner;

/// web の時刻発火 kind（sink 登録簿のキー・[`FireTarget::kind`]）。
///
/// discord / nostr の [`opencrab_actions::gateway_kinds`] と同じ役割だが、web は per-agent
/// ゲートウェイ登録簿（`AgentGatewayRegistry`）に載らないので kind はこの crate が持つ
/// （descriptor と sink 登録の両方がこの 1 つの定数を使い、文字列の食い違いを防ぐ）。
pub const WEB_TIMED_FIRE_KIND: &str = "web";

/// SSE へ流す web ハートビート応答の `kind`（ダッシュボードが読むイベント種別）。
const WEB_HEARTBEAT_EVENT_KIND: &str = "heartbeat";

/// `web-{agent}-{conversation}` の発火先を名乗る descriptor（#627 / #628）。
///
/// **性質**: live G マスタゲートの対象外（`is_g_gated=false`・Nostr と同じ）／発火ターンの
/// 応答本文はそのままダッシュボードへ配送される（`posts_response_body=true`）。web は外部接続を
/// 持たず常に立ち上がるので [`should_be_running`](TransportFire::should_be_running) は常に true
/// （隔離環境——Discord・Nostr 無効——でもハートビートの E2E が回せる要）。
pub struct WebFire;

impl TransportFire for WebFire {
    fn kind(&self) -> &'static str {
        WEB_TIMED_FIRE_KIND
    }

    /// `web-{agent}-{conversation}` を保存済み `agent_id` で剥がして発火先を導く。
    ///
    /// `agent_id`（UUID・ハイフン入り）で接頭辞を剥がし、残り（conversation_id）が**非空**なら
    /// 自分の発火先。conversation_id はハイフンを含みうるので、残り全体を `route` に入れる
    /// （discord のような数値検査はしない）。空（`web-{agent}` だけ）は `None`（fail-closed）。
    fn parse(&self, session_id: &str, agent_id: &str) -> Option<FireTarget> {
        let prefix = format!("{WEB_SESSION_PREFIX}{agent_id}-");
        let conversation = session_id.strip_prefix(&prefix)?;
        if conversation.is_empty() {
            return None;
        }
        Some(FireTarget {
            kind: WEB_TIMED_FIRE_KIND,
            // request の channel/guild token は空（web は session_id だけで宛先が決まる。
            // `TimedFireRequest` の shape は変えない・web は channel_id="" / guild_id=""）。
            channel_id: String::new(),
            guild_id: String::new(),
            // build 用の経路トークン（conversation_id）。request には載らない。
            route: conversation.to_string(),
        })
    }

    /// [`parse`](Self::parse) の逆写像。`web-{agent}-{conversation}` を組む
    /// （[`crate::gateway::web_session_id`] と同じ書式）。
    fn build_session_id(&self, target: &FireTarget, agent_id: &str) -> String {
        format!("{WEB_SESSION_PREFIX}{agent_id}-{}", target.route)
    }

    fn is_g_gated(&self) -> bool {
        false
    }

    fn posts_response_body(&self) -> bool {
        true
    }

    fn human_hint(&self) -> &'static str {
        "ダッシュボードの会話"
    }

    /// web は外部接続を持たず常に立ち上がる（隔離環境でも受け口が在る）。実行時述語だが
    /// env は引かない（web の「立ち上がるべきか」は環境に依存しない）。
    fn should_be_running(&self, _env: &TransportFireEnv) -> bool {
        true
    }

    fn sample_target(&self) -> FireTarget {
        FireTarget {
            kind: WEB_TIMED_FIRE_KIND,
            channel_id: String::new(),
            guild_id: String::new(),
            route: "conv-1".to_string(),
        }
    }
}

/// scheduler の時刻発火（#588 TimedFire）を web セッションで駆動する受け口。
///
/// web には常駐ループが無いので、自分で `tokio::spawn` して [`run_and_deliver_serialized`] を
/// 回す（モジュール doc 参照）。以降は inbound / subtask resume と同じ per-session 直列化・SSE
/// 配送・DB 記録を通り、発火後の一連（宣言・ツール呼び出し・サブタスク・継続ターン）が
/// ダッシュボードで 1 つのセッションとして追える（#627）。
pub struct WebTimedFireSink<R: WebAgentRunner> {
    runner: R,
}

impl<R: WebAgentRunner> WebTimedFireSink<R> {
    /// runner（`AppState`）はプロセス全体で 1 つの共有ランタイムへ到達する必要がある
    /// （inbound / resume と同じ SSE チャンネル・直列化ロックを引くため）。
    pub fn new(runner: R) -> Self {
        Self { runner }
    }
}

impl<R: WebAgentRunner> opencrab_actions::TimedFireSink for WebTimedFireSink<R> {
    fn fire_timed_turn(&self, req: opencrab_actions::TimedFireRequest) {
        let runner = self.runner.clone();
        // 非ブロック（scheduler を塞がない）: web はループが無いのでここで自分で spawn する。
        tokio::spawn(async move {
            // 必ず直列化込みの公開入口を通る（直呼び＝二重回答）。prompt は system プロンプトへ
            // 足す（会話ログに「発言」として残さない・discord / nostr と同じ）。
            run_and_deliver_serialized(
                &runner,
                &req.agent_id,
                &req.session_id,
                req.caller,
                Some(&req.prompt),
                WEB_HEARTBEAT_EVENT_KIND,
            )
            .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::gateway::web_session_id;
    use crate::testing::FakeRunner;
    use opencrab_actions::{CallerIdentity, TimedFireRequest, TimedFireSink};

    const AGENT_UUID: &str = "6b79ac3a-7f17-4618-a827-5bda992a3698";

    /// parse / build の round-trip（conversation は保存済み agent_id で剥がす）。
    #[test]
    fn parse_build_round_trip() {
        let sample = WebFire.sample_target();
        let sid = WebFire.build_session_id(&sample, AGENT_UUID);
        assert_eq!(sid, format!("web-{AGENT_UUID}-conv-1"));
        assert_eq!(WebFire.parse(&sid, AGENT_UUID), Some(sample));
        // web_session_id と同じ書式であること（配送経路の session_id と一致する要）。
        assert_eq!(sid, web_session_id(AGENT_UUID, "conv-1"));
    }

    /// conversation が空（`web-{agent}` だけ）・別 agent・別種別は None（fail-closed）。
    #[test]
    fn parse_fail_closed() {
        assert!(WebFire.parse(&format!("web-{AGENT_UUID}"), AGENT_UUID).is_none());
        assert!(WebFire
            .parse(&format!("web-{AGENT_UUID}-conv-1"), "other")
            .is_none());
        assert!(WebFire
            .parse(&format!("discord-{AGENT_UUID}-1-2"), AGENT_UUID)
            .is_none());
        assert!(WebFire
            .parse(&format!("nostr-{AGENT_UUID}"), AGENT_UUID)
            .is_none());
    }

    /// web の性質: G ゲート対象外・応答本文は自動配送・常に立ち上がる。
    #[test]
    fn web_properties() {
        assert!(!WebFire.is_g_gated());
        assert!(WebFire.posts_response_body());
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let configured = std::collections::HashSet::new();
        let env = TransportFireEnv {
            conn: &conn,
            configured_shared_kinds: &configured,
        };
        assert!(WebFire.should_be_running(&env));
    }

    /// 受け口: 発火要求を per-session 直列化込みで駆動し、prompt を system プロンプトへ足して
    /// 応答を SSE へ配送する（#627: 発火後の結果がダッシュボードで追える）。
    #[tokio::test]
    async fn sink_drives_serialized_turn_and_publishes() {
        let runner = FakeRunner::new("巡回しました");
        let sid = web_session_id(AGENT_UUID, "conv-1");
        let mut rx = runner.web_gateway().subscribe(&sid);

        WebTimedFireSink::new(runner.clone()).fire_timed_turn(TimedFireRequest {
            session_id: sid.clone(),
            agent_id: AGENT_UUID.to_string(),
            channel_id: String::new(),
            guild_id: String::new(),
            prompt: "[ハートビート] いまはハートビートの時間です".to_string(),
            caller: CallerIdentity::Owner,
        });

        // spawn した発火ターンが SSE へ配送する（heartbeat kind）。
        let payload = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("発火ターンの応答が SSE へ配送されない")
            .unwrap();
        assert!(payload.contains("\"kind\":\"heartbeat\""));
        assert!(payload.contains("巡回しました"));

        // run が走り、prompt が system プロンプトへ足されている（会話の発言にしない）。
        for _ in 0..100 {
            if !runner.runs().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let runs = runner.runs();
        assert_eq!(runs.len(), 1, "発火ターンが 1 回走る");
        assert_eq!(runs[0].session_id, sid);
        assert!(runs[0].system_prompt.contains("いまはハートビートの時間です"));
        assert_eq!(runs[0].caller, CallerIdentity::Owner);
    }
}
