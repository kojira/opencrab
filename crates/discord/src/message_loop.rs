//! Discordゲートウェイのメッセージ処理ループ（Event-Driven v3）。
//!
//! v3の変更点:
//! - Event-Drivenモデル: IncomingMessageとSubtaskCompletedをmpscチャンネルで処理
//! - P0修正: on_response_textコールバックでストリーミング応答を送信
//! - P1修正: 処理をtokio::spawnで非同期化、メインループをブロックしない
//! - P2修正: SubtaskCompleted callbackをLoopEvent送信に変更、イベントループで直列処理
//!
//! v3.1: P2 の「イベントループで直列処理」は廃止。SubtaskCompleted /
//! InteractionResponse の推論をループ内で await すると、その間**全チャンネル・
//! 全エージェント**の受信処理が停止する（サブタスクが report_progress するたびに
//! メインが無応答になる）。現在は全イベントを spawn + セッション単位ロック
//! （`SessionLocks::spawn_serialized`）で処理し、直列化の範囲を同一セッションに限定する。
//!
//! v3.2 (#156 S2): セッションロック表は Discord 独自実装をやめ、gateway 非依存層の
//! [`SessionLocks`](opencrab_actions::SessionLocks) に統合した（web / Nostr と同一実装）。
//! Discord 固有なのは「結果を待たずに spawn する」形だけで、それも共通側の薄い入口
//! [`SessionLocks::spawn_serialized`](opencrab_actions::SessionLocks::spawn_serialized)
//! に寄せてある。

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::gateway::DiscordGateway;
use opencrab_actions::{
    accept_inbound, delivery_effect, prepare_session_inbound, run_session_turn, start_session_turn,
    AdmittedInbound, DeliveryEffect, InboundAgentDrop, InboundLookups, InboundMessageDrop,
    InboundWork, NormalizedInbound, NormalizedInboundEvent, SessionLocks, TranscriptSource,
};
use opencrab_core::a2ui::UiRenderer;
use opencrab_gateway::IncomingMessage;

use crate::AgentRunner;

/// V3（専用）Discord gateway process の liveness を返す probe（`agent_id` → 受信中か）。
///
/// DESIGN-DISCORD-GATE §8.1 の二重受信防止 lever の per-agent legacy ループ側。実体は
/// server 層で `ExtgateState::agent_has_live_gateway(agent, "discord")` を包む closure で、
/// 判定は core の in-memory live registry を正とする（DB の enabled ではない）。
/// probe/ロック失敗は **false**（＝退かない）へ倒れ、V3 が死んでいる/不明なら legacy が
/// 処理を続けて外形を減らさない（#40 の `served_by_dedicated_gateway` と同じ fail-open 方向）。
///
/// `crate::server::dedicated_gateway::V3LivenessProbe`（V3AwareGateway 側）と同一の
/// 具象型（型エイリアスは透過）なので、server 層は 1 本の closure を両者へ渡せる。
pub type V3LivenessProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// 同一(channel, sender)のメッセージをまとめるまでの待機時間。
const DEBOUNCE_DELAY: Duration = Duration::from_secs(2);

/// 受信転送タスクが `recv()` エラーから再試行するまでの初回待機（#284 P0-2）。
///
/// 短くしすぎるとゲートウェイ切断中に error ログでディスクを埋める。長くしすぎると
/// 復旧後の受信再開が遅れる。500ms から始めて指数で伸ばし、上限で頭打ちにする。
const RECV_RETRY_BASE: Duration = Duration::from_millis(500);
/// 再試行間隔の上限。切断が長引いても 30 秒以内には必ず再接続を試す
/// （Discord 側の再接続が済んでいれば次の `recv()` で受信が戻る）。
const RECV_RETRY_MAX: Duration = Duration::from_secs(30);
/// これだけ連続で失敗したらオーナーへエスカレーションする。
///
/// 一過性の切断は数回の再試行で戻るので、単発では鳴らさない。5 回連続
/// （= 概ね 8 秒以上復旧しない）なら「沈黙したまま受信が死んでいる」疑いが濃い。
const RECV_FAILURES_BEFORE_ALERT: u32 = 5;

/// この回数の失敗ごとにエスカレーションを繰り返す（#286）。
///
/// 「N 回目ちょうど」で 1 度だけ鳴らすと、以後いくら失敗し続けても二度と警告が出ない
/// ＝ 復旧しないまま沈黙する（この機構が防ぎたかった状態そのもの）。バックオフが
/// 上限 30 秒で頭打ちなので、5 回ごと ≒ 2〜3 分おきの再通知になる。
fn should_alert_inbound_stalled(consecutive_failures: u32) -> bool {
    consecutive_failures >= RECV_FAILURES_BEFORE_ALERT
        && consecutive_failures.is_multiple_of(RECV_FAILURES_BEFORE_ALERT)
}

/// 連続失敗回数に対する再試行間隔（指数バックオフ、上限あり）。
fn recv_retry_backoff(consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.saturating_sub(1).min(16);
    RECV_RETRY_BASE
        .saturating_mul(1u32 << shift)
        .min(RECV_RETRY_MAX)
}

/// whitelist / DM trust による受信破棄を INFO で残すときの間引き窓（#419）。
///
/// 破棄自体は正しい動作だが、busy な非 whitelist チャンネルで破棄が連発すると
/// 同じ 1 行で `.server.log` が埋まる。同一宛先・同一理由の破棄は最大この間隔に
/// 1 行へ抑え、「このエージェントは今この宛先を設定で無視している」ことが grep で
/// 分かる可視性は保ちつつ洪水を防ぐ。
const DROP_LOG_THROTTLE: Duration = Duration::from_secs(300);

/// (理由:宛先) ごとに最後に破棄 INFO を出した時刻。プロセス内のログ間引き専用。
static DROP_LOG_LAST: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Instant>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// `key`（理由と宛先の組）の破棄を今 INFO で出してよいかを返す。
///
/// 初回、または前回出力から `window` 以上経過していれば true を返し、最終出力時刻を
/// `now` に更新する。窓の内側での連続破棄では false を返してログを間引く。
fn should_emit_drop_log(
    last_by_key: &std::sync::Mutex<HashMap<String, Instant>>,
    key: &str,
    now: Instant,
    window: Duration,
) -> bool {
    let mut map = last_by_key.lock().unwrap();
    match map.get(key) {
        Some(&last) if now.duration_since(last) < window => false,
        _ => {
            // #423: 出力のたびに窓を超えた古いエントリを掃除し、マップの無制限な成長を防ぐ。
            // 掃除後に現在のキーを入れる（now は窓内なので残る）。これで保持数は「直近 window
            // 以内に破棄が起きた宛先数」で有界になる。
            map.retain(|_, &mut last| now.duration_since(last) < window);
            map.insert(key.to_string(), now);
            true
        }
    }
}

/// メッセージループへの内部イベント。
pub enum LoopEvent {
    /// Discordからの新規メッセージ。
    IncomingMessage(IncomingMessage),
    /// サブタスク完了通知（P2対策: tokio::spawnではなくイベントで直列処理）。
    SubtaskCompleted {
        session_id: String,
        agent_id: String,
        subtask_id: String,
        result: String,
        exit_reason: String,
        channel_id: u64,
        channel_id_str: String,
        guild_id: String,
        is_dm: bool,
        /// resume する親ターンの呼び出し元（#298）。subtask を spawn した run の
        /// caller をそのまま引き継ぐ。ここを `Agent` 固定にすると、オーナー発の
        /// ターンが subtask 決着で降格し、owner/trusted のツールが list_tools からも
        /// dispatch からも丸ごと消える。
        caller: opencrab_actions::CallerIdentity,
    },
    /// A2UIインタラクション応答（ボタンクリック or タイムアウト）。
    InteractionResponse {
        interaction_id: String,
        session_id: String,
        agent_id: String,
        channel_id: u64,
        channel_id_str: String,
        guild_id: String,
        response: opencrab_core::a2ui::A2uiUserAction,
        is_dm: bool,
        /// resume する run の呼び出し元 =**その UI を描いた run の caller**
        /// （`PendingInteraction.caller` / #298 / #302）。
        ///
        /// 応答者（`response.responder_id`）から導出しては**いけない**。`send_ui` の
        /// `channel_id` は自由引数で、描画先チャンネルと resume 先セッションは
        /// 独立している。応答者から導くと `Agent` / `TrustedUser` のターンが描いた UI を
        /// オーナーが押した瞬間にそのセッションが `Owner` で resume する（昇格経路）。
        caller: opencrab_actions::CallerIdentity,
    },
    /// 時刻起因の発火（#588 TimedFire）。scheduler が「時刻が来たら、このセッションで・この
    /// プロンプトで 1 ターン回して」と送る。メッセージ以外の理由でターンを回す点は
    /// `SubtaskCompleted` と同じで、受けたら**いつもの turn**（配送・ロック・記録・継続ターンは
    /// ループ既存の実装）を回すだけ。`prompt` は system プロンプトへ足す（会話ログに「発言」として
    /// 残さない）。イベントは**種別を知らない**（ハートビート/アラーム/定時実行いずれも同じ口）。
    TimedFire {
        session_id: String,
        agent_id: String,
        channel_id: u64,
        channel_id_str: String,
        guild_id: String,
        is_dm: bool,
        /// system プロンプトへ足す入力。#584 指示解決の結果などを scheduler が渡す。
        prompt: String,
        /// 実行権限（時刻発火は本人の自己実行なので `Owner`）。
        caller: opencrab_actions::CallerIdentity,
    },
}

/// Discordのsystem promptに埋め込むcontext行を生成する。
///
/// guild_idが非空のときは `[Discord context: guild_id=..., channel_id=...]`、
/// 空（DM）のときは後方互換のため `[Discord context: channel_id=...]` を返す。
fn discord_context_line(guild_id: &str, channel_id: &str) -> String {
    if guild_id.is_empty() {
        format!("[Discord context: channel_id={}]", channel_id)
    } else {
        format!(
            "[Discord context: guild_id={}, channel_id={}]",
            guild_id, channel_id
        )
    }
}

/// Discord セッションID `discord-{agent_id}-{guild_id}-{channel_id}` から
/// `(guild_id, channel_id)` を復元する。DM は guild_id が空文字列。
///
/// agent_id はハイフンを含みうるため**右から**パースする（channel は数値、
/// guild は数値 or 空、という不変条件を利用）。形式が合わない場合は None。
pub(crate) fn parse_discord_session(session_id: &str) -> Option<(String, u64)> {
    // rsplitn は右から: [channel, guild, "discord-{agent_id}"]
    let mut parts = session_id.rsplitn(3, '-');
    let channel_str = parts.next()?;
    let guild = parts.next()?;
    let rest = parts.next()?;
    if !rest.starts_with("discord-") || rest.len() <= "discord-".len() {
        return None;
    }
    let channel_id: u64 = channel_str.parse().ok()?;
    if !guild.is_empty() && !guild.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((guild.to_string(), channel_id))
}

/// Discordメッセージの受信→エージェント処理→応答送信のEvent-Drivenループ。
///
/// バックグラウンドタスクとして`tokio::spawn`から呼ばれることを想定。
/// Create the event channel pair for the discord loop.
///
/// Returns (sender, receiver). The sender should be cloned and given to
/// DiscordGatewayActions (via `with_a2ui`) so it can inject events.
pub fn create_event_channel() -> (
    mpsc::UnboundedSender<LoopEvent>,
    mpsc::UnboundedReceiver<LoopEvent>,
) {
    mpsc::unbounded_channel()
}

/// scheduler の時刻発火（#588 TimedFire）を Discord ループへ流すための受け口。
///
/// [`opencrab_actions::TimedFireRouter`] に登録され、`fire_timed_turn` で
/// [`LoopEvent::TimedFire`] を 1 本 send するだけ（受け口は薄く保つ）。以降のターンは
/// ループ既存の実装（配送・ロック・記録・継続）が回す。Discord の宛先（channel_id u64 / is_dm）は
/// transport 中立な要求（`channel_id` 文字列 / `guild_id`）から復元する。
pub struct DiscordTimedFireSink {
    pub event_tx: mpsc::UnboundedSender<LoopEvent>,
}

impl opencrab_actions::TimedFireSink for DiscordTimedFireSink {
    fn fire_timed_turn(&self, req: opencrab_actions::TimedFireRequest) {
        let channel_id: u64 = req.channel_id.parse().unwrap_or(0);
        let is_dm = req.guild_id.is_empty();
        // send 失敗（ループ終了）は握りつぶす: 次 tick で再送される。
        let _ = self.event_tx.send(LoopEvent::TimedFire {
            session_id: req.session_id,
            agent_id: req.agent_id,
            channel_id,
            channel_id_str: req.channel_id,
            guild_id: req.guild_id,
            is_dm,
            prompt: req.prompt,
            caller: req.caller,
        });
    }
}

