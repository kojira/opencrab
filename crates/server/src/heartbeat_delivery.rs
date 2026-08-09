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
//! ハートビート発話の http ソースには**共有（config-based / TOML）Discord ゲートウェイ**
//! の `Http` が含まれる（per-agent ゲートウェイがあればそちらが優先される。下の #400 の
//! 節を参照）。ところが**共有ゲートウェイは `state.gateways`（登録簿）に
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
//!
//! ## 手順2 のハンドルは**発話する体ごと**に解決する（#400）
//!
//! 以前は `main.rs` が起動時に `gateway.discord.agent_ids` の**先頭 1 体**の per-agent
//! ゲートウェイからハンドルを 1 本取り、全エージェントのハートビートでそれを共有して
//! いた。`Http` はボットのトークンを保持するので**送信者名はハンドルが決める**。結果、
//! (1) Discord へ発話できるのは先頭の体だけ、(2) 先頭以外の体が Discord チャンネルへ
//! 向いていればその発話は先頭の体の名前で投稿される、という並び順依存があった。
//!
//! [`HeartbeatDiscordHttp`] はこれを配送時の解決に置き換える:
//!
//! 1. **発話する体自身の per-agent ゲートウェイ**のハンドル（あればこれが正しい名前）
//! 2. 無ければ**共有（TOML）ゲートウェイ**のハンドル（従来のフォールバック）
//!
//! 共有ゲートウェイを 2 番目に残すのが上の制約への回答である。共有ゲートウェイは登録簿に
//! 載らないため、ここを「per-agent が無ければ諦める」にすると共有ゲートウェイのみの構成で
//! 発話が飛ばなくなる（＝登録簿へ寄せたのと同じ後退になる）。逆に per-agent を先に見る
//! ことで、自分のボットを持つ体は**自分の名前で**出る。手順1（登録簿・非 Discord）と
//! 手順2 の順序、および fire-and-forget は変えていない。

use opencrab_actions::{chunk_text, gateway_kinds, AgentGatewayRegistry};
use std::sync::{Arc, Mutex};

/// per-agent Discord ゲートウェイからハンドルを引く口（#400）。
///
/// 具象の `DiscordGatewayManager` をここで名指しせずに済ませるための最小の窓。実装は
/// 下の `#[cfg(feature = "discord")]` ブロックにあり、テストはネットワークに出ない偽実装を
/// 差し込む。
pub(crate) trait PerAgentDiscordHttp: Send + Sync {
    /// その体専用のゲートウェイのハンドル。稼働していなければ `None`。
    fn http_for_agent(&self, agent_id: &str) -> Option<crate::DiscordHttp>;
}

/// per-agent Discord ゲートウェイからハンドルを引く（#400）。
///
/// 生存（`is_running`）では絞らない。`Http` は REST クライアントであり、受信ループの
/// 生死とは独立に送信できるため、**エントリは残っているが受信ループの handle だけ
/// finished** のケースでは絞らない方がその体自身の名前で出せる。
///
/// **ゲートウェイの停止・再起動中の窓は救えない。** `get_http_for_agent` は
/// `DiscordGatewayManager` のマップ引きに過ぎず、`stop_agent_gateway`（および
/// `start_agent_gateway` が先頭で呼ぶそれ）はエントリごと remove するので、その間は
/// `None` になり共有ゲートウェイのハンドルへ落ちる＝共有ボットの名前で出る。これは
/// `is_running` で絞るかどうかと無関係に起きる。塞ぐならフォールバック条件を「いま
/// ハンドルが引けるか」ではなく「その体が per-agent Discord 設定を持つか」に寄せる
/// 形になるが、配送経路に新しい DB 参照を足すことになる。窓は起動/再起動時に限られ、
/// **変更前（先頭 1 体のハンドルを全体で共有）より悪くはならない**ので本 PR では取らない。
#[cfg(feature = "discord")]
impl<T: opencrab_discord::AgentRunner + Send + Sync> PerAgentDiscordHttp
    for opencrab_discord::DiscordGatewayManager<T>
{
    fn http_for_agent(&self, agent_id: &str) -> Option<crate::DiscordHttp> {
        self.get_http_for_agent(agent_id)
    }
}

