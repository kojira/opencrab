//! **メッセージループの外（中央スケジューラ）から Discord チャンネルへ 1 通送るための送信**
//! （#400 / #588 single-entry）。
//!
//! ## なぜこれだけがハートビート側に必要か
//!
//! #588 single-entry でハートビートは専用のターン実装（旧 `heartbeat_turn.rs`）を撤去し、時刻が
//! 来たら発火先セッション上で**通常ルートと同じ 1 ターン**を走らせる（`scheduler::run_one_heartbeat`）。
//! 推論・会話組み立て・記録・直列化はすべて通常ルートのものを使う。ただ 1 点、**Discord への送信**
//! だけは通常ルートをそのまま使えない: 通常の Discord ターンは各ゲートウェイのメッセージループが
//! `DiscordGateway::send_to_channel` で送るが、スケジューラは**どのメッセージループにも属さず**発火
//! するため、その `send_to_channel` を持たない。ここが「ループ外からチャンネルへ送る」唯一の口で、
//! engine 標準の `on_response_text`（`scheduler::discord_response_text_cb`）から反復ごとに呼ばれる。
//!
//! Nostr（ブロードキャスト）はハートビートから自動配送しない（エージェントが `nostr_post` 等の
//! ツールで自分で投稿する＝通常の Nostr の動き）。したがってこのモジュールは **Discord 送信だけ**を
//! 担い、旧・transport 横断ルーティング（`DeliveryRoute` / `deliver_via_non_discord_registry`）は
//! 撤去した。
//!
//! ## Discord ハンドルは**発話する体ごと**に解決する（#400）
//!
//! 以前は `main.rs` が起動時に先頭 1 体の per-agent ゲートウェイからハンドルを 1 本取り、全体で共有
//! していた。`Http` はボットのトークンを保持するので**送信者名はハンドルが決める**ため、(1) Discord
//! へ出せるのは先頭の体だけ、(2) それ以外の体の発話も先頭の体の名前で出る、という並び順依存があった。
//! [`HeartbeatDiscordHttp`] は配送時の解決に置き換える:
//!
//! 1. **発話する体自身の per-agent ゲートウェイ**のハンドル（あればこれが正しい名前）
//! 2. 無ければ**共有（TOML）ゲートウェイ**のハンドル（従来のフォールバック）
//!
//! 共有ゲートウェイは `state.gateways`（登録簿）に載らない（登録されるのは per-agent の
//! `DiscordGatewayManager` だけ）ので、ここで直接 `Http` を持つ。「per-agent が無ければ諦める」に
//! すると共有ゲートウェイのみの構成で発話が飛ばなくなるため、共有を 2 番目のフォールバックに残す。

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

/// メッセージループの外（中央スケジューラ）から Discord チャンネルへ 1 通送る（#400 / #588
/// single-entry）。
///
/// 通常の Discord ターンは各ゲートウェイのメッセージループが `DiscordGateway::send_to_channel`
/// で送るが、ハートビートは**どのメッセージループにも属さず**発火するため、その `send_to_channel`
/// を持たない。ここが「ループ外からチャンネルへ送る」唯一の口で、engine 標準の `on_response_text`
/// （`scheduler::discord_response_text_cb`）から反復ごとに呼ばれる。
///
/// **どのハンドルで送るか**は [`HeartbeatDiscordHttp::resolve`] が `agent_id` ごとに
/// （per-agent → 共有の順で）引く（#400。送信者名がその体の名前になる）。呼び出し側は
/// `tokio::spawn` の中で `.await` し発火を塞がない（fire-and-forget）。配信の成否はログ
/// （送信成功=info / 失敗=error / ハンドル無し=warn）で観測する。
pub(crate) async fn deliver_via_discord_shared_http(
    discord_http: &HeartbeatDiscordHttp,
    agent_id: &str,
    channel_target: &str,
    content: &str,
) {
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
            } else {
                tracing::info!(agent_id = %agent_id, channel_id = %channel_target, http_source = http_source.as_str(), "Heartbeat spoke: {}", content);
            }
        }
        #[cfg(not(feature = "discord"))]
        {
            tracing::info!(agent_id = %agent_id, channel_id = %channel_target, http_source = http_source.as_str(), "Heartbeat Speak (discord disabled): {}", content);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

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
            install();
            let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
            SINK.with(|sink| *sink.borrow_mut() = Some(buf.clone()));
            let _capturing = Capturing;
            f();
            drop(_capturing);
            drain(&buf)
        }

        /// 捕捉用 subscriber をプロセスに 1 個だけ張る。
        fn install() {
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
        }

        fn drain(buf: &Arc<Mutex<Vec<u8>>>) -> String {
            let bytes = buf.lock().unwrap().clone();
            String::from_utf8(bytes).unwrap()
        }

        /// `f` が panic しても捕捉先を残さない。
        struct Capturing;
        impl Drop for Capturing {
            fn drop(&mut self) {
                SINK.with(|sink| *sink.borrow_mut() = None);
            }
        }
    }
    use capture::captured_logs;
}