// Discord ループの起動エントリ。各引数は独立した依存（gateway / state / registry /
// voice / 各種フラグ）で、構造体化しても呼び出し側の見通しが良くならないため許容する。
#[allow(clippy::too_many_arguments)]
pub async fn run_discord_loop<T: AgentRunner>(
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    pending_registry: Option<opencrab_core::a2ui::PendingInteractionRegistry>,
    event_channel: Option<(
        mpsc::UnboundedSender<LoopEvent>,
        mpsc::UnboundedReceiver<LoopEvent>,
    )>,
    // 共有（TOML）ゲートウェイのループなら true: 専用（per-agent）ゲートウェイが
    // **稼働中**のエージェントをメッセージ処理時にスキップする（#40 — 二重処理防止）。
    // 判定は liveness ベースなので、専用側が停止/起動失敗していれば共有側が
    // フォールバックとして処理を続ける。per-agent ゲートウェイ自身のループ
    // （manager.rs）は必ず false（true にすると自分自身を skip してしまう）。
    skip_agents_with_dedicated_gateway: bool,
    // per-agent（legacy）ループなら Some: 同じ agent を V3 gateway process が**実際に受信中**
    // のときメッセージ処理をスキップする（DESIGN-DISCORD-GATE §8.1 — 二重受信防止）。判定は
    // core の live registry 由来の probe（`V3LivenessProbe`）で、DB の enabled ではない。
    // 共有（TOML）ループは `served_by_dedicated_gateway`（V3AwareGateway が V3 liveness を OR）で
    // 既に V3 を除外するので、こちらは **None**（この lever は per-agent ループ専用・二重ゲート回避）。
    v3_liveness: Option<V3LivenessProbe>,
    // VC 対話が有効なとき Some。エージェント返信を対応する VC で読み上げる。
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    // auto-dispatch した background subtask を載せる共有 registry（RFC #152 S3a / P0）。
    // `DiscordGatewayActions` と**同一**の registry を渡すことで、auto-dispatch した
    // 単一ツール subtask が `cancel_subtask` の認可ゲート経由で親/owner から停止可能になる。
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
) {
    let (event_tx, mut event_rx) = match event_channel {
        Some((tx, rx)) => (tx, rx),
        None => mpsc::unbounded_channel::<LoopEvent>(),
    };

    // Discord受信をイベントに変換するタスク（P1: メインループをブロックしない）
    //
    // #284 P0-2: **このタスクは recv エラーで死んではいけない。**
    // 以前は `recv()` が `Err` を返した時点で `break` していた。以後 `IncomingMessage` は
    // 二度と流れないが、`SubtaskCompleted` 等は別経路で届き続けるため、外からは
    // 「ループは生きているのにユーザーの発言だけが永久に届かない」状態に見える。
    // 抜けてよいのはイベントループ側が畳まれたとき（送信先チャンネルが閉じたとき）だけ。
    //
    // **ただしこれは #284 の真因ではない**（#286 のレビューで判明）。現在の
    // `crate::gateway::DiscordGateway::recv` が `Err` を返すのは
    // 受信チャンネルの全 Sender が drop されたときだけで、その `tx` は `DiscordGateway`
    // 構造体のフィールドとして保持されている。ゲートウェイが生きている限り `Err` は
    // 起きず、旧コードの `break` は**到達不能**だった。真因は別（イベントループの滞留が
    // 有力）。ここに残すのは、実装が変わって `Err` が起きうるようになったときに
    // 「沈黙して死ぬ」形へ戻らないための防御であって、事故の説明ではない。
    {
        let gw = gateway.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
            let mut last_ok = Instant::now();
            loop {
                match gw.recv().await {
                    Ok(msg) => {
                        consecutive_failures = 0;
                        last_ok = Instant::now();
                        if tx.send(LoopEvent::IncomingMessage(msg)).is_err() {
                            // 受け手（イベントループ）が終了した。ここでだけ抜ける。
                            warn!("Discord event loop receiver closed; stopping inbound forwarder");
                            break;
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        let backoff = recv_retry_backoff(consecutive_failures);
                        error!(
                            failures = consecutive_failures,
                            secs_since_last_message = last_ok.elapsed().as_secs(),
                            retry_in_ms = backoff.as_millis() as u64,
                            "Discord recv error: {e}"
                        );
                        if should_alert_inbound_stalled(consecutive_failures) {
                            crate::owner_warning::warn_inbound_stalled(
                                consecutive_failures,
                                last_ok.elapsed().as_secs(),
                            );
                        }
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });
    }

    // A2UIインタラクション受信タスク: gatewayのinteraction channelから受信して処理
    if let Some(ref registry) = pending_registry {
        let gw = gateway.clone();
        let tx = event_tx.clone();
        let registry = registry.clone();
        let renderer_http = gateway.http().clone();
        // #337: このタスクも recv エラーで**黙って死んではいけない**（受信転送 #284 と同型）。
        // 以前は `recv_interaction()` が `Err` を返した時点で `break` して黙って終了し、
        // 以後ボタン/セレクト/モーダルの応答が一切届かなくなっていた（誰も気づけない）。
        // 受信転送側と同じく、指数バックオフで再試行しつつ、連続失敗が続いたら
        // オーナーへエスカレーションする。閾値・バックオフは受信転送と同じ実装を共有する。
        tokio::spawn(async move {
            let mut consecutive_failures: u32 = 0;
            let mut last_ok = Instant::now();
            loop {
                match gw.recv_interaction().await {
                    Ok(data) => {
                        consecutive_failures = 0;
                        last_ok = Instant::now();
                        handle_component_interaction(
                            data,
                            &registry,
                            renderer_http.clone(),
                            tx.clone(),
                        )
                        .await;
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        let backoff = recv_retry_backoff(consecutive_failures);
                        error!(
                            failures = consecutive_failures,
                            secs_since_last_ok = last_ok.elapsed().as_secs(),
                            retry_in_ms = backoff.as_millis() as u64,
                            "Discord interaction recv error: {e}"
                        );
                        if should_alert_inbound_stalled(consecutive_failures) {
                            crate::owner_warning::warn_interaction_recv_stalled(
                                consecutive_failures,
                                last_ok.elapsed().as_secs(),
                            );
                        }
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });
    }

    info!(
        agents = ?agent_ids,
        "Discord event loop v3 started"
    );

    // イベント処理ループ（直列）: P2のDB競合を構造的に解消
    // #543 / #556: デバウンス**バッファ**は **channel ごとに 1 本**（タイマーも 1 本）。
    // オーナー指示「デバウンスはチャンネルごと。人で分けたらだめ」そのまま。全メッセージは
    // 個別に記録して送信者の帰属を保つ。run は**フラッシュ時に切る「連続同権限グループ」**
    // ごとに 1 回（下のフラッシュ箇所を参照）。
    //
    // **権限で並行バッファに割らない理由（#556）**: run は DB から会話全体を読むので、権限で
    // 別バッファに割ると同じ文脈に対して 2 回 run が起き 2 回答える＝減らそうとした増幅になる。
    // かといって channel だけで丸ごと 1 run にすると、caller が最後の送信者で決まり owner 指示が
    // 降格しうる。両方を避けるため、バッファは 1 本にしつつ**フラッシュ時に連続同権限で
    // グループへ切る**（グループ内は権限が揃うので caller が一意・別権限は別 run で混ざらない）。
    let mut debounce_buffers: HashMap<String, (Vec<IncomingMessage>, Instant)> = HashMap::new();

    // セッション単位の推論直列化ランタイム（gateway 非依存層の共通実装 / #156 S2）。
    // 同一セッションへの推論が並行実行されると、1つ目の応答がまだDBに記録されていない
    // 状態で2つ目の会話履歴が構築され、同じ内容を二重回答してしまう。これを防ぐため、
    // 会話履歴の構築・推論・応答ログをセッション単位で直列化する。
    // dispatch registry は既存どおり呼び出し側から受け取る（DiscordGatewayActions と
    // 同じ Arc を共有する必要があるため）。そのためここで使うのは登録簿を持たない
    // `SessionLocks`（#223）。登録簿つきの `SessionRuntime` を持つと、その登録簿を
    // 「共有のもの」と誤認して dispatch 先を差し替えたときに cancel_subtask が
    // 走行中 subtask に届かなくなる。型として存在しなければその取り違えは起きない。
    //
    // #588 Stage 2: ローカル生成をやめ、プロセス全体で 1 つの共有 `SessionLocks`
    // （`AppState::session_locks`）を使う。これで同一セッション（`discord-{agent}-{guild}-{channel}`）
    // の通常メッセージ処理ターンと heartbeat の時間トリガーターンが直列化される。
    let session_locks = state.session_locks();

    loop {
        // 次にフラッシュすべきバッファのデッドラインを計算
        let next_deadline = debounce_buffers
            .values()
            .map(|(_, deadline)| *deadline)
            .min();

        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(LoopEvent::IncomingMessage(msg)) => {
                        // バッファキー = channel だけ（#556）。同一 channel は権限に関わらず 1 本の
                        // バッファに貯める。権限による run の分割は**フラッシュ時**（連続同権限
                        // グループ）に行う。ここでは caller を解決しない（グループ分けは flush 側）。
                        let entry = debounce_buffers
                            .entry(debounce_window_key(&msg))
                            .or_insert_with(|| (Vec::new(), Instant::now() + DEBOUNCE_DELAY));
                        entry.0.push(msg);
                        entry.1 = Instant::now() + DEBOUNCE_DELAY; // タイマーリセット
                    }
                    Some(LoopEvent::SubtaskCompleted {
                        session_id,
                        agent_id,
                        subtask_id,
                        result,
                        exit_reason,
                        channel_id,
                        channel_id_str,
                        guild_id,
                        is_dm,
                        caller,
                    }) => {
                        // 推論をイベントループ内で await しない。以前はここでフル推論を
                        // 直列実行していたため、サブタスクの report_progress / 完了のたびに
                        // 全チャンネル・全エージェントの受信処理が推論終了まで止まっていた
                        // （= サブ実行中メインが無応答になる）。同一セッションの直列化は
                        // セッションロックが引き続き担保する。
                        let gateway_c = gateway.clone();
                        let state_c = state.clone();
                        let ga_c = gateway_actions.clone();
                        let voice_c = voice.clone();
                        let event_tx_c = event_tx.clone();
                        let registry_c = subtask_registry.clone();
                        let sess = session_id.clone();
                        session_locks.spawn_serialized(sess, async move {
                            process_subtask_completed(
                                session_id,
                                agent_id,
                                subtask_id,
                                result,
                                exit_reason,
                                channel_id,
                                channel_id_str,
                                guild_id,
                                is_dm,
                                gateway_c,
                                state_c,
                                ga_c,
                                voice_c,
                                event_tx_c,
                                registry_c,
                                caller,
                            )
                            .await;
                        });
                        // 実行ハンドルは返ってこない（応答は待たない = 受信ループを止めない）。
                    }
                    Some(LoopEvent::InteractionResponse {
                        interaction_id,
                        session_id,
                        agent_id,
                        channel_id,
                        channel_id_str,
                        guild_id,
                        response,
                        is_dm,
                        caller,
                    }) => {
                        // SubtaskCompleted と同じ理由でループ内では await しない。
                        let gateway_c = gateway.clone();
                        let state_c = state.clone();
                        let ga_c = gateway_actions.clone();
                        let sess = session_id.clone();
                        session_locks.spawn_serialized(sess, async move {
                            process_interaction_response(
                                interaction_id,
                                session_id,
                                agent_id,
                                channel_id,
                                channel_id_str,
                                guild_id,
                                response,
                                is_dm,
                                gateway_c,
                                state_c,
                                ga_c,
                                caller,
                            )
                            .await;
                        });
                    }
                    Some(LoopEvent::TimedFire {
                        session_id,
                        agent_id,
                        channel_id,
                        channel_id_str,
                        guild_id,
                        is_dm,
                        prompt,
                        caller,
                    }) => {
                        // SubtaskCompleted と同じ理由でループ内では await しない。同一セッションの
                        // 直列化はセッションロックが担保する（通常メッセージ・継続ターンと同じロック）。
                        let gateway_c = gateway.clone();
                        let state_c = state.clone();
                        let ga_c = gateway_actions.clone();
                        let voice_c = voice.clone();
                        let event_tx_c = event_tx.clone();
                        let registry_c = subtask_registry.clone();
                        let sess = session_id.clone();
                        session_locks.spawn_serialized(sess, async move {
                            process_timed_fire(
                                session_id,
                                agent_id,
                                channel_id,
                                channel_id_str,
                                guild_id,
                                is_dm,
                                prompt,
                                gateway_c,
                                state_c,
                                ga_c,
                                voice_c,
                                event_tx_c,
                                registry_c,
                                caller,
                            )
                            .await;
                        });
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(next_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600))), if !debounce_buffers.is_empty() => {
                // デッドラインを過ぎたバッファをフラッシュ
            }
        }

        // デバウンス期限が来たバッファをまとめて処理
        let now = Instant::now();
        let expired_keys: Vec<_> = debounce_buffers
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired_keys {
            if let Some((messages, _)) = debounce_buffers.remove(&key) {
                // #665: デバウンス窓が満了し、溜めていた受信の処理へ進む段。ここより前は「受信を溜めて
                // いる」正常状態で、ここが「ターン処理へ入る」入口。session_id はまだ無い（agent 毎に
                // process_incoming_message 内で決まる）ので相関はチャンネルキーで出す。
                debug!(
                    channel = %key,
                    messages = messages.len(),
                    stage = "debounce_flush",
                    "turn: デバウンス満了 → 受信処理開始"
                );
                // #543 / #556: 全メッセージを**個別に**記録する（送信者の帰属を保つ）。run は
                // **到着順のまま「連続した同一 trust_level」で切ったグループ**ごとに 1 回だけ、
                // そのグループ内の**内容のある最後のメッセージ**が起こす。run は DB から会話全体を
                // 読むので、文脈は全員分・正しい帰属で入る。
                //
                // **なぜ「連続同権限グループ」か（#556）**: バッファを channel だけで 1 本にすると
                // owner と外部ユーザーが混ざる。窓を丸ごと 1 run にすると caller が最後の送信者で
                // 決まり owner 指示が降格しうる。権限ごとに並行バッファへ割ると同じ文脈に 2 回 run が
                // 起きて増幅する。連続同権限だけをグループにすれば、**グループ内は権限が揃うので
                // caller が一意**（降格しない）で、かつ**別権限は別グループ＝別 run**（混ざらない）。
                // 例: owner→外部→co_agent は [owner][外部][co_agent] の 3 run、owner→co_agent→外部は
                // [owner,co_agent][外部] の 2 run。
                //
                // **#489 未修正の今の実運用**: co_agent は resolve_caller で `Agent`(=0) に落ちるため、
                // owner→co_agent は同権限にならず別グループになる。ここでのグループ分けは
                // trust_level が揃った場合の挙動で、#489 が直れば owner 等価(=2)として合流する。
                // 誰か（caller / trust_level）と「何本の run にするか」は core。
                // 束を 1 回投げ、分割〜record_only〜ターン対象は accept_inbound が決める（Q13）。
                let channel_ids: Vec<(String, String)> = messages
                    .iter()
                    .map(|m| match &m.source {
                        opencrab_gateway::MessageSource::Discord {
                            guild_id,
                            channel_id,
                        } => (guild_id.clone(), channel_id.clone()),
                        _ => (String::new(), String::new()),
                    })
                    .collect();
                let mut admitted: Vec<Option<AdmittedInbound>> = vec![None; messages.len()];
                let mut run_at = vec![false; messages.len()];
                let mut read_at = vec![false; messages.len()];
                let accept_err = {
                    let works: Vec<InboundWork<'_>> = messages
                        .iter()
                        .enumerate()
                        .map(|(i, m)| {
                            let (guild, ch) = &channel_ids[i];
                            InboundWork {
                                event: NormalizedInboundEvent {
                                    sender_id: &m.sender.id,
                                    channel_id: ch,
                                    guild_id: guild,
                                },
                                has_content: incoming_has_content(m),
                                kind_label: "",
                                author_key: &m.sender.id,
                            }
                        })
                        .collect();
                    let resolve = |s: &str, a: &[String], o: &str| state.resolve_caller(s, a, o);
                    let dm_any = |s: &str, a: &[String], o: &str| state.dm_allowed_any(s, a, o);
                    let dm = |s: &str, a: &str, o: &str| state.dm_allowed(s, a, o);
                    let wl = |c: &str, a: &str| state.is_channel_whitelisted_for_agent(c, a);
                    let lookups = InboundLookups {
                        resolve_caller: &resolve,
                        dm_allowed_any: &dm_any,
                        dm_allowed: &dm,
                        channel_whitelisted: &wl,
                    };
                    accept_inbound::<()>(
                        &works,
                        &owner_discord_id,
                        &agent_ids,
                        &lookups,
                        None,
                        |_| (),
                        |i, adm| admitted[i] = Some(adm.clone()),
                        |i, _, read| {
                            run_at[i] = true;
                            for &r in read {
                                read_at[r] = true;
                            }
                        },
                    )
                };
                if let Err(e) = accept_err {
                    if matches!(
                        e,
                        opencrab_actions::InboundDrop::Message(InboundMessageDrop::DmNotTrusted)
                    ) {
                        let key = format!("dm_gate:{}", messages[0].sender.id);
                        if should_emit_drop_log(
                            &DROP_LOG_LAST,
                            &key,
                            Instant::now(),
                            DROP_LOG_THROTTLE,
                        ) {
                            info!(
                                sender = %messages[0].sender.id,
                                reason = "dm_sender_not_trusted",
                                "受信DMを破棄: 設定によりどのエージェントも送信者を信頼していない"
                            );
                        }
                    }
                    continue;
                }

                if messages.len() > 1 {
                    info!(
                        channel = %key,
                        messages = messages.len(),
                        groups = run_at.iter().filter(|r| **r).count(),
                        "Debounced (channel): recording all, running once per consecutive-privilege group"
                    );
                }

                for (i, msg) in messages.into_iter().enumerate() {
                    let Some(plan) = admitted[i].clone() else {
                        continue;
                    };
                    process_incoming_message(
                        msg,
                        gateway.clone(),
                        state.clone(),
                        agent_ids.clone(),
                        gateway_actions.clone(),
                        owner_discord_id.clone(),
                        session_locks.clone(),
                        skip_agents_with_dedicated_gateway,
                        v3_liveness.clone(),
                        voice.clone(),
                        event_tx.clone(),
                        subtask_registry.clone(),
                        !run_at[i],
                        read_at[i],
                        Some(plan),
                    )
                    .await;
                }
            }
        }
    }

    info!("Discord event loop v3 ended");
}

/// 受信メッセージを処理する。
///
/// バリデーション・セッション設定・エージェント処理のスポーンを行い、即座にリターン（P1）。
#[allow(clippy::too_many_arguments)]
async fn process_incoming_message<T: AgentRunner>(
    incoming: IncomingMessage,
    gateway: Arc<DiscordGateway>,
    state: T,
    agent_ids: Vec<String>,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    owner_discord_id: String,
    session_locks: Arc<SessionLocks>,
    skip_agents_with_dedicated_gateway: bool,
    v3_liveness: Option<V3LivenessProbe>,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
    // #543: true なら記録までで終え、推論（run）は起こさない。デバウンス窓で
    // 合流したメッセージのうち、run トリガーでないものに使う。各メッセージを正しい送信者で
    // 個別に会話ログへ残しつつ、run は窓につき 1 回だけにするための分岐。
    // 👀 は record_only では付けない。ターン文脈に含まれたとき（`mark_seen`）に付ける。
    record_only: bool,
    mark_seen: bool,
    preplanned: Option<AdmittedInbound>,
) {
    let (text, image_urls) = extract_discord_content(&incoming.content);
    if text.is_empty() && image_urls.is_empty() {
        return;
    }

    let (guild_id, channel_id_str) = match &incoming.source {
        opencrab_gateway::MessageSource::Discord {
            guild_id,
            channel_id,
        } => (guild_id.clone(), channel_id.clone()),
        _ => return,
    };

    let channel_id: u64 = match channel_id_str.parse() {
        Ok(id) => id,
        Err(_) => return,
    };

    let is_dm = guild_id.is_empty();

    // #40: 専用（per-agent）ゲートウェイが稼働中のエージェントは共有ループでは処理しない。
    // ここでリストごと絞るのは、後段の core inbound（accept_inbound）にも
    // スキップ対象エージェントの trusted_users を混入させないため。専用ゲートウェイが
    // 停止/起動失敗していれば絞られず、共有側がフォールバックとして処理を続ける。
    let agent_ids: Vec<String> = if skip_agents_with_dedicated_gateway {
        let filtered: Vec<String> = agent_ids
            .into_iter()
            .filter(|agent_id| {
                if state.served_by_dedicated_gateway(agent_id) {
                    debug!(
                        agent = %agent_id,
                        "Skipping agent on shared gateway: dedicated per-agent gateway is running"
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
        if filtered.is_empty() {
            return;
        }
        filtered
    } else {
        agent_ids
    };

    // DESIGN-DISCORD-GATE §8.1: per-agent（legacy）ループは、同じ agent を V3 gateway process が
    // **実際に受信中**なら退く（二重受信防止）。これが無いと legacy 車線が同一メッセージを
    // 二重処理し、V3 が正しい返信を出す横で 👀→NO_REPLY→🤐 を付ける（本バグの症状）。
    // 判定は probe（core の live registry 由来）で行い、DB の enabled ではない。probe が false
    // （V3 死亡/未接続/ロック失敗）なら退かず legacy が処理を続けて外形を減らさない。
    // 共有ループは `v3_liveness=None` で、上の `served_by_dedicated_gateway` が V3 を OR 済み
    // （二重ゲート回避）。ここで agent 単位に絞るのは、#40 と同じく後段 core inbound の
    // trusted_users にスキップ対象を混ぜないため。
    let agent_ids: Vec<String> = if let Some(ref probe) = v3_liveness {
        let filtered: Vec<String> = agent_ids
            .into_iter()
            .filter(|agent_id| {
                if probe(agent_id) {
                    debug!(
                        agent = %agent_id,
                        "Skipping agent on legacy per-agent gateway: live V3 gateway is receiving"
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
        if filtered.is_empty() {
            return;
        }
        filtered
    } else {
        agent_ids
    };

    // 誰か・権限は core の inbound 1 口。flush 経路は束を既に投げ済み。
    let inbound_event = NormalizedInboundEvent {
        sender_id: &incoming.sender.id,
        channel_id: &channel_id_str,
        guild_id: &guild_id,
    };
    let plan = if let Some(pre) = preplanned {
        pre
    } else {
        let work = InboundWork {
            event: inbound_event,
            has_content: true,
            kind_label: "",
            author_key: &incoming.sender.id,
        };
        let mut admitted = None;
        let accept_err = {
            let resolve = |s: &str, a: &[String], o: &str| state.resolve_caller(s, a, o);
            let dm_any = |s: &str, a: &[String], o: &str| state.dm_allowed_any(s, a, o);
            let dm = |s: &str, a: &str, o: &str| state.dm_allowed(s, a, o);
            let wl = |c: &str, a: &str| state.is_channel_whitelisted_for_agent(c, a);
            let lookups = InboundLookups {
                resolve_caller: &resolve,
                dm_allowed_any: &dm_any,
                dm_allowed: &dm,
                channel_whitelisted: &wl,
            };
            accept_inbound::<()>(
                &[work],
                &owner_discord_id,
                &agent_ids,
                &lookups,
                None,
                |_| (),
                |_, adm| admitted = Some(adm.clone()),
                |_, _, _| {},
            )
        };
        match accept_err {
            Ok(()) => admitted.expect("1 件の対話系は通るか Message drop"),
            Err(opencrab_actions::InboundDrop::Message(InboundMessageDrop::DmNotTrusted)) => {
                // #419: 破棄は正しい動作だが debug だと運用ログ（INFO）に出ず「無言・エラー
                // なし」の切り分けが難しい。設定による破棄を 1 行 INFO で残す（宛先ごとに間引き）。
                let key = format!("dm_gate:{}", incoming.sender.id);
                if should_emit_drop_log(&DROP_LOG_LAST, &key, Instant::now(), DROP_LOG_THROTTLE) {
                    info!(
                        sender = %incoming.sender.id,
                        reason = "dm_sender_not_trusted",
                        "受信DMを破棄: 設定によりどのエージェントも送信者を信頼していない"
                    );
                }
                return;
            }
            Err(e) => {
                unreachable!("watch 無しの対話系で Policy は出ない: {e}");
            }
        }
    };

    debug!(
        user = %incoming.sender.name,
        channel = channel_id,
        text = %text.chars().take(50).collect::<String>(),
        "Discord message received"
    );

    if !state.has_llm_providers() {
        debug!("No LLM providers configured, skipping agent response");
        return;
    }

    let caller = plan.caller.clone();

    let discord_message_id = incoming
        .metadata
        .get("discord_message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // LLM がこの投稿を読んだ（ターン文脈に含めた）ときに付ける 👀 を**一度だけ**
    // 付与するためのフラグ。複数エージェントが同じ投稿を処理しても 1 個で済ませる。
    // 送信者で付け外しはしない（自分自身の投稿は受信側 `is_own_message` で既に除外済み）。
    let mut reaction_added = false;

    for agent_id in &agent_ids {
        match plan.agent_drop(agent_id) {
            None => {}
            Some(InboundAgentDrop::ChannelNotWhitelisted) => {
                // #419: 設定によるチャンネル破棄を 1 行 INFO で残す（宛先ごとに間引き）。
                let key = format!("chan_wl:{agent_id}:{channel_id_str}");
                if should_emit_drop_log(&DROP_LOG_LAST, &key, Instant::now(), DROP_LOG_THROTTLE) {
                    info!(
                        channel = %channel_id_str,
                        agent = %agent_id,
                        reason = "channel_not_whitelisted",
                        "受信メッセージを破棄: 設定によりこのエージェントの非whitelistチャンネル"
                    );
                }
                continue;
            }
            Some(InboundAgentDrop::DmNotTrustedForAgent) => {
                // #419: 設定による DM 破棄を 1 行 INFO で残す（宛先ごとに間引き）。
                let key = format!("dm_trust:{}:{}", agent_id, incoming.sender.id);
                if should_emit_drop_log(&DROP_LOG_LAST, &key, Instant::now(), DROP_LOG_THROTTLE) {
                    info!(
                        sender = %incoming.sender.id,
                        agent = %agent_id,
                        reason = "dm_sender_not_trusted_for_agent",
                        "受信DMを破棄: 設定によりこのエージェントは送信者を信頼していない"
                    );
                }
                continue;
            }
        }

        let session_id = format!("discord-{}-{}-{}", agent_id, guild_id, channel_id);
        let (theme, metadata_json) = build_discord_session_metadata(&incoming);
        let inbound = NormalizedInbound {
            session_id: &session_id,
            agent_id,
            sender_id: &incoming.sender.id,
            sender_name: &incoming.sender.name,
            avatar_url: incoming.sender.avatar_url.as_deref(),
            channel_id: Some(&channel_id_str),
            pubkey: None,
            text: &text,
            image_urls: &image_urls,
            external_id: &discord_message_id,
        };

        // #284 P0-1 / #286: ユーザー発言の記録は**この処理で最初に行う副作用**。
        // セッションロックより前、Discord API より前。確保と記録は core。
        debug!(agent_id = %agent_id, session_id = %session_id, stage = "record_inbound", "turn: 受信記録 開始（入）");
        if !prepare_session_inbound(
            &state,
            TranscriptSource::Discord,
            &inbound,
            &theme,
            &metadata_json,
            "discord",
        ) {
            crate::owner_warning::warn_inbound_message_dropped(
                &session_id,
                &incoming.sender.id,
                text.len(),
            );
        }
        debug!(agent_id = %agent_id, session_id = %session_id, stage = "record_inbound", "turn: 受信記録 完了（出）");

        // LLM がこの投稿を読んだ（ターン文脈に含めた）ので 👀 を付ける。
        // record-only 単体では付けない。whitelist 通過後。失敗は非致命的。
        // 複数エージェントが同一投稿を処理しても一度だけ付与する。
        if mark_seen && !reaction_added {
            add_reaction_non_fatal(
                gateway.as_ref(),
                channel_id,
                &channel_id_str,
                &discord_message_id,
                SEEN_EMOJI,
            )
            .await;
            reaction_added = true;
        }

        // #543: record-only パス（デバウンス窓の非トリガーメッセージ）は記録まで。
        // typing / 推論（run）は起こさない。👀 は上の `mark_seen`（読むターンが走った時）。
        // 合流窓のトリガー 1 通だけが run を起こし、その run が DB から会話全体（この記録も含む）
        // を読むので、情報は落ちず文脈には正しい帰属で入る。
        if record_only {
            continue;
        }

        // タイピングインジケーター（ホワイトリスト通過後のみ）。
        // #429: 1 回だけ打つと Discord の失効（約 10 秒）で応答前に消えるため、ターンが
        // 生きている間は打ち直し続ける keepalive を起こす。ガード `typing_keepalive` は
        // 下の spawn_serialized 内へ move し、ターン終了（成功・空・NO_REPLY・エラー）で
        // drop されて確実に停止する。keepalive は別タスクなのでイベントループもターン本体も
        // ブロックしない。発火条件は従来どおり（ここに来た＝応答する体だけ）。
        let typing_keepalive = {
            let gw = gateway.clone();
            crate::typing_keepalive::spawn_typing_keepalive(
                crate::typing_keepalive::TYPING_REFRESH_INTERVAL,
                move || {
                    let gw = gw.clone();
                    async move {
                        if let Err(e) = gw.start_typing(channel_id).await {
                            warn!("Failed to refresh typing indicator: {e}");
                        }
                    }
                },
            )
        };

        // NOTE: 会話履歴の構築は、推論本体とともにセッション単位ロックの内側（spawn 内）で
        // 行う。これにより、割り込みメッセージが直前の推論完了前に走って履歴が不整合に
        // なり、同じ内容を二重回答する問題を防ぐ。

        // #352: 本ターンの caller（core の inbound 1 口が解決）で index を絞る。
        let (base_prompt, agent_name) = state.build_agent_context(agent_id, &caller);
        let system_prompt = format!(
            "{}\n\n{}",
            base_prompt,
            discord_context_line(&guild_id, &channel_id_str)
        );

        // #431: 「発言終わり」リアクション用に、このターンで自分が最後に投稿した
        // メッセージ id を追跡する。on_response_text は反復ごとに発火しうるため、
        // 送信は detach spawn（P1 非ブロック）のままにしつつ、**発火順（seq）が最大**の
        // 送信を「最後の投稿」として採る（完了順は前後しうるので発火順で選ぶ）。
        // ターン終了時に送信完了を待ってからリアクションを打つため、ハンドルも集める。
        let last_self_post: std::sync::Arc<std::sync::Mutex<(u64, Option<u64>)>> =
            std::sync::Arc::new(std::sync::Mutex::new((0, None)));
        let reply_send_seq = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let reply_send_tasks: std::sync::Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // #431: このターンが background subtask を起こしたか。`reply_send_seq` と同じ
        // ターン寿命で、run が返った後に読む。自動 dispatch と明示 `spawn_subtask` の
        // 両経路が、登録簿への登録が成立したところで加算する（`RunRequest::subtask_starts`）。
        let subtask_starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let on_response_text: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>> = {
            let state_for_cb = state.clone();
            let gateway_for_cb = gateway.clone();
            let channel_id_str_for_cb = channel_id_str.clone();
            let is_dm_for_cb = is_dm;
            let voice_for_cb = voice.clone();
            let agent_id_for_cb = agent_id.clone();
            let last_self_post_cb = last_self_post.clone();
            let reply_send_seq_cb = reply_send_seq.clone();
            let reply_send_tasks_cb = reply_send_tasks.clone();
            Some(std::sync::Arc::new(move |text: String| {
                tracing::warn!(
                    channel_id = channel_id,
                    text_len = text.len(),
                    text_preview = %text.chars().take(100).collect::<String>(),
                    "on_response_text callback invoked"
                );
                // 第一柱: NO_REPLY 終端解釈で前段のみ配送（空・単独 NO_REPLY はスキップ）。
                // 破棄ログは最終応答を判定する delivery_effect が出す（反復途中での二重計上を避ける）。
                let text = match opencrab_actions::terminate_at_no_reply(&text).speech() {
                    Some(s) if !s.trim().is_empty() => s.to_string(),
                    _ => return,
                };
                // #890 §11: 末尾 CONTINUE マーカーを剥がす（継続判定は engine 済み・ここは表示
                // 保護）。WARN は delivery_effect 側へ集約（反復途中での二重計上を避ける）。
                let text = {
                    let m = opencrab_actions::strip_continue_marker(&text);
                    if m.at_tail() {
                        m.into_kept()
                    } else {
                        text
                    }
                };
                if text.trim().is_empty() {
                    return;
                }
                let writable =
                    is_dm_for_cb || state_for_cb.is_channel_writable(&channel_id_str_for_cb);
                if !writable {
                    tracing::warn!(channel_id_str = %channel_id_str_for_cb, "on_response_text: channel not writable, skipping Discord send");
                    return;
                }
                let gateway_cb = gateway_for_cb.clone();
                let voice_cb = voice_for_cb.clone();
                let channel_id_str_cb = channel_id_str_for_cb.clone();
                let agent_id_cb = agent_id_for_cb.clone();
                let last_self_post_task = last_self_post_cb.clone();
                // 発火順の連番。後段で最大 seq の送信＝最後の投稿を採る。
                let seq = reply_send_seq_cb.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let handle = tokio::spawn(async move {
                    tracing::warn!(
                        channel_id = channel_id,
                        text_len = text.len(),
                        "on_response_text: sending to Discord channel"
                    );
                    match gateway_cb.send_to_channel(channel_id, &text).await {
                        Ok(msg_id) => {
                            tracing::warn!(
                                channel_id = channel_id,
                                "on_response_text: Discord send succeeded"
                            );
                            // #431: 発火順が最大の送信だけを「最後の投稿」として記録する。
                            if let Some(id) = msg_id {
                                let mut g = last_self_post_task.lock().unwrap();
                                if seq >= g.0 {
                                    *g = (seq, Some(id));
                                }
                            }
                            // VC セッションがこのチャンネルに紐づいていれば読み上げる
                            if let Some(v) = &voice_cb {
                                v.maybe_speak(&channel_id_str_cb, &agent_id_cb, &text);
                            }
                        }
                        Err(e) => {
                            tracing::error!("on_response_text Discord send failed: {e}");
                        }
                    }
                });
                reply_send_tasks_cb.lock().unwrap().push(handle);
            }))
        };

        // エージェント処理をバックグラウンドspawnで実行（P1: メインループをブロックしない）。
        // ただしセッション単位ロックで直列化し、履歴の構築→推論→応答ログを不可分にする。
        let state_spawn = state.clone();
        let ga_spawn = gateway_actions.clone();
        let agent_id_spawn = agent_id.clone();
        let agent_name_spawn = agent_name.clone();
        let session_id_spawn = session_id.clone();
        let system_prompt_spawn = system_prompt.clone();
        let caller_spawn = caller.clone();
        let image_urls_spawn = image_urls.clone();
        let discord_message_id_spawn = discord_message_id.clone();
        // NO_REPLY の可視化（#317）で使う。`gateway_for_cb`（:657）と同じ形の持ち込み。
        let gateway_spawn = gateway.clone();
        let channel_id_str_spawn = channel_id_str.clone();
        let sender_id_spawn = incoming.sender.id.clone();
        let sender_name_spawn = incoming.sender.name.clone();
        let sender_avatar_spawn = incoming.sender.avatar_url.clone();
        let text_spawn = text.clone();
        let event_tx_spawn = event_tx.clone();
        let registry_spawn = subtask_registry.clone();
        // #429: typing keepalive をターン本体へ move する。この future がどの経路で
        // 終わっても（下の早期パスを含む）ここで束ねたガードが drop され、keepalive は停止する。
        let typing_keepalive_spawn = typing_keepalive;
        // #431: 「発言終わり」リアクションの判定に使う（最後の自分の投稿 id / 送信ハンドル /
        // 実送信を試みた回数＝このターンで発話したか）。
        let last_self_post_spawn = last_self_post.clone();
        let reply_send_tasks_spawn = reply_send_tasks.clone();
        let reply_send_seq_spawn = reply_send_seq.clone();
        let subtask_starts_spawn = subtask_starts.clone();

        // #665: ターン本体を session 直列キューへ投入する（結果は待たない・#223）。この後、直列ロックの
        // 取得は共通の `SessionLocks::run_serialized`（session_lock 段）で計装される。
        debug!(agent_id = %agent_id, session_id = %session_id, stage = "enqueue_turn", "turn: ターンを直列キューへ投入");
        session_locks.spawn_serialized(session_id.clone(), async move {
            // ターンの寿命に typing keepalive を束ねる（#429）。名前付きで保持し、
            // ブロック終端まで生かす。drop = keepalive 停止。
            let _typing_keepalive = typing_keepalive_spawn;
            // NOTE: ユーザーメッセージの記録はロックより前に済んでいる（#284 P0-1）。
            // ターン起動（フック・文脈・run）は core。配送は下の handle_agent_response。
            let inbound = NormalizedInbound {
                session_id: &session_id_spawn,
                agent_id: &agent_id_spawn,
                sender_id: &sender_id_spawn,
                sender_name: &sender_name_spawn,
                avatar_url: sender_avatar_spawn.as_deref(),
                channel_id: Some(&channel_id_str_spawn),
                pubkey: None,
                text: &text_spawn,
                image_urls: &image_urls_spawn,
                external_id: &discord_message_id_spawn,
            };
            debug!(agent_id = %agent_id_spawn, session_id = %session_id_spawn, stage = "context_build", "turn: 文脈構築 開始（入）");
            if let Some(result) = start_session_turn(
                &state_spawn,
                TranscriptSource::Discord,
                &inbound,
                &system_prompt_spawn,
                // 予算計上は wrap が前置する runtime context と一致させる（同じ theme / message_id）。
                &prepend_runtime_context_discord("", "Discord conversation", &discord_message_id_spawn),
                |raw| {
                    debug!(
                        session_id = %session_id_spawn,
                        agent_id = %agent_id_spawn,
                        conversation_len = raw.len(),
                        stage = "context_build",
                        "turn: 文脈構築 完了（出）"
                    );
                    prepend_runtime_context_discord(
                        raw,
                        "Discord conversation",
                        &discord_message_id_spawn,
                    )
                },
                |conversation| {
                    let mut run_req = opencrab_actions::RunRequest::new(
                        &agent_id_spawn,
                        &agent_name_spawn,
                        &session_id_spawn,
                        &system_prompt_spawn,
                        &conversation,
                        "discord",
                        caller_spawn,
                    )
                    .with_gateway_actions(ga_spawn)
                    .with_reply_target(channel_id_str_spawn.clone())
                    .with_image_urls(image_urls_spawn.clone());
                    if !discord_message_id_spawn.is_empty() {
                        run_req = run_req.with_trigger_message_id(discord_message_id_spawn.clone());
                    }
                    if let Some(cb) = on_response_text {
                        run_req = run_req.with_on_response_text(cb);
                    }
                    let sink: std::sync::Arc<dyn opencrab_actions::SubtaskCompletionSink> =
                        std::sync::Arc::new(crate::gateway_actions::DiscordCompletionSink {
                            event_tx: Some(event_tx_spawn.clone()),
                        });
                    run_req = run_req.with_dispatch(Some(registry_spawn.clone()), sink);
                    run_req = run_req.with_subtask_starts(subtask_starts_spawn.clone());
                    run_req
                },
            )
            .await
            {

                // #431: 「発言終わり」リアクションの可否を effect を move する前に確定する。
                // 「発話したか」は最終応答テキストではなく、このターンで on_response_text が
                // 送信タスクを起こした回数で見る（run_agent_response は既に完了しているので
                // 発火は出揃っている。送信タスク自体の完了待ちは下の detach 側で行う）。
                // 反復途中で喋って最終応答が NO_REPLY のターンを取りこぼさないため。
                let effect = delivery_effect(
                    result,
                    opencrab_actions::DeliveryContext {
                        session_id: &session_id_spawn,
                        agent_id: &agent_id_spawn,
                        origin: "discord",
                    },
                );
                let posted =
                    reply_send_seq_spawn.load(std::sync::atomic::Ordering::SeqCst) > 0;
                // このターンが「次の行動」を起こしたか（自動 dispatch / 明示 spawn_subtask）。
                let started_subtask =
                    subtask_starts_spawn.load(std::sync::atomic::Ordering::SeqCst) > 0;
                let eos_qualifies = end_of_speech_qualifies(&effect, posted, started_subtask);

                // #665: run から戻り、最終応答の処理・配送（記録／NO_REPLY 可視化）へ入る段。反復途中の
                // 配送は on_response_text の detach spawn（別途 warn ログあり）で、ここは最終応答の後始末。
                debug!(agent_id = %agent_id_spawn, session_id = %session_id_spawn, stage = "reply", "turn: 応答処理・配送 開始（入）");
                handle_agent_response(
                    effect,
                    &agent_id_spawn,
                    &session_id_spawn,
                    channel_id,
                    &channel_id_str_spawn,
                    &state_spawn,
                    gateway_spawn.as_ref(),
                    &discord_message_id_spawn,
                )
                .await;
                debug!(agent_id = %agent_id_spawn, session_id = %session_id_spawn, stage = "reply", "turn: 応答処理・配送 完了（出）");

                // #431: 自然終了かつ発話ありなら、そのターンで自分が最後に投稿した
                // メッセージに SPOKE_EMOJI を付ける。ストリーミング送信（detach spawn）が
                // 全て完了してから最後の投稿 id を読むが、その待機はセッションロックを
                // 塞がないよう別 detach タスクで行う（応答経路もブロックしない・non-fatal）。
                if eos_qualifies {
                    let handles: Vec<tokio::task::JoinHandle<()>> =
                        std::mem::take(&mut *reply_send_tasks_spawn.lock().unwrap());
                    let last_self_post_react = last_self_post_spawn.clone();
                    let gateway_react = gateway_spawn.clone();
                    let channel_id_str_react = channel_id_str_spawn.clone();
                    tokio::spawn(async move {
                        for h in handles {
                            let _ = h.await;
                        }
                        let last_id = last_self_post_react.lock().unwrap().1;
                        if let Some(id) = last_id {
                            add_reaction_non_fatal(
                                gateway_react.as_ref(),
                                channel_id,
                                &channel_id_str_react,
                                &id.to_string(),
                                SPOKE_EMOJI,
                            )
                            .await;
                        }
                    });
                }
            }
        });
    }
}

/// エージェント応答結果を処理してDiscordに送信する。
///
/// `gateway` / `channel_id` / `message_id` は `NO_REPLY` の可視化（#317）にだけ使う。
/// `message_id` は元のユーザー投稿の Discord ID（空なら付与をスキップ）。
#[allow(clippy::too_many_arguments)]
async fn handle_agent_response<T: AgentRunner, G: ReactionAdder>(
    effect: DeliveryEffect,
    agent_id: &str,
    session_id: &str,
    channel_id: u64,
    channel_id_str: &str,
    state: &T,
    gateway: &G,
    message_id: &str,
) {
    match effect {
        DeliveryEffect::NoReply => {
            debug!(agent_id = %agent_id, "Agent returned NO_REPLY");
            state.record_agent_no_reply(agent_id, session_id);
            // 黙ったことを投稿者に見せる（#317）。失敗しても応答処理は続けない
            // ＝ NO_REPLY のまま終わるのは変わらない。
            add_reaction_non_fatal(
                gateway,
                channel_id,
                channel_id_str,
                message_id,
                NO_REPLY_EMOJI,
            )
            .await;
        }
        DeliveryEffect::Text {
            body,
            tool_calls_made,
            ..
        } => {
            state.record_outbound_reply(
                opencrab_actions::TranscriptSource::Discord,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id,
                    session_id,
                    channel_id: Some(channel_id_str),
                    text: &body,
                    context: Some(opencrab_actions::AgentReplyContext::Direct { tool_calls_made }),
                },
            );
        }
        DeliveryEffect::Empty => debug!(agent_id = %agent_id, "Agent produced empty response"),
        DeliveryEffect::Failed { error } => {
            error!(agent_id = %agent_id, error = %error, "SkillEngine failed");
            // #668: ターンが失敗したことを、トリガー投稿への ❌ リアクションだけで可視化する。
            // **エラー本文はチャンネルへ出さない**（複数エージェントが居るチャンネルで互いの
            // エラー文に反応し合う無限ループを防ぐ。詳細はログ＝#665 の計装と llm_logs が持つ）。
            // ここに来るのはターンにつき 1 回・最終 Result の Err なので、エンジン内リトライ
            // （#667）が決着した後の**最終失敗時のみ**付く（途中のリトライには付かない）。
            // 付与失敗自体は add_reaction_non_fatal が warn ログで握る（それ以上連鎖しない）。
            add_reaction_non_fatal(
                gateway,
                channel_id,
                channel_id_str,
                message_id,
                FAILED_EMOJI,
            )
            .await;
        }
    }
}

/// サブタスク完了イベントを処理する（P2: イベントループで直列実行）。
///
/// `caller` は subtask を spawn した**元のターンの呼び出し元**（#298）。resume は
/// 元の会話の続きなので、ここで最小権限へ落とすと owner/trusted のツールが
/// `policy_allows` で list_tools からも dispatch からも消える。引き継ぐだけで、
/// 昇格はしない（元が `Agent` のターンは `Agent` のまま）。
#[allow(clippy::too_many_arguments)]
async fn process_subtask_completed<T: AgentRunner>(
    session_id: String,
    agent_id: String,
    subtask_id: String,
    _result: String,
    exit_reason: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
    caller: opencrab_actions::CallerIdentity,
) {
    // #352: 本ターンの caller で index を絞る（resume は元ターンの caller を引き継ぐ）。
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id, &caller);

    // Get task description from subtask session
    let task_description = {
        let sub_session_id = format!("subtask-{}", subtask_id);
        state
            .session_theme(&sub_session_id)
            .map(|theme| {
                // theme is "Subtask: {task}", strip the prefix
                theme
                    .strip_prefix("Subtask: ")
                    .unwrap_or(&theme)
                    .to_string()
            })
            .unwrap_or_default()
    };

    let system_prompt = format!(
        "{}\n\n{}\n[subtask_completed: subtask_id={}, task=\"{}\", exit_reason={}]",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        subtask_id,
        task_description,
        exit_reason
    );
    // #431: resume ターンが**さらに** subtask を投げたら、そこにも「発言終わり」は
    // 付けず次の resume へ委ねる。通常経路と同じカウンタの張り方。
    let subtask_starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    debug!(agent_id = %agent_id, session_id = %session_id, stage = "context_build", "turn: 文脈構築 開始（入）");
    let Some(run_result) = run_session_turn(
        &state,
        &session_id,
        &agent_id,
        &system_prompt,
        &prepend_runtime_context_discord("", "Discord conversation", ""),
        |raw| {
            debug!(agent_id = %agent_id, session_id = %session_id, conversation_len = raw.len(), stage = "context_build", "turn: 文脈構築 完了（出）");
            prepend_runtime_context_discord(raw, "Discord conversation", "")
        },
        |conversation| {
            opencrab_actions::RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                caller,
            )
            .with_subtask_starts(subtask_starts.clone())
            .with_gateway_actions(gateway_actions)
            .with_reply_target(channel_id_str.clone())
            .with_dispatch(Some(subtask_registry.clone()), {
                let sink: std::sync::Arc<dyn opencrab_actions::SubtaskCompletionSink> =
                    std::sync::Arc::new(crate::gateway_actions::DiscordCompletionSink {
                        event_tx: Some(event_tx.clone()),
                    });
                sink
            })
        },
    )
    .await
    else {
        return;
    };
    match delivery_effect(
        run_result,
        opencrab_actions::DeliveryContext {
            session_id: &session_id,
            agent_id: &agent_id,
            origin: "discord",
        },
    ) {
        DeliveryEffect::NoReply => {
            state.record_agent_no_reply(&agent_id, &session_id);
        }
        DeliveryEffect::Text {
            body,
            stopped_by_limit,
            ..
        } => {
            if !is_dm && !state.is_channel_writable(&channel_id_str) {
                return;
            }
            let sent_id = match gateway.send_to_channel(channel_id, &body).await {
                Ok(id) => {
                    if let Some(v) = &voice {
                        v.maybe_speak(&channel_id_str, &agent_id, &body);
                    }
                    id
                }
                Err(e) => {
                    error!("Subtask completion Discord send failed: {e}");
                    None
                }
            };
            state.record_outbound_reply(
                opencrab_actions::TranscriptSource::Discord,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id: &agent_id,
                    session_id: &session_id,
                    channel_id: Some(&channel_id_str),
                    text: &body,
                    context: Some(opencrab_actions::AgentReplyContext::SubtaskCompleted),
                },
            );
            // #431: 自然終了（NO_REPLY/空は上で return 済み）かつ打ち切りでなく、実際に
            // 投稿できたなら「発言終わり」を付ける。判定は通常経路と同じゲートへ寄せる。
            // この経路は 1 応答 1 送信なので `posted` = 送信 id の有無。付与失敗は non-fatal。
            if end_of_speech_qualifies_ok(
                stopped_by_limit,
                sent_id.is_some(),
                subtask_starts.load(std::sync::atomic::Ordering::SeqCst) > 0,
            ) {
                if let Some(id) = sent_id {
                    add_reaction_non_fatal(
                        gateway.as_ref(),
                        channel_id,
                        &channel_id_str,
                        &id.to_string(),
                        SPOKE_EMOJI,
                    )
                    .await;
                }
            }
        }
        _ => {}
    }
}

/// 時刻起因の発火（#588 TimedFire）を**いつもの Discord ターン**として処理する。
///
/// `SubtaskCompleted` の resume と同型（受信を記録せず system プロンプトへマーカー/プロンプトを足して
/// ターンを回す）だが、初回発火は「宣言 → `spawn_subtask` → 最終 `NO_REPLY`」の形になるので、通常の
/// 受信ターンと同じく **`on_response_text` で反復ごとに配送**する（そうしないと最終が `NO_REPLY` のとき
/// 宣言がチャンネルに出ない）。継続ターンは `with_dispatch`（`DiscordCompletionSink` → `SubtaskCompleted`）
/// でループ既存の resume 経路に載る（ハートビート専用の継続機構は不要）。
///
/// **受け口は薄い**: 送信・ロック・記録・継続はすべてループ既存の実装。ここが担うのは
/// 「渡された prompt を system プロンプトへ足して回す」だけ。プロンプトは会話ログに「発言」として
/// 残さない（#501）。時刻発火の沈黙（`NO_REPLY`）は無記録（ハートビートの決定を踏襲。通常の受信ターンは
/// NO_REPLY マーカーを残すが、ここは発火元メッセージが無いのでマーカー行を積み上げない）。
#[allow(clippy::too_many_arguments)]
async fn process_timed_fire<T: AgentRunner>(
    session_id: String,
    agent_id: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    is_dm: bool,
    prompt: String,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    voice: Option<std::sync::Arc<crate::voice_session::VoiceSessionManager>>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
    subtask_registry: opencrab_actions::subtask::SubtaskRegistry,
    caller: opencrab_actions::CallerIdentity,
) {
    // 時刻発火の受信ログ（#588）。送信側（scheduler）の「発火」ログと突き合わせれば、
    // scheduler→この Discord ループ間で落ちたかが分かる。heartbeat 専用の文言にしない。
    tracing::info!(
        agent_id = %agent_id,
        session_id = %session_id,
        transport = "discord",
        is_dm,
        prompt_preview = %opencrab_actions::prompt_preview(&prompt),
        "timed-fire: ターン開始（Discord loop 受信）"
    );
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id, &caller);
    // 渡された prompt（#584 指示解決の結果など）は system プロンプトへ足す（通常ターンの
    // discord_context_line も付ける）。会話ログには「発言」として残さない（#501）。
    let system_prompt = format!(
        "{}\n\n{}\n\n{}",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        prompt
    );
    // 反復ごとに応答テキストを配送する（通常の受信ターンと同じ on_response_text）。宣言が出る要。
    // NO_REPLY・空はスキップ。書き込み不可チャンネルもスキップ。発火を塞がないよう spawn。
    let on_response_text: Arc<dyn Fn(String) + Send + Sync> = {
        let gateway = gateway.clone();
        let state = state.clone();
        let voice = voice.clone();
        let channel_id_str = channel_id_str.clone();
        let agent_id = agent_id.clone();
        Arc::new(move |text: String| {
            // 第一柱: NO_REPLY 終端解釈で前段のみ配送（空・単独 NO_REPLY はスキップ）。
            // 破棄ログは最終応答を判定する delivery_effect が出す（反復途中での二重計上を避ける）。
            let text = match opencrab_actions::terminate_at_no_reply(&text).speech() {
                Some(s) if !s.trim().is_empty() => s.to_string(),
                _ => return,
            };
            // #890 §11: 末尾 CONTINUE マーカーを剥がす（継続判定は engine 済み・表示保護）。
            let text = {
                let m = opencrab_actions::strip_continue_marker(&text);
                if m.at_tail() {
                    m.into_kept()
                } else {
                    text
                }
            };
            if text.trim().is_empty() {
                return;
            }
            if !is_dm && !state.is_channel_writable(&channel_id_str) {
                return;
            }
            let gateway = gateway.clone();
            let voice = voice.clone();
            let channel_id_str = channel_id_str.clone();
            let agent_id = agent_id.clone();
            tokio::spawn(async move {
                match gateway.send_to_channel(channel_id, &text).await {
                    Ok(_) => {
                        if let Some(v) = &voice {
                            v.maybe_speak(&channel_id_str, &agent_id, &text);
                        }
                    }
                    Err(e) => tracing::error!("TimedFire Discord send failed: {e}"),
                }
            });
        })
    };

    debug!(agent_id = %agent_id, session_id = %session_id, stage = "context_build", "turn: 文脈構築 開始（入）");
    let Some(result) = run_session_turn(
        &state,
        &session_id,
        &agent_id,
        &system_prompt,
        &prepend_runtime_context_discord("", "Discord conversation", ""),
        |raw| {
            debug!(agent_id = %agent_id, session_id = %session_id, conversation_len = raw.len(), stage = "context_build", "turn: 文脈構築 完了（出）");
            prepend_runtime_context_discord(raw, "Discord conversation", "")
        },
        |conversation| {
            opencrab_actions::RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                caller,
            )
            .with_gateway_actions(gateway_actions)
            .with_reply_target(channel_id_str.clone())
            .with_on_response_text(on_response_text)
            .with_dispatch(Some(subtask_registry.clone()), {
                let sink: std::sync::Arc<dyn opencrab_actions::SubtaskCompletionSink> =
                    std::sync::Arc::new(crate::gateway_actions::DiscordCompletionSink {
                        event_tx: Some(event_tx.clone()),
                    });
                sink
            })
        },
    )
    .await
    else {
        return;
    };

    // 記録（配送は on_response_text が済ませているので送信はしない）。最終応答が NO_REPLY 以外なら
    // 通常ターンと同じ record_outbound_reply。沈黙は無記録（上記 doc）。
    match delivery_effect(
        result,
        opencrab_actions::DeliveryContext {
            session_id: &session_id,
            agent_id: &agent_id,
            origin: "discord",
        },
    ) {
        DeliveryEffect::Text {
            body,
            tool_calls_made,
            ..
        } => {
            state.record_outbound_reply(
                opencrab_actions::TranscriptSource::Discord,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id: &agent_id,
                    session_id: &session_id,
                    channel_id: Some(&channel_id_str),
                    text: &body,
                    context: Some(opencrab_actions::AgentReplyContext::Direct { tool_calls_made }),
                },
            );
        }
        DeliveryEffect::Failed { error } => {
            error!(agent_id = %agent_id, error = %error, "TimedFire turn failed");
        }
        DeliveryEffect::NoReply | DeliveryEffect::Empty => {}
    }
}

/// Discordコンポーネントインタラクション（ボタンクリック・セレクトメニュー・モーダルSubmit）を処理する。
///
/// PendingInteractionRegistryから該当するインタラクションを検索し、
/// LoopEvent::InteractionResponseとしてイベントループに送信する。
async fn handle_component_interaction(
    data: crate::gateway::ComponentInteractionData,
    registry: &opencrab_core::a2ui::PendingInteractionRegistry,
    renderer_http: Arc<serenity::http::Http>,
    event_tx: mpsc::UnboundedSender<LoopEvent>,
) {
    // Parse custom_id format: "interaction:{uuid}:{component_id}:{action_name}"
    let parts: Vec<&str> = data.custom_id.splitn(4, ':').collect();
    if parts.len() < 4 || parts[0] != "interaction" {
        warn!(custom_id = %data.custom_id, "Invalid A2UI custom_id format");
        return;
    }
    let interaction_id = parts[1].to_string();
    let component_id = parts[2].to_string();
    let action_name = parts[3].to_string();
    // serenityのインタラクション由来のguild_id（DMの場合は空）を保持。
    let guild_id = data.guild_id.clone();

    // Look up in registry, capture fields, then drop the ref
    let pending_data = {
        let pending_ref = registry.get(&interaction_id);
        match pending_ref {
            Some(ref pending) => {
                // Owner-only check.
                // オーナー未設定（空文字・空白のみ）なら誰も操作できない（#174）。
                // 以前は「空なら判定しない」＝誰でも操作可という fail-open だった。
                if !opencrab_core::owner::is_owner_id(&pending.owner_id, &data.user_id) {
                    debug!(
                        user_id = %data.user_id,
                        owner_id = %pending.owner_id,
                        "Non-owner tried to interact with owner-only UI"
                    );
                    return;
                }

                Some((
                    pending.session_id.clone(),
                    pending.agent_id.clone(),
                    // 保留状態はコアの `RenderTarget` を持つ（#156 S3）。Discord の
                    // チャンネル識別子は数値なので、移設前と同じフォールバック
                    // （`parse().unwrap_or(0)`）で数値化する。
                    pending.target.channel_id.parse::<u64>().unwrap_or(0),
                    pending.target.channel_id.clone(),
                    // 旧 `PendingInteraction.is_dm` は send_ui 時点で常に false が入って
                    // いた（送信時には判定できない）。移設後もその既定を保つ。
                    false,
                    pending.surface_id.clone(),
                    pending.rendered_message.clone(),
                    // resume の呼び出し元は**この UI を描いた run の caller**（#302）。
                    // クリックした本人からは導出しない: 上の owner-only ゲートで
                    // 押せるのはオーナーだけなので、応答者から導くと
                    // 「`Agent` のターンが描いた UI をオーナーが押す」＝昇格に
                    // なってしまう。
                    pending.caller.clone(),
                ))
            }
            None => {
                debug!(
                    interaction_id = %interaction_id,
                    "Interaction not found in registry (expired or already handled)"
                );
                None
            }
        }
    };

    let (
        session_id,
        agent_id,
        channel_id,
        channel_id_str,
        is_dm,
        surface_id,
        rendered_message,
        caller,
    ) = match pending_data {
        Some(d) => d,
        None => return,
    };

    // Handle ModalSubmit: extract field values and merge into context
    if data.interaction_kind == crate::gateway::InteractionKind::ModalSubmit {
        // Remove from registry
        let _ = registry.remove(&interaction_id);

        // Build context from modal values
        let mut context = serde_json::Map::new();
        if let Some(modal_values) = &data.modal_values {
            for (field_id, value) in modal_values {
                context.insert(field_id.clone(), serde_json::Value::String(value.clone()));
            }
        }

        let _ = event_tx.send(LoopEvent::InteractionResponse {
            interaction_id,
            session_id,
            agent_id,
            channel_id,
            channel_id_str,
            guild_id: guild_id.clone(),
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
            caller: caller.clone(),
        });
        return;
    }

    // Handle SelectMenu: merge selected_values into context
    if data.interaction_kind == crate::gateway::InteractionKind::SelectMenu {
        // Remove from registry
        let _ = registry.remove(&interaction_id);

        // Disable the select menu
        let renderer = crate::renderer::DiscordRenderer::new(renderer_http);
        let _ = renderer
            .update_on_response(
                &rendered_message,
                &opencrab_core::a2ui::UserActionResponse {
                    action_name: action_name.clone(),
                    context: None,
                    user_id: data.user_id.clone(),
                },
            )
            .await;

        // Build context with selected_values
        let mut context = serde_json::Map::new();
        if let Some(values) = &data.selected_values {
            context.insert(
                "selected_values".to_string(),
                serde_json::Value::Array(
                    values
                        .iter()
                        .map(|v| serde_json::Value::String(v.clone()))
                        .collect(),
                ),
            );
        }

        let _ = event_tx.send(LoopEvent::InteractionResponse {
            interaction_id,
            session_id,
            agent_id,
            channel_id,
            channel_id_str,
            guild_id: guild_id.clone(),
            response: opencrab_core::a2ui::A2uiUserAction {
                surface_id,
                component_id,
                action_name,
                context: Some(serde_json::Value::Object(context)),
                responder_id: data.user_id,
            },
            is_dm,
            caller: caller.clone(),
        });
        return;
    }

    // Handle Button: Form オープンは gateway の interaction_create で Modal 応答済み（ここには来ない）。

    // Remove from registry
    let _ = registry.remove(&interaction_id);

    // Disable buttons on the message
    let renderer = crate::renderer::DiscordRenderer::new(renderer_http);
    let _ = renderer
        .update_on_response(
            &rendered_message,
            &opencrab_core::a2ui::UserActionResponse {
                action_name: action_name.clone(),
                context: None,
                user_id: data.user_id.clone(),
            },
        )
        .await;

    // Send event to the loop
    let _ = event_tx.send(LoopEvent::InteractionResponse {
        interaction_id,
        session_id,
        agent_id,
        channel_id,
        channel_id_str,
        guild_id,
        response: opencrab_core::a2ui::A2uiUserAction {
            surface_id,
            component_id,
            action_name,
            context: None,
            responder_id: data.user_id,
        },
        is_dm,
        caller,
    });
}

/// A2UIインタラクション応答イベントを処理する。
///
/// SubtaskCompletedと同様のパターンで、応答情報をシステムプロンプトに含めて
/// エージェントを再呼び出しする。
///
/// `caller` は**この UI を描いた run の呼び出し元**（`PendingInteraction.caller` /
/// #298 / #302）。subtask 決着の resume（[`process_subtask_completed`]）とまったく
/// 同じ方針で、元のターンの呼び出し元を**引き継ぐだけ**。
///
/// `CallerIdentity::Agent` 固定にすると owner/trusted のツールが `policy_allows` で
/// 丸ごと消える（降格）。逆に応答者（`response.responder_id`）から導出すると昇格経路に
/// なる: `send_ui` の `channel_id` は自由引数なので、描画先チャンネルと resume 先
/// セッションは独立している。`Agent` のターンがオーナーの見るチャンネルへ UI を描き、
/// オーナーが押すとそのセッションが `Owner` で resume してしまう。
#[allow(clippy::too_many_arguments)]
async fn process_interaction_response<T: AgentRunner>(
    interaction_id: String,
    session_id: String,
    agent_id: String,
    channel_id: u64,
    channel_id_str: String,
    guild_id: String,
    response: opencrab_core::a2ui::A2uiUserAction,
    is_dm: bool,
    gateway: Arc<DiscordGateway>,
    state: T,
    gateway_actions: Arc<dyn opencrab_gateway::GatewayActions>,
    caller: opencrab_actions::CallerIdentity,
) {
    info!(
        interaction_id = %interaction_id,
        action = %response.action_name,
        component = %response.component_id,
        "Processing A2UI interaction response"
    );

    // 1. Update DB
    {
        let response_json = serde_json::to_string(&response).ok();
        state.mark_interaction_status(
            &interaction_id,
            if response.action_name == "timeout" {
                "timeout"
            } else {
                "responded"
            },
            response_json.as_deref(),
            Some(&response.responder_id),
        );
    }

    // 2. Record in session_log
    {
        let log_content = format!(
            "[interaction_response] ユーザーがUIに応答しました。\nsurface_id: {}\ncomponent_id: {}\naction: {}\ncontext: {}\nresponder: {}",
            response.surface_id,
            response.component_id,
            response.action_name,
            response.context.as_ref().map(|c| c.to_string()).unwrap_or_default(),
            response.responder_id,
        );
        state.record_interaction_response(
            &agent_id,
            &session_id,
            &opencrab_actions::InteractionRecord {
                interaction_id: &interaction_id,
                surface_id: &response.surface_id,
                action_name: &response.action_name,
                component_id: &response.component_id,
                responder_id: &response.responder_id,
                content: &log_content,
            },
        );
    }

    // 3. Re-invoke agent (same pattern as SubtaskCompleted)
    // #352: 本ターンの caller で index を絞る（resume は元ターンの caller を引き継ぐ）。
    let (base_prompt, agent_name) = state.build_agent_context(&agent_id, &caller);

    let context_str = response
        .context
        .as_ref()
        .map(|c| c.to_string())
        .unwrap_or_default();
    let system_prompt = format!(
        "{}\n\n{}\n[interaction_response: interaction_id={}, surface_id={}, action={}, component_id={}, context={}, responder={}]",
        base_prompt,
        discord_context_line(&guild_id, &channel_id_str),
        interaction_id, response.surface_id,
        response.action_name, response.component_id, context_str, response.responder_id,
    );
    // #431: この経路も規則を揃える（subtask を起こしたターンには付けない）。
    let subtask_starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    debug!(agent_id = %agent_id, session_id = %session_id, stage = "context_build", "turn: 文脈構築 開始（入）");
    let Some(run_result) = run_session_turn(
        &state,
        &session_id,
        &agent_id,
        &system_prompt,
        &prepend_runtime_context_discord("", "Discord conversation", ""),
        |raw| {
            debug!(agent_id = %agent_id, session_id = %session_id, conversation_len = raw.len(), stage = "context_build", "turn: 文脈構築 完了（出）");
            prepend_runtime_context_discord(raw, "Discord conversation", "")
        },
        |conversation| {
            opencrab_actions::RunRequest::new(
                &agent_id,
                &agent_name,
                &session_id,
                &system_prompt,
                &conversation,
                "discord",
                caller,
            )
            .with_gateway_actions(gateway_actions)
            .with_subtask_starts(subtask_starts.clone())
            .with_reply_target(channel_id_str.clone())
        },
    )
    .await
    else {
        return;
    };
    match delivery_effect(
        run_result,
        opencrab_actions::DeliveryContext {
            session_id: &session_id,
            agent_id: &agent_id,
            origin: "discord",
        },
    ) {
        DeliveryEffect::NoReply => {
            state.record_agent_no_reply(&agent_id, &session_id);
        }
        DeliveryEffect::Text {
            body,
            stopped_by_limit,
            ..
        } => {
            if !is_dm && !state.is_channel_writable(&channel_id_str) {
                return;
            }
            let sent_id = match gateway.send_to_channel(channel_id, &body).await {
                Ok(id) => id,
                Err(e) => {
                    error!("Interaction response Discord send failed: {e}");
                    None
                }
            };
            state.record_outbound_reply(
                opencrab_actions::TranscriptSource::Discord,
                &opencrab_actions::OutboundReplyRecord {
                    agent_id: &agent_id,
                    session_id: &session_id,
                    channel_id: Some(&channel_id_str),
                    text: &body,
                    context: Some(opencrab_actions::AgentReplyContext::InteractionResponse {
                        interaction_id: &interaction_id,
                    }),
                },
            );
            // #431: 自然終了（NO_REPLY/空は上で return 済み）かつ打ち切りでなく、実際に
            // 投稿できたなら「発言終わり」を付ける。判定は通常経路と同じゲートへ寄せる。
            // この経路は 1 応答 1 送信なので `posted` = 送信 id の有無。付与失敗は non-fatal。
            if end_of_speech_qualifies_ok(
                stopped_by_limit,
                sent_id.is_some(),
                subtask_starts.load(std::sync::atomic::Ordering::SeqCst) > 0,
            ) {
                if let Some(id) = sent_id {
                    add_reaction_non_fatal(
                        gateway.as_ref(),
                        channel_id,
                        &channel_id_str,
                        &id.to_string(),
                        SPOKE_EMOJI,
                    )
                    .await;
                }
            }
        }
        _ => {}
    }
}

/// IncomingMessage からセッション用のリッチメタデータとテーマを構築する。
fn build_discord_session_metadata(incoming: &IncomingMessage) -> (String, String) {
    let (guild_id, channel_id) = match &incoming.source {
        opencrab_gateway::MessageSource::Discord {
            guild_id,
            channel_id,
        } => (guild_id.clone(), channel_id.clone()),
        _ => (String::new(), String::new()),
    };

    let is_dm = guild_id.is_empty();

    if is_dm {
        let dm_user_name = incoming.sender.name.clone();
        let theme = format!("DM with {}", dm_user_name);
        let mut meta = serde_json::json!({
            "source": "discord",
            "is_dm": true,
            "channel_id": channel_id,
            "dm_user_name": dm_user_name,
            "dm_user_id": incoming.sender.id,
        });
        if let Some(ref avatar_url) = incoming.sender.avatar_url {
            meta["dm_user_avatar_url"] = serde_json::json!(avatar_url);
        }
        (theme, meta.to_string())
    } else {
        let guild_name = incoming
            .metadata
            .get("guild_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let guild_icon_url = incoming
            .metadata
            .get("guild_icon_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let channel_name = incoming
            .metadata
            .get("channel_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let theme = if !channel_name.is_empty() && !guild_name.is_empty() {
            format!("#{} in {}", channel_name, guild_name)
        } else {
            "Discord conversation".to_string()
        };

        let meta = serde_json::json!({
            "source": "discord",
            "is_dm": false,
            "guild_id": guild_id,
            "guild_name": guild_name,
            "guild_icon_url": guild_icon_url,
            "channel_id": channel_id,
            "channel_name": channel_name,
        });
        (theme, meta.to_string())
    }
}

/// Discord用: message_idを含む変動コンテキストを前置するヘルパー。
fn prepend_runtime_context_discord(
    user_message: &str,
    session_theme: &str,
    message_id: &str,
) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\nDiscord message_id: {message_id}\n\n{user_message}"
    )
}

/// リアクション付与だけを切り出した継ぎ目（#317）。
///
/// 本番の実体は [`DiscordGateway`]。テストは Discord へ実際に HTTP を出せないため、
/// 付与要求を記録する fake を差し替えて配線を固定する。gateway 非依存層には何も
/// 足さない — この trait は `crates/discord` に閉じている。
#[async_trait::async_trait]
pub(crate) trait ReactionAdder: Send + Sync {
    /// 固有メソッド `DiscordGateway::add_reaction` と**名前を分ける**。同名にすると
    /// 実装本体（`DiscordGateway::add_reaction(self, ..)`）が固有メソッドではなく
    /// この trait メソッド自身へ解決されうる。いまは固有メソッドが優先されるので
    /// 動くが、固有側が改名・削除された瞬間に**コンパイルは通ったまま無限再帰**になる。
    async fn add_unicode_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl ReactionAdder for DiscordGateway {
    async fn add_unicode_reaction(
        &self,
        channel_id: u64,
        message_id: u64,
        emoji: &str,
    ) -> anyhow::Result<()> {
        self.add_reaction(channel_id, message_id, emoji).await
    }
}

/// 元の投稿に Unicode 絵文字のリアクションを 1 個付ける（非致命的）。
///
/// 使い分けは**絵文字だけ**で、手続きは共通:
/// - 👀 = LLM がこの投稿を読んだ（ターンの文脈に含めた）
/// - 🤐 = エージェントが `NO_REPLY` を選んだ（読んで黙ると**決めた**）。
///   これが無いと投稿者からは「読んで黙った」のか「落ちて返せなかった」のか区別できない（#317）
///
/// 絵文字は呼び出し側のハードコード（設定項目にしない）。付与失敗（権限不足・削除済み
/// メッセージ・無効なID等）は握りつぶし、channel_id/message_id/絵文字とエラー内容だけを
/// ログに残す（秘密値は含めない）。message_id が空/非数値なら付与自体を諦める。
async fn add_reaction_non_fatal<G: ReactionAdder>(
    gateway: &G,
    channel_id: u64,
    channel_id_str: &str,
    message_id: &str,
    emoji: &str,
) {
    let msg_id = match parse_reaction_message_id(message_id) {
        Some(id) => id,
        None => {
            if !message_id.is_empty() {
                warn!(
                    channel_id = %channel_id_str,
                    message_id = %message_id,
                    emoji = %emoji,
                    "Skip reaction: invalid message_id"
                );
            }
            return;
        }
    };
    if let Err(e) = gateway
        .add_unicode_reaction(channel_id, msg_id, emoji)
        .await
    {
        warn!(
            channel_id = %channel_id_str,
            message_id = %message_id,
            emoji = %emoji,
            error = %e,
            "Failed to add reaction (non-fatal)"
        );
    }
}

/// LLM が投稿を読んだ（ターン文脈に含めた）ときに付ける印。
const SEEN_EMOJI: &str = "👀";

/// エージェントが `NO_REPLY` を選んだことを示す印（#317）。
/// 👀 と同じ絵文字にすると 2 つの状態が区別できなくなる。
const NO_REPLY_EMOJI: &str = "🤐";

/// ターンが自然終了したとき、自分が最後に投稿したメッセージに付ける「発言終わり」の印（#431）。
///
/// 目的: 見ている人間が「まだ続きを書いているのか、言い終わったのか」を判別できる。
/// 👀（読んだ）/🤐（黙ると決めた）と意味が衝突しない絵文字にする。付与対象も違い、
/// これは**自分の投稿**に付く（👀/🤐 は受信したユーザー投稿に付く）。
/// 既存の 2 種と同様ハードコード（設定項目は増やさない / #431 の判断）。
const SPOKE_EMOJI: &str = "🏁";

/// ターンがエラーで失敗したことを示す印（#668）。
///
/// 上流プロバイダ障害等でターンが落ちたとき、**エラー本文をチャンネルへ出す代わりに**
/// トリガー投稿へこれを付け「失敗した」ことだけを可視化する（本文投稿は複数エージェント間で
/// エラー文に反応し合う無限ループを誘発するため出さない）。付与対象は受信したユーザー投稿
/// （👀/🤐 と同じ側）だが、意味が衝突しない絵文字にする。既存の 3 種と同様ハードコード。
const FAILED_EMOJI: &str = "❌";

/// ターンが「発言終わり」リアクションの対象になるか（#431）。
///
/// `true` を返すのは、ターンが**自然に**（次の行動を選ばず）終わり、かつそのターンで
/// 自分が**実際に投稿できた**（`posted`）ときだけ。以下は `false`:
/// - エラー / タイムアウト（`Err`）… 「言い終わった」ではなく落ちた
/// - 反復上限での打ち切り（`stopped_by_limit`）… 途中で切られた
/// - `posted == false` … このターンで実投稿していない（全反復 NO_REPLY/空、または
///   非 writable で送信に至らなかった）＝そもそも発話していない
/// - `started_subtask == true` … このターンが background subtask を起こした。掘削を
///   投げたターンは「次の行動を選んで」終わっている。ここで付けると『調べますね🏁』の
///   数分後に続きが届く**逆の情報**になる。印は subtask 完了で resume したターンが
///   自然終了したときにそちらへ付く（`process_subtask_completed`）。resume ターンが
///   さらに subtask を投げた場合も同じ条件で弾かれ、次の resume へ委ねられる。
///
///   `started_subtask` の実体は `RunRequest::subtask_starts` に渡したターンローカルな
///   カウンタで、**自動 dispatch と明示 `spawn_subtask` の両方**が登録簿への登録が
///   成立したところで加算する。登録簿を後から覗く形にしないのは、run が返る前に決着
///   した subtask が既に除去されていて取りこぼす（＝まさに resume が来るケースを
///   見落とす）ため。
///
/// **最終応答テキストの中身（NO_REPLY/空）では判定しない。** 反復途中で発話し最終応答が
/// `NO_REPLY` で自然終了するターン（例: 反復1で発話 → 最終 NO_REPLY）を取りこぼすため。
/// 「発話したか」は実投稿の有無（`posted`）で見る。`posted` の実体は、通常経路では
/// 実送信を試みた回数（`reply_send_seq > 0`）、subtask/interaction 経路では送信 id
/// （`sent_id.is_some()`）。実送信を試みたが失敗して id が採れなかった場合は、この関数は
/// `true` を返すが、実際の付与は呼び出し側の「最後の投稿 id が Some か」で最終的に弾かれる。
fn end_of_speech_qualifies(effect: &DeliveryEffect, posted: bool, started_subtask: bool) -> bool {
    match effect {
        DeliveryEffect::Failed { .. } => false,
        DeliveryEffect::Text {
            stopped_by_limit, ..
        } => end_of_speech_qualifies_ok(*stopped_by_limit, posted, started_subtask),
        DeliveryEffect::NoReply | DeliveryEffect::Empty => posted && !started_subtask,
    }
}

/// [`end_of_speech_qualifies`] の Text 側だけを取り出したもの。subtask 完了 /
/// interaction 応答の経路は本文配送のあとで判定するため、この形が要る。
fn end_of_speech_qualifies_ok(stopped_by_limit: bool, posted: bool, started_subtask: bool) -> bool {
    !stopped_by_limit && posted && !started_subtask
}

/// リアクションを付ける対象の message_id を解析する。
///
/// 空文字（message_idがメタデータに無い）や数値でない場合は `None` を返し、
/// 呼び出し側はリアクション付与をスキップする。
fn parse_reaction_message_id(message_id: &str) -> Option<u64> {
    if message_id.is_empty() {
        return None;
    }
    message_id.parse::<u64>().ok()
}

/// デバウンス**バッファ**のキー = **channel だけ**（#543 / #556）。
///
/// オーナー指示「デバウンスはチャンネルごと。人で分けたらだめ」そのまま。バッファは channel
/// ごとに 1 本（タイマーも 1 本）。権限で並行バッファに割らない: run は DB から会話全体を読む
/// ので、権限で割ると同じ文脈に対し run が 2 回起きて増幅するだけ。
///
/// ただし**フラッシュ時に**、バッファ内を到着順のまま「連続した同一 trust_level」でグループへ
/// 切り、run はグループごとに 1 回起こす（`plan_record_only_flags`）。これで別権限は別 run に分かれ、
/// caller の降格（最後の送信者に引きずられる）も起きない。詳細は `run_discord_loop` のフラッシュ
/// 箇所のコメントを参照。
fn debounce_window_key(msg: &IncomingMessage) -> String {
    match &msg.source {
        opencrab_gateway::MessageSource::Discord { channel_id, .. } => channel_id.clone(),
        _ => String::new(),
    }
}

/// メッセージが応答対象になる内容（テキストまたは画像）を持つか（#543）。
///
/// デバウンス窓の run トリガー選びに使う。`process_incoming_message` 冒頭の早期 return
/// （text も image も空なら何もしない）と同じ判定にそろえ、内容の無いメッセージを
/// トリガーに選んで run を消してしまうことを防ぐ。
fn incoming_has_content(msg: &IncomingMessage) -> bool {
    let (text, images) = extract_discord_content(&msg.content);
    !text.is_empty() || !images.is_empty()
}

/// メッセージコンテンツからテキストと画像URLを抽出する。
fn extract_discord_content(content: &opencrab_gateway::MessageContent) -> (String, Vec<String>) {
    match content {
        opencrab_gateway::MessageContent::Text(t) => (t.clone(), vec![]),
        opencrab_gateway::MessageContent::Image { url, .. } => (String::new(), vec![url.clone()]),
        opencrab_gateway::MessageContent::Multi(parts) => {
            let mut texts = Vec::new();
            let mut urls = Vec::new();
            for part in parts {
                match part {
                    opencrab_gateway::ContentPart::Text(t) => texts.push(t.clone()),
                    opencrab_gateway::ContentPart::Image { url, .. } => urls.push(url.clone()),
                }
            }
            (texts.join(" "), urls)
        }
    }
}

// 本番配線の同一性テスト（#203）。変異注入の実験でこのファイルだけを巻き戻しても
// テストが消えないよう、別ファイルに置いている。
#[cfg(test)]
#[path = "message_loop_wiring_tests.rs"]
mod wiring_tests;

#[cfg(test)]
mod tests {
    use super::{
        discord_context_line, end_of_speech_qualifies, end_of_speech_qualifies_ok,
        parse_discord_session, parse_reaction_message_id, recv_retry_backoff,
        should_alert_inbound_stalled, should_emit_drop_log, FAILED_EMOJI, NO_REPLY_EMOJI,
        RECV_FAILURES_BEFORE_ALERT, RECV_RETRY_BASE, RECV_RETRY_MAX, SEEN_EMOJI, SPOKE_EMOJI,
    };
    use opencrab_actions::delivery_effect;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio::time::{Duration, Instant};

    /// #431 テスト用に `EngineResult` を組む。判定に効くのは `stopped_by_limit` だけで、
    /// `response` は「最終応答テキストでは判定しない」ことを示すために置いている。
    fn mk_result(response: &str, stopped_by_limit: bool) -> opencrab_core::EngineResult {
        opencrab_core::EngineResult {
            response: response.to_string(),
            iterations: 1,
            tool_calls_made: 0,
            stopped_by_limit,
            xml_fallback_parses: 0,
        }
    }

    fn mk_effect(response: &str, stopped_by_limit: bool) -> opencrab_actions::DeliveryEffect {
        delivery_effect(
            Ok(mk_result(response, stopped_by_limit)),
            opencrab_actions::DeliveryContext::default(),
        )
    }

    /// #431: 「発言終わり」の絵文字は既存の 2 種と衝突しない（区別できないと意味がない）。
    #[test]
    fn spoke_emoji_is_distinct_from_existing_reactions() {
        assert_ne!(SPOKE_EMOJI, SEEN_EMOJI);
        assert_ne!(SPOKE_EMOJI, NO_REPLY_EMOJI);
        assert!(!SPOKE_EMOJI.is_empty());
    }

    /// #668: 「失敗」の絵文字は他の 3 種すべてと衝突しない。読んだ（👀）や NO_REPLY（🤐）と
    /// 同じだと「失敗した」のか「読んだ／黙った」のか区別できず可視化の意味が消える。
    #[test]
    fn failed_emoji_is_distinct_from_existing_reactions() {
        assert_ne!(FAILED_EMOJI, SEEN_EMOJI);
        assert_ne!(FAILED_EMOJI, NO_REPLY_EMOJI);
        assert_ne!(FAILED_EMOJI, SPOKE_EMOJI);
        assert!(!FAILED_EMOJI.is_empty());
    }

    /// #431: 自然終了かつ発話成立のターンだけが対象。
    #[test]
    fn end_of_speech_marks_only_natural_completed_replies() {
        // 発話して自然終了・subtask を起こしていない → 対象
        assert!(end_of_speech_qualifies(
            &mk_effect("言い終わったよ", false),
            true,
            false
        ));
    }

    /// #431: **反復途中で喋り、最終応答が `NO_REPLY` で自然終了した**ターンも対象。
    /// これが取りこぼされると「調べます」と言ったきり沈黙する——🏁 が解決すべき当の
    /// 状況——に印が付かない。判定は最終応答テキストではなく実投稿の有無で見る。
    #[test]
    fn end_of_speech_marks_turn_that_spoke_then_ended_with_no_reply() {
        assert!(end_of_speech_qualifies(
            &mk_effect("NO_REPLY", false),
            true,
            false
        ));
        // 最終応答が空で終わるターン（ツール実行だけして締める）も同じ。
        assert!(end_of_speech_qualifies(&mk_effect("", false), true, false));
    }

    /// #431: 付けない経路を網羅する（恒真回避 — 各除外条件を個別に踏む）。
    #[test]
    fn end_of_speech_excludes_non_speech_and_cutoff() {
        // 発話ゼロ（全反復 NO_REPLY / 非 writable で送信に至らず）→ 付けない。
        // 逆流防止: 実投稿が無いターンには最終応答が何であれ付かない。
        assert!(!end_of_speech_qualifies(
            &mk_effect("NO_REPLY", false),
            false,
            false
        ));
        assert!(!end_of_speech_qualifies(
            &mk_effect("", false),
            false,
            false
        ));
        assert!(!end_of_speech_qualifies(
            &mk_effect("送れなかった本文", false),
            false,
            false
        ));
        // 反復上限で打ち切り（自然終了でない）→ 発話していても付けない
        assert!(!end_of_speech_qualifies(
            &mk_effect("途中まで", true),
            true,
            false
        ));
        // エラー / タイムアウト終了 → 付けない
        assert!(!end_of_speech_qualifies(
            &delivery_effect(
                Err(anyhow::anyhow!("boom")),
                opencrab_actions::DeliveryContext::default()
            ),
            true,
            false
        ));
    }

    /// #431: **subtask を起こして終わったターンには付けない。**
    ///
    /// 掘削を投げたターンは「次の行動を選んで」終わっており、数分後に完了 resume の
    /// 続きが届く。ここで付けると『調べますね🏁』という逆の情報になる。発話していても
    /// （`posted == true`）付けないのが要点。
    ///
    /// 起動経路（自動 dispatch / 明示 `spawn_subtask`）はこのゲートからは見えない。
    /// 両方が同じカウンタへ載ることは呼び出し側の配線テストが押さえる
    /// （`message_loop_wiring_tests.rs`）。
    #[test]
    fn end_of_speech_excludes_turn_that_started_a_subtask() {
        // 「調べますね」と喋ってから掘削を投げたターン。
        assert!(!end_of_speech_qualifies(
            &mk_effect("調べますね", false),
            true,
            true
        ));
        // subtask 完了 resume / interaction 経路の `Ok` 側ゲートも同じ規則に従う。
        // resume ターンがさらに subtask を投げたら、その resume にも付けず次へ委ねる。
        assert!(!end_of_speech_qualifies_ok(false, true, true));
        // 回帰: subtask を起こしていない自然終了は従来どおり対象。
        assert!(end_of_speech_qualifies(
            &mk_effect("言い終わったよ", false),
            true,
            false
        ));
        assert!(end_of_speech_qualifies_ok(false, true, false));
    }

    /// #419: フィルタ破棄 INFO は宛先ごとに間引く。初回は出し、窓の内側では抑制し、
    /// 窓を越えたら再び出す。異なる宛先どうしは互いに間引かない。
    #[test]
    fn drop_log_throttle_emits_first_suppresses_within_window_reemits_after() {
        let map: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
        let window = Duration::from_secs(300);
        let t0 = Instant::now();

        // 初回は必ず出す。
        assert!(should_emit_drop_log(&map, "chan_wl:a1:c1", t0, window));
        // 窓の内側（同一宛先）は抑制する。ここが常に true だと洪水対策が壊れる。
        assert!(!should_emit_drop_log(
            &map,
            "chan_wl:a1:c1",
            t0 + window - Duration::from_millis(1),
            window
        ));
        // 別宛先は独立に出す（他宛先の破棄で自分が間引かれない）。
        assert!(should_emit_drop_log(&map, "chan_wl:a1:c2", t0, window));
        // 窓を越えたら同一宛先でも再び出す（沈黙し続けないため）。
        assert!(should_emit_drop_log(
            &map,
            "chan_wl:a1:c1",
            t0 + window,
            window
        ));
    }

    /// #423: 出力のたびに窓を超えた古いエントリを掃除し、マップが無制限に育たない。
    /// retain を外すとここで len が増え続けて落ちる。
    #[test]
    fn drop_log_throttle_prunes_stale_entries_on_emit() {
        let map: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
        let window = Duration::from_secs(300);
        let t0 = Instant::now();

        // 3 宛先を t0 で記録。
        assert!(should_emit_drop_log(&map, "a", t0, window));
        assert!(should_emit_drop_log(&map, "b", t0, window));
        assert!(should_emit_drop_log(&map, "c", t0, window));
        assert_eq!(map.lock().unwrap().len(), 3);

        // 窓を越えた時刻で新しい宛先 d を記録 → 出力時に窓超えの a/b/c が掃除され d だけ残る。
        let later = t0 + window + Duration::from_secs(1);
        assert!(should_emit_drop_log(&map, "d", later, window));
        let m = map.lock().unwrap();
        assert_eq!(m.len(), 1, "窓超えの古いエントリは掃除される");
        assert!(m.contains_key("d"));
    }

    /// #286: エスカレーションは 1 度きりで終わらない。
    ///
    /// 「N 回目ちょうど」だけで鳴らすと、復旧しないまま失敗し続けても二度と警告が
    /// 出ず、この機構が防ぎたかった「沈黙したまま受信が死ぬ」状態に戻る。
    #[test]
    fn stalled_alert_repeats_instead_of_firing_once() {
        let n = RECV_FAILURES_BEFORE_ALERT;
        // 閾値未満では鳴らさない（一過性の切断でノイズを出さない）。
        for failures in 0..n {
            assert!(!should_alert_inbound_stalled(failures), "{failures}");
        }
        // 閾値ちょうど、およびその倍数で鳴る。
        assert!(should_alert_inbound_stalled(n));
        assert!(should_alert_inbound_stalled(n * 2));
        assert!(should_alert_inbound_stalled(n * 20));
        // 間は鳴らさない（毎回鳴らすとログが埋まる）。
        assert!(!should_alert_inbound_stalled(n + 1));
    }

    /// #284 P0-2: 再試行間隔は指数で伸び、上限で頭打ちになる（0 にならない）。
    ///
    /// 0 に落ちると切断中にビジーループでログを埋める。頭打ちが無いと、長い切断の後に
    /// 復旧しても受信再開が何時間も遅れる。
    #[test]
    fn recv_backoff_grows_then_caps() {
        assert_eq!(recv_retry_backoff(1), RECV_RETRY_BASE);
        assert_eq!(recv_retry_backoff(2), RECV_RETRY_BASE * 2);
        assert_eq!(recv_retry_backoff(3), RECV_RETRY_BASE * 4);
        // 何回失敗しても上限を超えず、かつ 0 にはならない（overflow で 0 に落ちない）。
        for failures in [8u32, 20, 1_000, u32::MAX] {
            let d = recv_retry_backoff(failures);
            assert_eq!(d, RECV_RETRY_MAX, "failures={failures}");
        }
        assert!(recv_retry_backoff(0) > std::time::Duration::ZERO);
    }

    #[test]
    fn parse_discord_session_guild_channel() {
        assert_eq!(
            parse_discord_session("discord-crab-111-222"),
            Some(("111".to_string(), 222))
        );
    }

    #[test]
    fn parse_discord_session_dm_has_empty_guild() {
        assert_eq!(
            parse_discord_session("discord-crab--222"),
            Some((String::new(), 222))
        );
    }

    #[test]
    fn parse_discord_session_agent_id_with_hyphens() {
        // agent_id はハイフンを含みうる → 右からのパースで channel/guild を確定する
        assert_eq!(
            parse_discord_session("discord-my-cool-agent-987-654"),
            Some(("987".to_string(), 654))
        );
    }

    #[test]
    fn parse_discord_session_rejects_invalid() {
        // channel が数値でない
        assert_eq!(parse_discord_session("discord-crab-111-abc"), None);
        // guild が数値でも空でもない（agent_id 末尾との混同を防ぐ）
        assert_eq!(parse_discord_session("discord-crab-xyz-222"), None);
        // discord- プレフィックスが無い / セグメント不足
        assert_eq!(parse_discord_session("subtask-1234"), None);
        assert_eq!(parse_discord_session("discord--222"), None);
        assert_eq!(parse_discord_session(""), None);
    }

    #[test]
    fn context_line_includes_guild_id_when_present() {
        assert_eq!(
            discord_context_line("123", "456"),
            "[Discord context: guild_id=123, channel_id=456]"
        );
    }

    #[test]
    fn context_line_omits_guild_id_for_dm() {
        assert_eq!(
            discord_context_line("", "456"),
            "[Discord context: channel_id=456]"
        );
    }

    #[test]
    fn parse_reaction_message_id_accepts_valid_numeric_id() {
        assert_eq!(
            parse_reaction_message_id("1234567890123456789"),
            Some(1234567890123456789)
        );
    }

    #[test]
    fn parse_reaction_message_id_rejects_empty() {
        // メタデータに discord_message_id が無いケース → スキップ
        assert_eq!(parse_reaction_message_id(""), None);
    }

    #[test]
    fn parse_reaction_message_id_rejects_non_numeric() {
        assert_eq!(parse_reaction_message_id("not-a-number"), None);
        assert_eq!(parse_reaction_message_id("123abc"), None);
    }
}