/// 解決に使ったハンドルの出所。ログとテストの観測点（#400）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeartbeatHttpSource {
    /// 発話する体自身の per-agent ゲートウェイ（Discord 上もその体の名前で出る）。
    PerAgent,
    /// 共有（TOML）ゲートウェイ。送信者名は共有ボットのもの。
    Shared,
    /// どちらも無い（Discord へは出せない）。
    None,
}

impl HeartbeatHttpSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::PerAgent => "per_agent",
            Self::Shared => "shared",
            Self::None => "none",
        }
    }
}

/// ハートビート発話の Discord ハンドル解決口（#400）。
///
/// 起動時に 1 本スナップショットするのではなく**配送のたびに体ごとに**引く。ダッシュボード
/// から per-agent ゲートウェイを起動/停止しても解決結果が古びない。
pub(crate) struct HeartbeatDiscordHttp {
    /// 共有（TOML）ゲートウェイの http。登録簿に載らないのでここで直接持つ（モジュール doc）。
    shared: crate::DiscordHttpArc,
    /// per-agent ゲートウェイの引き口。Discord feature 無効時や配線前は `None`。
    per_agent: Mutex<Option<Arc<dyn PerAgentDiscordHttp>>>,
}

impl HeartbeatDiscordHttp {
    pub(crate) fn new(shared: crate::DiscordHttpArc) -> Self {
        Self {
            shared,
            per_agent: Mutex::new(None),
        }
    }

    /// per-agent ゲートウェイの引き口を配線する（起動時に 1 回）。
    ///
    /// Discord feature 無効ビルドでは per-agent ゲートウェイ自体が存在せず、`main.rs` に
    /// 呼び出し側が無い（テストからは呼ぶ）。
    #[cfg_attr(not(feature = "discord"), allow(dead_code))]
    pub(crate) fn set_per_agent_source(&self, source: Arc<dyn PerAgentDiscordHttp>) {
        *self.per_agent.lock().unwrap() = Some(source);
    }

    /// `agent_id` の体が Discord へ出るときのハンドルと、その出所を返す。
    pub(crate) fn resolve(
        &self,
        agent_id: &str,
    ) -> (Option<crate::DiscordHttp>, HeartbeatHttpSource) {
        // Arc を取り出してからロックを落とす（引き口の内部ロックとの入れ子を作らない）。
        let per_agent = self.per_agent.lock().unwrap().clone();
        if let Some(source) = per_agent {
            if let Some(http) = source.http_for_agent(agent_id) {
                return (Some(http), HeartbeatHttpSource::PerAgent);
            }
        }
        match self.shared.lock().unwrap().clone() {
            Some(http) => (Some(http), HeartbeatHttpSource::Shared),
            None => (None, HeartbeatHttpSource::None),
        }
    }
}

/// 起動時診断の判定（純粋関数 / #400）。
///
/// 以前はハンドルが取れないと `if let Some` が外れるだけで**何も出なかった**ため、
/// 「Discord へ発話できない構成のまま動いている」ことに気づけなかった
/// （`deliver_via_discord_shared_http` の WARN は発火後の取りこぼしを指すもので、
/// 起動時のハンドル未解決そのものは可視化されていない）。
///
/// 戻り値は (レベル, 理由)。レベルの判定だけを純粋関数に切り出し、ログ出力は
/// [`log_startup_http_resolution`] の薄い match が行う。
fn startup_http_diagnosis(source: HeartbeatHttpSource) -> (tracing::Level, &'static str) {
    match source {
        HeartbeatHttpSource::PerAgent => (
            tracing::Level::INFO,
            "この体専用の Discord ゲートウェイのハンドルで発話する（Discord 上もこの体の名前）",
        ),
        HeartbeatHttpSource::Shared => (
            tracing::Level::INFO,
            "この体専用の Discord ゲートウェイが無いため共有（TOML）ゲートウェイのハンドルで発話する（Discord 上は共有ボットの名前になる）",
        ),
        HeartbeatHttpSource::None => (
            tracing::Level::WARN,
            "Discord ハンドルを解決できない（この体専用のゲートウェイが未稼働、かつ共有（TOML）ゲートウェイも未起動）。この体のハートビート発話は Discord へは出ない（他 transport が稼働していればそちらへ出る）",
        ),
    }
}

