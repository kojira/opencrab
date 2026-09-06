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

use opencrab_gateway::IncomingMessage;

mod incoming;
mod interactions;
mod loop_runtime;
mod reactions;
mod turn_completion;

pub use loop_runtime::run_discord_loop;
pub(crate) use reactions::ReactionAdder;

#[cfg(test)]
use incoming::process_incoming_message;
#[cfg(test)]
use interactions::process_interaction_response;
#[cfg(test)]
use reactions::{
    debounce_window_key, end_of_speech_qualifies, end_of_speech_qualifies_ok, incoming_has_content,
    parse_reaction_message_id, FAILED_EMOJI, NO_REPLY_EMOJI, SEEN_EMOJI, SPOKE_EMOJI,
};
#[cfg(test)]
use turn_completion::{handle_agent_response, process_subtask_completed};

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
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
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