/// 起動時に、ハートビートを回す体ごとにハンドルの解決可否を 1 行残す（#400）。
///
/// Discord feature 無効ビルドでは呼び出し側（`main.rs` の起動時診断）ごと落ちる
/// （ハンドルが存在しようがない構成で WARN を並べても雑音にしかならない）。
#[cfg_attr(not(feature = "discord"), allow(dead_code))]
pub(crate) fn log_startup_http_resolution(http: &HeartbeatDiscordHttp, agent_id: &str) {
    let (_, source) = http.resolve(agent_id);
    let (level, reason) = startup_http_diagnosis(source);
    if level == tracing::Level::WARN {
        tracing::warn!(agent_id = %agent_id, http_source = source.as_str(), "Heartbeat Discord ハンドル: {reason}");
    } else {
        tracing::info!(agent_id = %agent_id, http_source = source.as_str(), "Heartbeat Discord ハンドル: {reason}");
    }
}

/// ハートビート発話を配送する（段階3 PR-A / #246）。
///
/// 手順1（登録簿・非 Discord）で配れなければ手順2（Discord 共有 http・現行不変）へ落ちる。
/// 呼び出し側は本関数を `tokio::spawn` の中で `.await` し、発火 tick を塞がない
/// （fire-and-forget を維持。#178 系）。
///
/// 第 1 引数は `&AppState` ではなく `&AgentGatewayRegistry` を直接受ける（本関数が使うのは
/// `state.gateways` だけ。AppState 構築を避けて結線を単体テスト可能にするため / PR-C）。
///
/// **戻り値: Discord へ実際に配信できたか（#425）。** `true` のときだけ呼び出し側が、その
/// 発話を本人の `discord-…` 会話セッションへ二重記録する（本人が後続の通常返信ターンで
/// 自分の HB 投稿を思い出せるようにする）。手順1 の非 Discord transport が担当した場合は
/// `discord-…` セッションが対象外なので `false`、Discord 配信に失敗／配信先が無い場合も
/// `false`（言っていないことを記憶に残さない）。
pub(crate) async fn deliver_heartbeat_speech(
    gateways: &AgentGatewayRegistry,
    discord_http: &HeartbeatDiscordHttp,
    agent_id: &str,
    channel_target: &str,
    content: &str,
) -> bool {
    // 手順1: 非 Discord の登録 transport を registry 経由で試す。
    if deliver_via_non_discord_registry(gateways, agent_id, channel_target, content).await {
        // 非 Discord へ配れた。`discord-…` 会話セッションは記録先ではないので false。
        return false;
    }
    // 手順2: 既存の Discord 共有 http 経路（現行 main.rs の直叩きと同一）。
    deliver_via_discord_shared_http(discord_http, agent_id, channel_target, content).await
}

/// 稼働中の**非 Discord** transport へ登録簿経由で 1 通配る。配れたら（＝ある transport が
/// 担当したら）`true`。
///
/// **Discord 種別は意図的にスキップ**する（理由はモジュール doc）。
///
/// Nostr は既に `text_delivery()` を提供済み（`nostr/src/actions.rs` の
/// `NostrGatewayActions::text_delivery` → `nostr/src/text_delivery.rs` の
/// `NostrTextDelivery`）。そのため Nostr ゲートウェイがその体で稼働していれば、この走査は
/// Nostr に**当たって publish する**（≠常に `false`）。ただし `NostrTextDelivery::send_text`
/// は **`target` を無視して `post_note` する**（kind:1 broadcast＝エージェント設定のリレー
/// 集合へ自発投稿）。したがって `channel_target` は Nostr 経路では効かず、宛先を絞る意味は
/// 持たない。
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

/// Discord 経路（整形・宛先意味論・fail-safe は移設時から不変）。
///
/// **どのハンドルで送るかだけが #400 で変わった**: 全体で共有する 1 本ではなく、
/// [`HeartbeatDiscordHttp::resolve`] が `agent_id` ごとに（per-agent → 共有の順で）引く。
///
/// **戻り値: Discord へ 1 通送れたか（#425）。** 送信 `Ok` のときだけ `true`。送信失敗・
/// 配信先が無い（ハンドル未解決 / channel_target が無効）・Discord feature 無効ビルドは
/// いずれも `false`（実際には Discord へ出ていないので、二重記録の対象にしない）。
async fn deliver_via_discord_shared_http(
    discord_http: &HeartbeatDiscordHttp,
    agent_id: &str,
    channel_target: &str,
    content: &str,
) -> bool {
    let channel_id_u64: Option<u64> = channel_target.parse().ok();
    let (http_opt, http_source) = discord_http.resolve(agent_id);
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
                tracing::error!(agent_id = %agent_id, channel_id = %channel_target, http_source = http_source.as_str(), "Heartbeat send_speech failed: {e}");
                false
            } else {
                tracing::info!(agent_id = %agent_id, channel_id = %channel_target, http_source = http_source.as_str(), "Heartbeat spoke: {}", content);
                true
            }
        }
        #[cfg(not(feature = "discord"))]
        {
            tracing::info!(agent_id = %agent_id, channel_id = %channel_target, http_source = http_source.as_str(), "Heartbeat Speak (discord disabled): {}", content);
            false
        }
    } else {
        // 手順1（非 Discord registry）も手順2（Discord 共有 http）も配れなかった＝
        // 発火したのに発話先が無い。特に AgentScoped で opt-in 済みだが Nostr 等が未稼働／
        // Discord channel も未設定（channel_target が空/無効）のときに起きる。沈黙で発話を
        // 見失わないよう WARN で可視化する。Nostr 等で正常に配れた場合は手順1 で早期 return
        // するのでここへは来ず、Discord 送信成功時も上の分岐で info を出すのでここへは来ない
        // （＝この WARN は「取りこぼし」時のみ出る）。
        //
        // #400: 「ハンドルが無い」のか「channel が無効」なのかを切り分けられるよう、
        // 解決結果を添える（http_source=none ならこの体の Discord ハンドルが無い）。
        tracing::warn!(
            agent_id = %agent_id,
            channel_target = %channel_target,
            http_source = http_source.as_str(),
            http_resolved = http_opt.is_some(),
            channel_valid = channel_id_u64.is_some(),
            "Heartbeat: 発火したが発話先が無い（transport 未稼働 / channel 未設定・空/無効）。発話を取りこぼした"
        );
        false
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
        let none_http = empty_http();
        deliver_via_discord_shared_http(&none_http, "crab", "123456789", "hi").await;
        // 不正な channel（数値でない）。
        deliver_via_discord_shared_http(&none_http, "crab", "not-a-number", "hi").await;
    }

    // ---- #400: ハンドルは発話する体ごとに解決する ----

    /// テスト用のハンドル。`serenity::http::Http::new` は接続しない（実ネットワークに
    /// 出ない）。Discord feature 無効時は `()` なので識別子は持てない。
    #[cfg(feature = "discord")]
    fn fake_http(token: &str) -> crate::DiscordHttp {
        Arc::new(serenity::http::Http::new(token))
    }
    #[cfg(not(feature = "discord"))]
    fn fake_http(_token: &str) -> crate::DiscordHttp {}

    /// 体ごとのハンドルを返す偽の per-agent ゲートウェイ（ネットワークに出ない）。
    /// **誰の id で引かれたか**を記録する（配送経路が発話者の id を渡すことの観測点）。
    struct FakePerAgentHttp {
        handles: Vec<(String, crate::DiscordHttp)>,
        queried: Mutex<Vec<String>>,
    }

    impl FakePerAgentHttp {
        fn queried(&self) -> Vec<String> {
            self.queried.lock().unwrap().clone()
        }
    }

    impl PerAgentDiscordHttp for FakePerAgentHttp {
        fn http_for_agent(&self, agent_id: &str) -> Option<crate::DiscordHttp> {
            self.queried.lock().unwrap().push(agent_id.to_string());
            self.handles
                .iter()
                .find(|(id, _)| id == agent_id)
                .map(|(_, http)| http.clone())
        }
    }

    fn fake_per_agent(handles: Vec<(&str, crate::DiscordHttp)>) -> Arc<FakePerAgentHttp> {
        Arc::new(FakePerAgentHttp {
            handles: handles
                .into_iter()
                .map(|(id, http)| (id.to_string(), http))
                .collect(),
            queried: Mutex::new(Vec::new()),
        })
    }

    fn empty_http() -> HeartbeatDiscordHttp {
        HeartbeatDiscordHttp::new(Arc::new(Mutex::new(None)))
    }

    fn with_shared(http: crate::DiscordHttp) -> HeartbeatDiscordHttp {
        HeartbeatDiscordHttp::new(Arc::new(Mutex::new(Some(http))))
    }

    /// **#400 の核心。** 体ごとに**その体自身の**ゲートウェイのハンドルが返る。
    /// `agent_ids` の並び順（ここでは登録順）に関係なく、2 番目の体も自分のハンドルを得る
    /// ＝「先頭の体の名前で投稿される」なりすましが構造的に起きない。
    #[test]
    fn resolves_each_agents_own_handle_regardless_of_order() {
        let first = fake_http("token-first");
        let second = fake_http("token-second");
        let resolver = with_shared(fake_http("token-shared"));
        resolver.set_per_agent_source(fake_per_agent(vec![
            ("first", first.clone()),
            ("second", second.clone()),
        ]));

        let (http_first, source_first) = resolver.resolve("first");
        let (http_second, source_second) = resolver.resolve("second");

        assert_eq!(source_first, HeartbeatHttpSource::PerAgent);
        assert_eq!(
            source_second,
            HeartbeatHttpSource::PerAgent,
            "先頭以外の体も自分のゲートウェイから引く（共有 1 本ではない）"
        );
        #[cfg(feature = "discord")]
        {
            assert!(
                Arc::ptr_eq(&http_first.unwrap(), &first),
                "1 体目には 1 体目のハンドル"
            );
            assert!(
                Arc::ptr_eq(&http_second.unwrap(), &second),
                "2 体目には**2 体目の**ハンドル（先頭のものではない）"
            );
        }
        #[cfg(not(feature = "discord"))]
        {
            assert!(http_first.is_some() && http_second.is_some());
        }
    }

    /// 自分のゲートウェイを持たない体は共有（TOML）ゲートウェイへ落ちる。
    /// **これを残すのが `heartbeat_delivery` モジュール doc の制約**（共有ゲートウェイは
    /// 登録簿に載らないので、per-agent が無いときに諦めると共有のみの構成で発話が飛ばない）。
    #[test]
    fn falls_back_to_the_shared_gateway_handle() {
        let shared = fake_http("token-shared");
        let resolver = with_shared(shared.clone());
        resolver.set_per_agent_source(fake_per_agent(vec![("other", fake_http("token-other"))]));

        let (http, source) = resolver.resolve("crab");

        assert_eq!(source, HeartbeatHttpSource::Shared);
        #[cfg(feature = "discord")]
        assert!(
            Arc::ptr_eq(&http.unwrap(), &shared),
            "他の体のハンドルではなく共有ゲートウェイのハンドル"
        );
        #[cfg(not(feature = "discord"))]
        assert!(http.is_some());

        // per-agent の引き口が未配線（Discord feature 無効・共有のみ構成）でも同じ。
        let unwired = with_shared(fake_http("token-shared"));
        assert_eq!(unwired.resolve("crab").1, HeartbeatHttpSource::Shared);
    }

    /// per-agent も共有も無ければ解決できない（＝この体は Discord へ出せない）。
    #[test]
    fn reports_none_when_no_handle_is_available() {
        let resolver = empty_http();
        resolver.set_per_agent_source(fake_per_agent(vec![("other", fake_http("token-other"))]));

        let (http, source) = resolver.resolve("crab");

        assert!(http.is_none());
        assert_eq!(source, HeartbeatHttpSource::None);
    }

    /// **解決できなかったら起動時診断は WARN。** 以前はここが `if let Some` の空振りで
    /// 何も出ず、Discord へ一切出ない構成のまま何度も起動していた（#400）。
    #[test]
    fn startup_diagnosis_warns_only_when_the_handle_is_missing() {
        assert_eq!(
            startup_http_diagnosis(HeartbeatHttpSource::None).0,
            tracing::Level::WARN,
            "ハンドル未解決は WARN（沈黙させない）"
        );
        assert_eq!(
            startup_http_diagnosis(HeartbeatHttpSource::PerAgent).0,
            tracing::Level::INFO
        );
        assert_eq!(
            startup_http_diagnosis(HeartbeatHttpSource::Shared).0,
            tracing::Level::INFO
        );
        // 理由は空にしない（どの体で・なぜ取れないかをログに残すため）。
        for source in [
            HeartbeatHttpSource::None,
            HeartbeatHttpSource::PerAgent,
            HeartbeatHttpSource::Shared,
        ] {
            assert!(!startup_http_diagnosis(source).1.is_empty());
        }
    }

    /// **配送経路は「発話する体の id」で解決を引く。** 解決規則を単体で確かめても、
    /// 配送関数が誰の id を渡すかは別の性質なのでここで押さえる（`agent_id` を渡し忘れて
    /// 固定値や先頭の体を引くようになったら落ちる）。
    /// channel は不正値にして送信分岐へ入らせない＝実ネットワークに出ない。
    #[tokio::test]
    async fn delivery_resolves_the_handle_with_the_speaking_agents_id() {
        let source = fake_per_agent(vec![
            ("first", fake_http("token-first")),
            ("second", fake_http("token-second")),
        ]);
        let resolver = empty_http();
        resolver.set_per_agent_source(source.clone());

        deliver_via_discord_shared_http(&resolver, "second", "not-a-number", "hi").await;

        assert_eq!(
            source.queried(),
            vec!["second".to_string()],
            "発話者（2 体目）の id で 1 回引く（先頭の体でも固定値でもない）"
        );
    }

    /// **解決できないとき、実際に warn イベントが出て、どの体かが分かる。**
    /// レベル判定（`startup_http_diagnosis`）だけを見ていると、出力側の分岐を反転しても
    /// フィールドを落としても気づけない。#400 の要件は「どの体で、なぜ取れなかったかを
    /// ログに残す」なのでここを実出力で押さえる。
    #[test]
    fn missing_handle_actually_emits_a_warn_naming_the_agent() {
        let resolver = empty_http();
        resolver.set_per_agent_source(fake_per_agent(vec![("other", fake_http("token-other"))]));

        let logs = captured_logs(|| log_startup_http_resolution(&resolver, "speaker-under-test"));

        assert!(logs.contains("WARN"), "warn レベルで出ること: {logs}");
        assert!(
            logs.contains("agent_id") && logs.contains("speaker-under-test"),
            "どの体か分かること（agent_id フィールド付き）: {logs}"
        );
        assert!(
            logs.contains("http_source") && logs.contains("none"),
            "なぜ取れないか（解決の出所）が載ること: {logs}"
        );
    }

    /// 逆側: 解決できたときは warn を出さない（上のテストと対で、出力側の分岐が
    /// 反転したら両方が落ちる）。
    #[test]
    fn a_resolvable_handle_emits_no_warn() {
        let resolver = with_shared(fake_http("token-shared"));

        let logs = captured_logs(|| log_startup_http_resolution(&resolver, "speaker-under-test"));

        assert!(
            logs.is_empty(),
            "解決できていれば WARN 以上は出ない（info のみ）: {logs}"
        );
    }

    /// テスト用: `tracing` 出力を文字列として捕まえるヘルパー。
    ///
    /// **`crates/server/src/caller_identity.rs` の同名ヘルパーの複製**（設計と注意点は
    /// そちらのコメントが本体）。共有できないのはターゲットが違うため:
    /// `heartbeat_delivery` は bin（`main.rs`）側のモジュールで、lib 側の `#[cfg(test)]`
    /// アイテムは bin のテストビルドには入らない。lib 側へ出すには本番ビルドに
    /// テスト専用の捕捉機構を公開することになるので、複製を選ぶ。
    mod capture {
        use std::cell::RefCell;
        use std::io;
        use std::sync::{Arc, Mutex, Once};

        thread_local! {
            /// このスレッドが捕捉中なら書き込み先。捕捉していなければ `None`（捨てる）。
            static SINK: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
        }

        #[derive(Clone, Copy, Default)]
        struct ThreadLocalWriter;

        impl io::Write for ThreadLocalWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                SINK.with(|sink| {
                    if let Some(sink) = sink.borrow().as_ref() {
                        sink.lock().unwrap().extend_from_slice(buf);
                    }
                });
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalWriter {
            type Writer = ThreadLocalWriter;
            fn make_writer(&'a self) -> Self::Writer {
                *self
            }
        }

        /// `f` の実行中に**このスレッドで**出た tracing 出力（WARN 以上）を返す。
        /// 「警告条件を満たす」ではなく「実際に warn イベントが出る」ことを見るため。
        ///
        /// subscriber はプロセスで 1 個だけ張り、どのテストの出力を拾うかはスレッド
        /// ローカルの捕捉先で切り替える（`with_default` だと callsite の `Interest` が
        /// 「出さない」で焼き付く競合がある。詳細は caller_identity.rs のコメント）。
        pub(super) fn captured_logs(f: impl FnOnce()) -> String {
            static INSTALL: Once = Once::new();
            INSTALL.call_once(|| {
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(ThreadLocalWriter)
                    .with_ansi(false)
                    .with_max_level(tracing::Level::WARN)
                    .finish();
                tracing::subscriber::set_global_default(subscriber)
                    .expect("捕捉用 subscriber を張れること（他に global default が居ない）");
                // 張る途中の窓で焼き付いた `Interest` を計算し直す。
                tracing::callsite::rebuild_interest_cache();
            });

            /// `f` が panic しても捕捉先を残さない。
            struct Capturing;
            impl Drop for Capturing {
                fn drop(&mut self) {
                    SINK.with(|sink| *sink.borrow_mut() = None);
                }
            }

            let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
            SINK.with(|sink| *sink.borrow_mut() = Some(buf.clone()));
            let _capturing = Capturing;
            f();
            drop(_capturing);
            let bytes = buf.lock().unwrap().clone();
            String::from_utf8(bytes).unwrap()
        }
    }
    use capture::captured_logs;

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
        let none_http = empty_http();

        let delivered =
            deliver_heartbeat_speech(&registry, &none_http, "crab", "note-target", "自律発話")
                .await;

        assert_eq!(
            *calls.lock().unwrap(),
            vec![("note-target".to_string(), "自律発話".to_string())],
            "手順1 が content を配送し、手順2 へは進まない"
        );
        // #425: 非 Discord へ配れたターンは Discord 会話セッションへの二重記録対象では
        // ないので false（記録先の `discord-…` セッションが無い）。
        assert!(
            !delivered,
            "非 Discord transport が担当したターンは false（discord 会話セッションへ記録しない）"
        );
    }

    /// (f) 逆側: 稼働中の非 Discord transport が居なければ手順2（Discord 共有 http）へ落ちる。
    /// http 無しでも panic せず、registry 側 spy には何も渡らない。
    #[tokio::test]
    async fn deliver_heartbeat_speech_falls_through_to_discord_when_step1_declines() {
        let (calls, _delivery) = spy();
        // 空 registry（手順1 は誰も引き受けない）。
        let registry = AgentGatewayRegistry::new();
        let none_http = empty_http();

        let delivered =
            deliver_heartbeat_speech(&registry, &none_http, "crab", "123456789", "hi").await;

        assert!(
            calls.lock().unwrap().is_empty(),
            "手順1 が居なければ registry spy には渡らない（手順2 の Discord 経路へ）"
        );
        // #425: 手順2 も http 未解決で配れなかった（送信していない）。方向ケース: 配信に
        // 失敗したターンは false ＝ 呼び出し側は記録しない（言っていないことを残さない）。
        assert!(
            !delivered,
            "Discord ハンドルが無く配れなかったターンは false（記録しない）"
        );
    }
}
