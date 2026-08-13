//! 中央スケジューラ（#439 / #437 / #438 / #455 / 設計 §3・§7）。
//!
//! # なぜ中央スケジューラか
//!
//! 旧実装は**エージェントごとに** `core::heartbeat::heartbeat_loop` を 1 本立て、固定
//! グリッド（グローバル 1800 秒に丸めた sleep）で目を覚まし、コールバック内の
//! `resolve_agent_heartbeat` + `heartbeat_firing_plan` で発火可否を判定していた。位相は
//! メモリ（`Instant`）だけで持ち、再起動で消えて（#439-1）、設定変更はループを張り直す
//! まで効かず（#437）、sleep グリッドと設定間隔が食い違っていた（#438）。
//!
//! ここでは **単一のタスク**が、`session_heartbeat_config`（enabled 行）と
//! `agent_schedules`（enabled 行・#455）を毎ウェイクで DB から読み直し、**永続アンカー**
//! （`anchor_at`/`last_fired_at`・壁時計）から**正確な次回発火時刻**を算出し、**最も近い
//! 次回発火まで眠る**。設定変更・発火ターン完了・global config 変更は `scheduler_wake`
//! （[`AppState::scheduler_wake`]）で起こして rebuild させる（即時反映・#437）。位相は DB に
//! 永続するので再起動で伸びない（#439-1）。
//!
//! # 2 種のエントリ（設計 §3.1）
//!
//! - **Heartbeat**（session 単位・[`EntryKey::Heartbeat`]）: 時刻が来たら発火先セッション上で
//!   **通常ルートと同じ 1 ターン**を走らせる（[`run_one_heartbeat`]。caller=Owner・
//!   `run_agent_response`・Discord は engine 標準の `on_response_text` で配送）。固有なのは
//!   「時間のトリガー＋渡すプロンプト（#584 指示解決）」と「発火の記録（`heartbeat_log`）」だけ。
//! - **Schedule**（#455・[`EntryKey::Schedule`]）: cron / `@every` の時刻に、登録された
//!   `message` を対象セッションへ self-message として注入し、**通常メッセージ処理経路**
//!   （`run_agent_response`・caller=Owner）で 1 ターン走らせる（SkillEngine が走る）。
//!   HB の probe とは別物なので SPEAK/LEARN/IDLE 解釈はしない。
//!
//! **キーを enum で分ける理由**: schedule は**同一セッションに複数ぶら下がる**（同じ
//! `nostr-{agent}` に「毎朝 7 時」と「3 時間ごと」が併存しうる）。in-flight / attempts を
//! session_id 文字列で持つと別スケジュールを誤ブロックする。3 つ目の用途が来ても enum の
//! 変種追加で済み、特定の用途名は型・スキーマに入らない。
//!
//! # 発火集合（不変条件・設計 §4.2 / §5）
//!
//! **HB の実発火 = `enabled AND (session が nostr- OR live G)`**。`discord-` セッションは
//! 発火時に live G（`cfg.agent.heartbeat_enabled`・hot-reload 追従）でゲートし、`nostr-`
//! は G 非依存。**whitelist ゲートは現行 HB 発火経路に存在しないので掛けない**（掛けると
//! PR1 が確立した「発火集合を変えない」不変条件と食い違う。設計 §5 N3）。
//! **schedule には G を掛けない**（統括裁定・設計 §10.1）: G は heartbeat のマスタスイッチで
//! あって schedule のものではない。schedule は自身の `enabled`（既定 0 = fail-closed）で制御し、
//! **運用者が G を切っても定時実行は止まらない**（config/docs/PR に明記）。
//!
//! # ビジーループを作らない（設計 §3.2 A1 / §6）
//!
//! 走行中（in-flight）エントリは (1) sleep の `min` 候補から除外し、(2) ターン完了で
//! wake する。よって走行中は 0 秒スピンせず、完了した瞬間に rebuild してそのターンが
//! truthful に刻んだ `last_fired_at` から次回を計算する。`last_fired_at` は**成功発火時
//! だけ**刻み（skip / 異常終了では刻まない・§6 N2）、異常終了は**メモリの last_attempt**
//! で interval ぶん backoff して再発火ループを止める（§3.7 N-a）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use opencrab_core::heartbeat::HeartbeatConfig;
use tokio::sync::watch;

use crate::heartbeat_delivery::HeartbeatDiscordHttp;
use opencrab_actions::transcript::{AgentReplyContext, OutboundReplyRecord, TranscriptSource};
use opencrab_actions::{
    CallerIdentity, RunRequest, SettleKind, SubtaskCompletionSink, SubtaskSettled,
};
use opencrab_gateway::GatewayActions;
use opencrab_server::AppState;

/// sleep の頭打ち（秒）。NTP ジャンプ・DST・notify 取りこぼしの安全網（設計 §3.4）。
/// これで頭打ちしても判定は必ず `<= now` を再評価するので、遅れて拾うだけ。
const MAX_SLEEP_SECS: u64 = 300;

/// セッションの発火先（`session_id` 接頭辞から導く・設計 §3.6）。
///
/// **実体は db クレートへ移設した**（[`opencrab_db::queries::SessionFireTarget`] /
/// [`resolve_fire_target`]）。理由: 発火（スケジューラ）と受理判定・ゲート理由表示
/// （`set_my_heartbeat` / `get_my_heartbeat`・別クレート）が **同じ種別集合**で判定
/// しなければ「設定できたのに永遠に発火しない行」ができる（設計 §13.1）。源を二重化
/// しないため、両者が依存できる db 層の 1 関数に集約する。ここは別名で受けるだけ。
use opencrab_db::queries::resolve_session_fire_target as resolve_fire_target;
use opencrab_db::queries::SessionFireTarget as FireTarget;

/// in-flight / attempts のキー（設計 §3.1）。**session_id ではなく enum**で分ける。
///
/// schedule は同一セッションに複数ぶら下がるので session_id 文字列では別スケジュールを
/// 誤ブロックする。HB は 1 セッション 1 発火なので session_id で足りる。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum EntryKey {
    /// heartbeat（session 単位）。
    Heartbeat { session_id: String },
    /// #455 の定時実行（schedule 行の id 単位）。
    Schedule { schedule_id: i64 },
}

/// 発火時の振る舞い（設計 §3.1）。
#[derive(Debug, Clone)]
enum FireKind {
    /// heartbeat: 発火先セッション上で通常ルートと同じ 1 ターンを走らせる（[`run_one_heartbeat`]）。
    Heartbeat { target: FireTarget },
    /// #455: 対象セッションへ注入して通常メッセージ処理経路で 1 ターン走らせる。
    ScheduledMessage { message: String },
}

/// rebuild で組む 1 エントリ。
#[derive(Debug, Clone)]
struct Entry {
    /// in-flight / attempts / sleep 除外のキー（設計 §3.1）。
    key: EntryKey,
    agent_id: String,
    /// 発火先/注入先セッション（`nostr-…` / `discord-…`）。
    session_id: String,
    /// `None` = 起点（anchor/last_fired）が無い＝即発火可（設計 §4.3）。
    /// `interval`/`cron` は算出済みなので保持しない（次回は完了後の rebuild で再計算する）。
    next_fire_at: Option<DateTime<Utc>>,
    kind: FireKind,
}

/// rfc3339 文字列を壁時計へ。壊れていれば `None`（起点なし扱い＝安全側では即発火だが、
/// 発火後に truthful な last_fired が上書きするので暴走しない）。
fn parse_wall_clock(s: &Option<String>) -> Option<DateTime<Utc>> {
    let s = s.as_ref()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// 2 つの時刻のうち遅い方（どちらか一方でも可）。next_fire の base に
/// `max(last_fired_at, last_attempt_at)` を入れて backoff と truthfulness を両立させる。
fn later_of(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// enabled のセッション（HB）とスケジュール（#455）から発火エントリを組む（設計 §3.2 rebuild）。
///
/// - `live_g`: 発火時に読んだ live G（`discord-` HB のマスタゲート）。`nostr-` HB・schedule は非依存。
/// - `attempts`: 異常終了の last_attempt（メモリ・§3.7 N-a）。base を後ろへ逃がして backoff。
///
/// 期待集合を手書きしない（`list_enabled_*` をそのまま解決）。
fn rebuild_entries(
    conn: &rusqlite::Connection,
    live_g: bool,
    default_interval_secs: u64,
    min_interval_secs: u64,
    attempts: &HashMap<EntryKey, DateTime<Utc>>,
) -> Vec<Entry> {
    let mut entries = Vec::new();

    // ── (A) heartbeat（session 単位・設計 §4.2 / §5） ─────────────────────────
    match opencrab_db::queries::list_enabled_session_heartbeat_configs(conn) {
        Ok(rows) => {
            for row in rows {
                let Some(target) = resolve_fire_target(&row.session_id, &row.agent_id) else {
                    // 未知/解釈不能な session_id → 発火しない（壊れた行で外部へ publish しない）。
                    tracing::warn!(
                        agent_id = %row.agent_id,
                        session_id = %row.session_id,
                        "scheduler: 発火先を解決できない session_id を skip（fail-closed）"
                    );
                    continue;
                };
                // G ゲート: `discord-` は live G が false なら発火しない（§4.2 ランタイム G ゲート）。
                // `nostr-` は G 非依存。**whitelist ゲートは掛けない**（設計 §5 N3・現行経路に無い）。
                if matches!(target, FireTarget::DiscordChannel { .. }) && !live_g {
                    continue;
                }
                // 壊れた interval（0 以下）は fail-closed で発火しない（§4.3 と同じ意味論）。
                let Some(interval_secs) = opencrab_db::queries::resolve_session_interval_secs(
                    row.interval_secs,
                    default_interval_secs,
                    min_interval_secs,
                ) else {
                    tracing::warn!(
                        agent_id = %row.agent_id,
                        session_id = %row.session_id,
                        interval_secs = ?row.interval_secs,
                        "scheduler: 壊れた interval_secs を skip（fail-closed）"
                    );
                    continue;
                };
                let key = EntryKey::Heartbeat {
                    session_id: row.session_id.clone(),
                };
                let anchor = parse_wall_clock(&row.anchor_at);
                let db_last = parse_wall_clock(&row.last_fired_at);
                // base = max(last_fired_at, last_attempt_at)。truthful な last_fired は保ちつつ、
                // 異常終了時の attempt で next_fire を interval ぶん後ろへ逃がす（§3.7 N-a）。
                let effective_last = later_of(db_last, attempts.get(&key).copied());
                let next_fire_at = opencrab_db::queries::heartbeat_next_fire_at(
                    anchor,
                    effective_last,
                    interval_secs,
                );
                entries.push(Entry {
                    key,
                    agent_id: row.agent_id,
                    session_id: row.session_id,
                    next_fire_at,
                    kind: FireKind::Heartbeat { target },
                });
            }
        }
        Err(e) => tracing::warn!("scheduler: enabled セッションの列挙に失敗: {e}"),
    }

    // ── (B) schedule（#455・設計 §7） ────────────────────────────────────────
    // **G ゲートは掛けない**（統括裁定・§10.1）。cron/`@every` の next は照会時算出
    // （キャッシュ列なし）。解釈不能な式は fail-closed で skip（CRUD が 400 で弾くので
    // 通常ここには来ないが、DB を手で壊しても外部へ影響しないための保険）。
    match opencrab_db::queries::list_enabled_agent_schedules(conn) {
        Ok(rows) => {
            for row in rows {
                let Some(schedule_id) = row.id else {
                    continue; // DB 由来は必ず Some。
                };
                let key = EntryKey::Schedule { schedule_id };
                let anchor = parse_wall_clock(&row.anchor_at);
                let db_last = parse_wall_clock(&row.last_fired_at);
                let effective_last = later_of(db_last, attempts.get(&key).copied());
                let next_fire_at = match opencrab_server::schedule_cron::schedule_next_fire_at(
                    &row.cron_expr,
                    &row.timezone,
                    anchor,
                    effective_last,
                ) {
                    Ok(next) => next,
                    Err(e) => {
                        tracing::warn!(
                            agent_id = %row.agent_id,
                            schedule_id,
                            cron_expr = %row.cron_expr,
                            "scheduler: 解釈できない schedule 式を skip（fail-closed）: {e}"
                        );
                        continue;
                    }
                };
                entries.push(Entry {
                    key,
                    agent_id: row.agent_id,
                    session_id: row.session_id,
                    next_fire_at,
                    kind: FireKind::ScheduledMessage {
                        message: row.message,
                    },
                });
            }
        }
        Err(e) => tracing::warn!("scheduler: enabled schedule の列挙に失敗: {e}"),
    }

    entries
}

/// in-flight 除去 + wake を**パニックでも**確実に行う Drop ガード（設計 §6 / §3.5d）。
///
/// 完了で in-flight を外し、同時に `scheduler_wake` を鳴らす。これで走行中に眠っていた
/// スケジューラが即座に rebuild して、完了ターンが刻んだ `last_fired_at` から次回を計算
/// できる（A1 のスピン回避と truthfulness の両立）。異常終了時の backoff は spawn 時に
/// 打った `attempts[key]` が担う（このガードは触らない）。
struct InFlightGuard {
    key: EntryKey,
    in_flight: Arc<Mutex<HashSet<EntryKey>>>,
    wake: Arc<tokio::sync::Notify>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(&self.key);
        }
        // 完了 wake。取りこぼしても §MAX_SLEEP で再ループするので正しさは rebuild が担保。
        self.wake.notify_one();
    }
}

/// ハートビート指示文を system プロンプト用の 1 文へ整形する（#501）。
///
/// `channel_name` は発火経路で決まる（`FireTarget::NostrBroadcast` は
/// [`crate::HEARTBEAT_NOSTR_CHANNEL_LABEL`]、`DiscordChannel` はチャンネル設定名）。
/// `instructions_text` は `resolve_heartbeat_instructions` の合成結果。整形はここ 1 箇所で、
/// `build_context` はこの文字列を system プロンプトへそのまま載せる。
///
/// #588 Stage 3: **ハートビートは専用の語彙を持たず、通常のターンとして走る。** いま動く必要が
/// 無ければ通常のターンと同じく `NO_REPLY` とだけ返す（沈黙＝無配送・無記録）。旧
/// `SPEAK`/`LEARN`/`IDLE` の出力規約と、見送り理由を毎回記録させる規約（#515）は撤去した。
///
/// **誘導は発火元 transport で変わる**（`posts_response_body`）。応答本文の自動配送は Discord
/// チャンネルの発火だけで、ブロードキャスト（Nostr）は自動配送しない（[`run_one_heartbeat`]。
/// オーナー判断）。したがって:
/// - **Discord（`posts_response_body = true`）**: 「この応答がそのままチャンネルへ投稿される」。
/// - **ブロードキャスト（`false`）**: 「投稿するなら投稿ツールで自分から。応答本文は投稿されない」。
///
/// 「宣言 → サブタスク起動」の進め方は**プロンプトで誘導するだけ**（機構では強制しない）。
/// **定型の宣言文は埋め込まない**（#588: 毎回同じ文字列が出ると、撤去した `IDLE:` の定型文と
/// 同じく「エージェントの発話」ではなく「システムの通知」になり、会話ログを汚染する）。伝えるのは
/// transport ごとの事実だけで、言い回しはエージェントが毎ターン自分の言葉で決める。
fn format_heartbeat_prompt(
    channel_name: &str,
    instructions_text: &str,
    posts_response_body: bool,
) -> String {
    let action = if posts_response_body {
        // Discord: 応答本文がそのまま投稿される。
        "取り組むことがあれば、この応答がそのままチャンネルへ投稿されるので、これから何をするかを自分の言葉で短く添えたうえで、実作業は spawn_subtask で起動してください。"
    } else {
        // ブロードキャスト（Nostr）: 応答本文は投稿されない。投稿はツールで行う。
        "取り組むことがあれば、投稿は投稿ツール（nostr_post 等）で自分から行い、実作業は spawn_subtask で起動してください。この応答の本文はチャンネルへは投稿されません。"
    };
    format!(
        "[ハートビート] 現在の会話「{channel_name}」。{instructions_text}\nいまはハートビートの時間です。{action}いま何もすることが無ければ、通常のターンと同じく NO_REPLY とだけ答えてください。"
    )
}

/// 発火元 transport の `GatewayActions` を登録簿から引く（#588 single-entry）。
///
/// 通常ターンと同じツール環境を渡すため。稼働していなければ `None`（それでも `spawn_subtask` は
/// `SystemGatewayActions` 経由で使える）。Nostr なら `nostr_post` 等が載る。
fn resolve_heartbeat_gateway_actions(
    gateways: &opencrab_actions::AgentGatewayRegistry,
    target: &FireTarget,
    agent_id: &str,
) -> Option<Arc<dyn GatewayActions>> {
    let kind = match target {
        FireTarget::NostrBroadcast => opencrab_actions::gateway_kinds::NOSTR,
        FireTarget::DiscordChannel { .. } => opencrab_actions::gateway_kinds::DISCORD,
    };
    gateways.get(kind)?.gateway_actions_for(agent_id)
}

/// Discord 発火の `on_response_text`（通常ループと同じく反復ごとに応答テキストを配送する）。
///
/// **なぜ scheduler 側に配線が要るか**: 通常の Discord メッセージループは自分の
/// `on_response_text` で反復ごとに `DiscordGateway::send_to_channel` している。scheduler は
/// **どのゲートウェイのメッセージループにも属さず**発火するため、その `send_to_channel` を持たない。
/// メッセージループ外からチャンネルへ送る唯一の口が [`crate::heartbeat_delivery`]（#400 の
/// per-agent ハンドル解決）で、ここを engine 標準の `on_response_text` から呼ぶ。`NO_REPLY`・空は
/// 送らない（通常ループと同じ判定）。
fn discord_response_text_cb(
    discord_http: &Arc<HeartbeatDiscordHttp>,
    agent_id: &str,
    channel_id: &str,
) -> Arc<dyn Fn(String) + Send + Sync> {
    let discord_http = discord_http.clone();
    let agent_id = agent_id.to_string();
    let channel_id = channel_id.to_string();
    Arc::new(move |text: String| {
        if text.is_empty() || text.trim() == "NO_REPLY" {
            return;
        }
        let discord_http = discord_http.clone();
        let agent_id = agent_id.clone();
        let channel_id = channel_id.clone();
        // 発火・反復を塞がないよう spawn（通常ループの on_response_text と同じく非ブロック）。
        tokio::spawn(async move {
            crate::heartbeat_delivery::deliver_via_discord_shared_http(
                &discord_http,
                &agent_id,
                &channel_id,
                &text,
            )
            .await;
        });
    })
}

/// 発火の記録（`heartbeat_log`）。**これと「時間のトリガー＋渡すプロンプト」だけがハートビート
/// 固有**（single-entry の裁定）。`decision` は廃止語彙のため固定値 `fired`（列定義に経緯を明記）。
fn record_heartbeat_fire(
    db: &opencrab_db::Db,
    agent_id: &str,
    channel_id: &str,
    source: &str,
    continuation: Option<&SubtaskContinuation>,
) {
    let Ok(conn) = db.lock() else {
        return;
    };
    let mut result = serde_json::json!({ "channel_id": channel_id, "source": source });
    // 継続ターンだけ発火元を添える（どの発火が subtask 決着由来かを後から切り分けられるように）。
    if let (Some(c), Some(obj)) = (continuation, result.as_object_mut()) {
        obj.insert("origin".to_string(), serde_json::json!("subtask_resume"));
        obj.insert("subtask_id".to_string(), serde_json::json!(c.subtask_id));
        obj.insert("exit_reason".to_string(), serde_json::json!(c.exit_reason));
    }
    if let Err(e) = opencrab_db::queries::insert_heartbeat_log(
        &conn,
        agent_id,
        "fired",
        Some(&result.to_string()),
    ) {
        tracing::error!(agent_id, "scheduler: heartbeat_log の記録に失敗: {e}");
    }
}

/// ハートビートが dispatch したサブタスクの決着（#440）。継続ターンの前置に使う。
#[derive(Clone, Debug)]
struct SubtaskContinuation {
    subtask_id: String,
    exit_reason: String,
}

/// `exit_reason` を人へ見せる述部へ写す（#443）。`SettleKind::Completed` は completed /
/// stopped_by_limit / error / timeout の**どれでも**発火するので、一律「完了」と言うと
/// タイムアウトした subtask にも「完了」と伝わり同じ prompt 内のマーカーと食い違う。未知の値は
/// 断定しない（正確な値は同 prompt の `[subtask_completed: … exit_reason=…]` が持つ）。
fn settle_sentence(exit_reason: &str) -> &'static str {
    match exit_reason {
        "completed" => "完了しました",
        "stopped_by_limit" => "反復上限に達して途中で打ち切られました",
        "error" => "エラーで失敗しました",
        "timeout" => "時間切れで打ち切られました",
        _ => "終了しました",
    }
}

/// 継続ターンの system プロンプト前置（#440/#443）。サブタスクの完了本文は `settle_completed` が
/// 親セッションへ永続化済みで会話再構成で載るため、ここでは本文を載せずマーカーだけ足す。
fn continuation_prompt_suffix(c: &SubtaskContinuation) -> String {
    let outcome = settle_sentence(&c.exit_reason);
    format!(
        "[ハートビート] 依頼していたバックグラウンド処理が{outcome}。詳細は直前の会話ログの subtask_completed に入っています。\n\
         状況を見て、いま発話するかどうかは自分で決めてください。\n\
         [subtask_completed: subtask_id={}, exit_reason={}]",
        c.subtask_id, c.exit_reason
    )
}

/// 決着イベントから継続の有無を決める（純粋関数・旧 `resume_origin`）。
///
/// 継続しない（`None`）のは 2 つ: 決着以外（進捗通知は走行中の run へ二重応答してしまう）／
/// 親セッションがこの sink を配線したターンのものでない。sink は [`RunRequest::with_dispatch`] で
/// **その run が dispatch した subtask にのみ**配線されるので、主判定は `ev.session_id == 発火セッション`
/// で足りる（`heartbeat-` 接頭辞には依存しない・#573 Stage A。統合後は実会話セッションで走る）。
fn resume_continuation(own_session_id: &str, ev: &SubtaskSettled) -> Option<SubtaskContinuation> {
    if ev.kind != SettleKind::Completed {
        return None;
    }
    if ev.session_id != own_session_id {
        return None;
    }
    Some(SubtaskContinuation {
        subtask_id: ev.subtask_id.clone(),
        exit_reason: ev.exit_reason.clone(),
    })
}

/// ハートビートが dispatch したサブタスクの決着を受け、**継続ターン**を起動する sink（#440）。
///
/// `run_one_heartbeat` が `with_dispatch` で配線する。決着が来たら（自分の発火セッションの完了なら）
/// `tokio::spawn` して、共有ロックの下で同じ発火先の継続ターンを 1 回走らせる。**spawn する**のは、
/// この sink が `settle_completed` の途中で同期的に呼ばれるため、ここで待つとサブタスクの決着処理
/// そのものが止まるから（Discord / Nostr / web の sink が resume を spawn するのと同じ）。ロックの
/// 取得は spawn した中で行う（元の run のロックは dispatch が非ブロックのため既に解放されている）。
struct HeartbeatContinuationSink {
    state: AppState,
    discord_http: Arc<HeartbeatDiscordHttp>,
    agent_id: String,
    target: FireTarget,
    session_id: String,
}

impl SubtaskCompletionSink for HeartbeatContinuationSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        let Some(cont) = resume_continuation(&self.session_id, &ev) else {
            return;
        };
        tracing::info!(
            agent_id = %self.agent_id,
            session_id = %self.session_id,
            subtask_id = %ev.subtask_id,
            exit_reason = %ev.exit_reason,
            "heartbeat: subtask settled; starting continuation turn"
        );
        let state = self.state.clone();
        let discord_http = self.discord_http.clone();
        let agent_id = self.agent_id.clone();
        let target = self.target.clone();
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            // 継続ターンも通常ターン・時間発火と同じ共有ロックで直列化する（#588 Stage 2）。
            state
                .session_locks
                .run_serialized(
                    &session_id,
                    run_one_heartbeat(&state, &discord_http, &agent_id, &target, Some(cont)),
                )
                .await;
        });
    }
}

/// scheduler 発火の**共通文脈組み立て**。heartbeat tick（[`run_one_heartbeat`]）と #455 schedule
/// （[`run_one_schedule`]）で**完全一致していた処理**——LLM 確認 → `build_agent_context`（caller=Owner）
/// → モデル解決 → 予算計算 → `build_conversation_string` → `prepend_runtime_context`——を 1 箇所に
/// 集約する（重複解消）。`system_suffix` が非空なら system プロンプトへ足す（heartbeat の指示文。
/// schedule は `""`＝入力は呼び出し側が会話へ speech 注入済み・#501）。
///
/// 戻り値 `(system_prompt, agent_name, conversation)`。`None` = **ターンを開始できない**
/// （LLM 無し / 会話組み立て失敗）→ 呼び出し側は発火扱いしない。
///
/// この関数の後は呼び出し側ごとに違うので共通化しない: RunRequest の transport 差分
/// （heartbeat は gateway_actions / reply_target / `on_response_text` 配送、schedule は素通し）と、
/// 記録（heartbeat は Discord 配送記録＋発火ログ、schedule は plain speech・engine 失敗時の扱いも別）。
/// `run_agent_response` の呼び出し自体は 1 行だが、渡す req も結果処理も別物なので各所に置く。
fn build_scheduled_context(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    runtime_context: &str,
    system_suffix: &str,
) -> Option<(String, String, String)> {
    // LLM が無ければ開始できない（backoff。set/CRUD/次ウェイクで再試行）。
    if state.llm_router.get().provider_names().is_empty() {
        tracing::warn!(
            agent_id,
            session_id,
            "scheduler: LLM provider が無く発火を skip"
        );
        return None;
    }
    let conn = state.db.lock().ok()?;
    let (base_prompt, agent_name) =
        opencrab_server::process::build_agent_context(&conn, agent_id, &CallerIdentity::Owner);
    let eff =
        opencrab_db::queries::effective_model_for_agent(&conn, agent_id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone());
    let (prov, mdl) = opencrab_server::process::split_llm_model_spec(&eff);
    let budget =
        opencrab_server::process::compute_context_budget(&conn, prov, mdl, state.compaction_ratio);
    let raw = match opencrab_server::process::build_conversation_string(
        &conn, session_id, agent_id, budget,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                agent_id,
                session_id,
                "scheduler: 会話文字列の組み立てに失敗: {e}"
            );
            return None;
        }
    };
    let conversation = opencrab_server::process::prepend_runtime_context(&raw, runtime_context);
    // プロンプトの入れ方の差分: system プロンプトへ足す入力（空なら足さない）。
    let system_prompt = if system_suffix.is_empty() {
        base_prompt
    } else {
        format!("{base_prompt}\n\n{system_suffix}")
    };
    Some((system_prompt, agent_name, conversation))
}

/// 1 発火分（heartbeat）: 時刻が来たら、**通常ルートと同じ 1 ターン**を発火先セッション上で走らせる。
///
/// # 「トリガー＋プロンプト」だけがハートビート固有（#588 single-entry）
///
/// ハートビートに固有なのは (1) 時間で発火すること、(2) 渡すプロンプト（指示解決 #584 →
/// [`format_heartbeat_prompt`]）、(3) 発火の記録（[`record_heartbeat_fire`]）の 3 点だけ。それ以外は
/// **通常ルートのものをそのまま使う**: 推論は [`opencrab_server::process::run_agent_response`]、会話
/// 組み立ては [`build_conversation_string`]、配送は Discord なら engine 標準の `on_response_text`
/// （[`discord_response_text_cb`]）、記録は [`opencrab_server::transcript::record_outbound_reply`]、
/// 直列化は呼び出し側が共有 [`SessionLocks`] で巻く（#588 Stage 2）。専用のターン実装・専用の配送
/// ルーティング・専用の語彙（旧 `heartbeat_turn.rs`）は撤去したが、**継続ターン（#440）は残す**。
///
/// caller は **常に `Owner`**（本人が自分の意思で動くターン。`resolve_caller` には載せない）。
/// プロンプトは **system プロンプトへ 1 度だけ**載せ、会話ログには「発言」として残さない（#501。
/// 毎 tick 積まれると挙動を歪めるため）。
///
/// **継続ターン（#440/#443）**: `with_dispatch` で非ブロック dispatch と [`HeartbeatContinuationSink`]
/// を配線する。dispatch したサブタスクが決着すると、sink が同じ発火先で継続ターンを 1 回起動し
/// （`continuation = Some`）、その結果（例: 見つけたニュース）をエージェントが会話へ出せる。継続で
/// なければ `continuation = None`。継続ターンがないと、サブタスクの成果は会話ログに残るだけで
/// チャンネルへ届かない（本番で確認・#440 の要点）。
///
/// `None` = ターンを開始できなかった（文脈組み立て失敗）で、呼び出し側は last_fired を刻まない。
async fn run_one_heartbeat(
    state: &AppState,
    discord_http: &Arc<HeartbeatDiscordHttp>,
    agent_id: &str,
    target: &FireTarget,
    continuation: Option<SubtaskContinuation>,
) -> Option<()> {
    let db = &state.db;
    let is_discord = matches!(target, FireTarget::DiscordChannel { .. });
    // 発火先セッションと、表示用の channel_id / channel_name を種別から解く（#508）。
    let session_id = target.channel_session_id(agent_id);
    let (channel_id, channel_name) = match target {
        FireTarget::NostrBroadcast => (
            String::new(),
            crate::HEARTBEAT_NOSTR_CHANNEL_LABEL.to_string(),
        ),
        FireTarget::DiscordChannel { channel_id, .. } => {
            let name = {
                let conn = db.lock().ok()?;
                opencrab_db::queries::get_channel_config_for_agent(&conn, channel_id, agent_id)
                    .ok()
                    .flatten()
                    .map(|r| r.channel_name)
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| channel_id.clone())
            };
            (channel_id.clone(), name)
        }
    };

    // 前処理（ハートビート固有）: 指示解決（#584: channel → agent → default）→ 整形。応答本文を
    // 自動配送するのは Discord だけ（Nostr はツール投稿）なので、誘導も transport で変える。
    let (mut system_suffix, instructions_source) = {
        let conn = db.lock().ok()?;
        let resolved =
            opencrab_db::queries::resolve_heartbeat_instructions(&conn, agent_id, &channel_id);
        (
            format_heartbeat_prompt(&channel_name, &resolved.text, is_discord),
            resolved.source,
        )
    };
    // 継続ターン（#440）は指示文の後ろへ subtask 完了マーカーを足す（本文は会話再構成で載る）。
    if let Some(c) = &continuation {
        system_suffix = format!("{system_suffix}\n\n{}", continuation_prompt_suffix(c));
    }

    // 共通文脈（LLM 確認・build_agent_context・会話組み立て）。開始できなければ発火扱いしない。
    let (system_prompt, agent_name, conversation) = build_scheduled_context(
        state,
        agent_id,
        &session_id,
        "ハートビート自律行動",
        &system_suffix,
    )?;

    // RunRequest（heartbeat 固有の transport 配線）。Discord のときだけツール域＋既定宛先＋反復ごと
    // 配送（通常ループと同じ on_response_text）。Nostr は自動配送しない（ツール投稿）。
    let mut req = RunRequest::new(
        agent_id,
        &agent_name,
        &session_id,
        &system_prompt,
        &conversation,
        "heartbeat",
        CallerIdentity::Owner,
    );
    if let Some(ga) = resolve_heartbeat_gateway_actions(&state.gateways, target, agent_id) {
        req = req.with_gateway_actions(ga);
    }
    if is_discord {
        req = req
            .with_reply_target(channel_id.clone())
            .with_on_response_text(discord_response_text_cb(
                discord_http,
                agent_id,
                &channel_id,
            ));
    }
    // 継続ターン（#440）: 非ブロック dispatch と継続 sink を配線する。dispatch した subtask が
    // 決着すると sink が同じ発火先で継続ターンを 1 回起動する（registry は agent 単位で共有し、
    // 後続 tick / 継続から cancel_subtask が引けるようにする）。
    let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(HeartbeatContinuationSink {
        state: state.clone(),
        discord_http: discord_http.clone(),
        agent_id: agent_id.to_string(),
        target: target.clone(),
        session_id: session_id.clone(),
    });
    req = req.with_dispatch(Some(state.subtask_registries.registry_for(agent_id)), sink);
    let engine_result = opencrab_server::process::run_agent_response(state, req).await;

    // 記録（heartbeat 固有）。Discord: 最終応答が NO_REPLY 以外なら record_outbound_reply。中間の宣言は
    // engine の turn ログ（on_tool_call）が speech で残す（通常ループと同じ）。Nostr は自動配送しないので
    // 配送記録も残さない。engine 失敗でも「発火した」ことは heartbeat_log に残す。
    match &engine_result {
        Ok(result) => {
            if is_discord {
                let text = result.response.trim();
                if !text.is_empty() && text != "NO_REPLY" {
                    // 通常ループと同じ `triggered_by`: 継続ターンは subtask 完了、tick は直接応答。
                    let context = if continuation.is_some() {
                        AgentReplyContext::SubtaskCompleted
                    } else {
                        AgentReplyContext::Direct {
                            tool_calls_made: result.tool_calls_made,
                        }
                    };
                    if let Ok(conn) = db.lock() {
                        opencrab_server::transcript::record_outbound_reply(
                            &conn,
                            TranscriptSource::Discord,
                            &OutboundReplyRecord {
                                agent_id,
                                session_id: &session_id,
                                channel_id: Some(channel_id.as_str()),
                                text,
                                context: Some(context),
                            },
                        );
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(agent_id, session_id = %session_id, "scheduler: heartbeat ターンが失敗: {e}");
        }
    }

    // 発火の記録（ハートビート固有の 3 点目）。engine の成否によらず「発火した」ことを残す。
    // 継続ターンだけ発火元（subtask 決着）を添える（tick 行の形は不変）。
    record_heartbeat_fire(
        db,
        agent_id,
        &channel_id,
        instructions_source,
        continuation.as_ref(),
    );
    Some(())
}

/// 1 発火分（#455 schedule）: `message` を対象セッションへ注入し、通常メッセージ処理経路で
/// 1 ターン走らせる（設計 §7.3・統括裁定）。**caller=Owner**（HB tick と同じ自己実行）。
///
/// [`run_one_heartbeat`] と同じく `run_agent_response` を直呼びする scheduler 発火だが、#455 は
/// **応答を生成・記録するが自動配送はしない**——外界への出力はエージェントが自分のツール域で
/// 行う（#458 intake と同型・外部作用面は設計 §10.1）。ハートビートが Discord へ応答テキストを
/// 配送する（[`run_one_heartbeat`]）のと違い、schedule はチャンネルへ自動投稿しない。
///
/// 直列化は呼び出し側が [`opencrab_actions::SessionLocks::run_serialized`] で行う（同一セッションの
/// schedule 同士を直列化）。`None` = ターンを開始できなかった（→ 呼び出し側で backoff）。
async fn run_one_schedule(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    message: &str,
) -> Option<()> {
    let db = &state.db;

    // LLM が無ければ **speech 注入もせず**諦める（backoff）。`build_scheduled_context` も後で確認するが、
    // 注入は文脈組み立ての前に済ませる必要があるため、LLM 無しの間 `message` が会話へ積み上がらない
    // よう注入前にも見る（heartbeat は speech 注入をしないのでこのガードは schedule 固有）。
    if state.llm_router.get().provider_names().is_empty() {
        tracing::warn!(
            agent_id,
            session_id,
            "scheduler: LLM provider が無く schedule 発火を skip"
        );
        return None;
    }

    // 前処理（schedule 固有）1: 注入先セッションを用意（無ければ作る。`send_agent_message` と同型）。
    {
        let conn = db.lock().ok()?;
        let existing = opencrab_db::queries::get_session(&conn, session_id)
            .ok()
            .flatten();
        if existing.is_none() {
            let session = opencrab_db::queries::SessionRow {
                id: session_id.to_string(),
                mode: "autonomous".to_string(),
                theme: "direct_message".to_string(),
                phase: "divergent".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: serde_json::json!([agent_id]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            };
            if let Err(e) = opencrab_db::queries::insert_session(&conn, &session) {
                tracing::error!(
                    agent_id,
                    session_id,
                    "scheduler: schedule セッション作成に失敗: {e}"
                );
                return None;
            }
        }
    }

    // 前処理（schedule 固有）2: `message` を **speech**（speaker=`schedule`・≠ agent_id）として注入する。
    // `send_agent_message`（REST）と同じ形にするのが要点: `is_user_speech`（log_type=="speech"
    // かつ speaker!=agent_id・#284）が「エージェントが応答すべき直近のユーザー発言」として認識し、
    // コンテキスト切り詰め後も会話へ混ぜ戻す（`system` で注入すると truncation で落ちて発火しても
    // 届かないことがある）。ハートビートの指示文と違い毎回内容が変わるので会話へ積んでよい（#501）。
    {
        let conn = db.lock().ok()?;
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: message.to_string(),
            speaker_id: Some("schedule".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            tracing::error!(
                agent_id,
                session_id,
                "scheduler: schedule メッセージの注入に失敗: {e}"
            );
            return None;
        }
    }

    // 共通文脈。schedule は system プロンプトへ足さず（入力は上で会話へ speech 注入済み）。
    let (system_prompt, agent_name, conversation) =
        build_scheduled_context(state, agent_id, session_id, "direct_message", "")?;

    // RunRequest（schedule 固有）。gateway_actions / dispatch / 配送は付けない（#458 intake と同型・
    // 自動配送しない。外界への出力はエージェントが自分のツール域で行う）。engine 失敗なら backoff。
    let req = RunRequest::new(
        agent_id,
        &agent_name,
        session_id,
        &system_prompt,
        &conversation,
        "schedule",
        CallerIdentity::Owner,
    );
    let engine_result = match opencrab_server::process::run_agent_response(state, req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                agent_id,
                session_id,
                "scheduler: schedule 発火ターンが失敗: {e}"
            );
            return None;
        }
    };

    // 後処理（schedule 固有）: 応答をセッションへ記録する（次ターンが文脈を失わないように・監査痕跡）。
    if let Ok(conn) = db.lock() {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: "speech".to_string(),
            content: engine_result.response.clone(),
            speaker_id: Some(agent_id.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            tracing::error!(
                agent_id,
                session_id,
                "scheduler: schedule 応答の記録に失敗: {e}"
            );
        }
    }
    Some(())
}

/// 中央スケジューラの本体ループ。プロセス寿命で回り続ける（設計 §3.2）。
///
/// - `config_rx`: hot-reload が push する `HeartbeatConfig`。**発火時に `.borrow().enabled`
///   を読んで live G を得る**（起動時スナップショットにしない・§4.2 の kill-switch ライブ性）。
/// - `wake`: [`AppState::scheduler_wake`]。set/CRUD/global 変更/ターン完了で rebuild を促す。
pub(crate) async fn run_scheduler(
    state: AppState,
    // メッセージループ外からチャンネルへ送るための Discord ハンドル（#400）。ハートビートは
    // どのゲートウェイのループにも属さず発火するため、通常の `send_to_channel` を持てない。
    // これが「通常ルートに無いが必要」な唯一の依存（`run_one_heartbeat` の doc）。
    heartbeat_discord_http: Arc<HeartbeatDiscordHttp>,
    config_rx: watch::Receiver<HeartbeatConfig>,
) {
    let wake = state.scheduler_wake.clone();
    let db = state.db.clone();
    let default_interval_secs = state.heartbeat_limits.default_interval_secs;
    let min_interval_secs = state.heartbeat_limits.min_interval_secs;
    // #588 Stage 2: schedule ターン・HB tick・通常メッセージ処理ターンを同一セッション上で
    // 直列化するため、プロセス全体で 1 つの共有 `SessionLocks`（`AppState::session_locks`）を使う。
    // 以前は schedule 専用のローカルインスタンスで、HB は別 locks、通常ターンは各ゲートウェイの
    // 別 locks だったため、同じ session_id でも相互排他しなかった。HB tick も schedule と同じく
    // ここで `run_serialized` に載せる（`run_one_heartbeat`）。
    let session_locks = state.session_locks.clone();

    // config 変更を wake へ橋渡しする（変更で rebuild → live G を読み直す・#437(c)）。
    // 別 receiver clone を消費し、本体ループは `borrow()` で live 値を読むだけにする
    // （`changed()` の Err で本体がスピンしないよう分離）。
    {
        let mut bridge_rx = config_rx.clone();
        let bridge_wake = wake.clone();
        tokio::spawn(async move {
            while bridge_rx.changed().await.is_ok() {
                bridge_wake.notify_one();
            }
        });
    }

    let in_flight: Arc<Mutex<HashSet<EntryKey>>> = Arc::new(Mutex::new(HashSet::new()));
    // 異常終了（panic / 文脈失敗）の last_attempt（メモリ・§3.7 N-a）。成功で除去。
    let attempts: Arc<Mutex<HashMap<EntryKey, DateTime<Utc>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tracing::info!("中央スケジューラを開始（heartbeat + agent_schedules）");

    loop {
        // live G は発火のたびに hot-reload 後の現在値を読む（起動時スナップにしない）。
        let live_g = config_rx.borrow().enabled;
        let now = Utc::now();

        let entries = {
            let Ok(conn) = db.lock() else {
                tracing::error!("scheduler: db lock 取得に失敗。MAX_SLEEP 眠って再試行");
                sleep_or_wake(MAX_SLEEP_SECS, &wake).await;
                continue;
            };
            let attempts_snapshot = attempts.lock().unwrap();
            rebuild_entries(
                &conn,
                live_g,
                default_interval_secs,
                min_interval_secs,
                &attempts_snapshot,
            )
        };

        // due（next_fire <= now もしくは起点なし）かつ非 in-flight を発火する。
        for entry in &entries {
            let is_due = entry.next_fire_at.map(|t| t <= now).unwrap_or(true);
            if !is_due {
                continue;
            }
            // check-and-insert（本体ループは単一タスクなのでこの間に割り込みは無い。
            // spawn した発火の Drop ガードだけが remove する）。既に走行中なら skip し、
            // last_fired を進めない（§6 N2: 虚偽時刻を出さない）。
            {
                let mut set = in_flight.lock().unwrap();
                if set.contains(&entry.key) {
                    tracing::info!(
                        agent_id = %entry.agent_id,
                        session_id = %entry.session_id,
                        key = ?entry.key,
                        "scheduler skip: 前回発火がまだ走行中"
                    );
                    continue;
                }
                set.insert(entry.key.clone());
            }
            // 異常終了の backoff 起点を spawn 時に打つ（panic で success 印が付かなくても
            // next_fire = attempt + interval になり再発火ループを止める・§3.7 N-a）。成功で除去。
            attempts.lock().unwrap().insert(entry.key.clone(), now);

            let entry = entry.clone();
            let heartbeat_discord_http = heartbeat_discord_http.clone();
            let state = state.clone();
            let db = db.clone();
            let in_flight = in_flight.clone();
            let attempts = attempts.clone();
            let wake = wake.clone();
            let session_locks = session_locks.clone();
            tokio::spawn(async move {
                // 完了（成功/失敗/パニック）で in-flight 除去 + wake（Drop ガード）。
                let _guard = InFlightGuard {
                    key: entry.key.clone(),
                    in_flight,
                    wake,
                };
                // 種別ごとに発火し、成功時だけ truthful に last_fired を刻む。
                let fired: bool = match &entry.kind {
                    FireKind::Heartbeat { target } => {
                        // 同一セッションのターン（HB tick・schedule・通常メッセージ処理）を共有
                        // ロックで直列化して発火する（#588 Stage 2。通常ルートと同じロック）。
                        let outcome = session_locks
                            .run_serialized(
                                &entry.session_id,
                                run_one_heartbeat(
                                    &state,
                                    &heartbeat_discord_http,
                                    &entry.agent_id,
                                    target,
                                    None,
                                ),
                            )
                            .await;
                        if outcome.is_some() {
                            let fired_at = Utc::now().to_rfc3339();
                            if let Ok(conn) = db.lock() {
                                if let Err(e) = opencrab_db::queries::set_session_last_fired(
                                    &conn,
                                    &entry.agent_id,
                                    &entry.session_id,
                                    &fired_at,
                                ) {
                                    tracing::error!(
                                        agent_id = %entry.agent_id,
                                        session_id = %entry.session_id,
                                        "scheduler: last_fired_at の更新に失敗: {e}"
                                    );
                                }
                            }
                            true
                        } else {
                            false
                        }
                    }
                    FireKind::ScheduledMessage { message } => {
                        let EntryKey::Schedule { schedule_id } = &entry.key else {
                            // ScheduledMessage は必ず Schedule キー。
                            return;
                        };
                        let schedule_id = *schedule_id;
                        tracing::info!(
                            agent_id = %entry.agent_id,
                            session_id = %entry.session_id,
                            schedule_id,
                            "scheduler: 定時実行を発火（#455）"
                        );
                        // 同一セッションのターン（schedule 同士・HB tick・通常メッセージ処理）を
                        // 共有ロックで直列化して発火する（#588 Stage 2）。
                        let outcome = session_locks
                            .run_serialized(
                                &entry.session_id,
                                run_one_schedule(
                                    &state,
                                    &entry.agent_id,
                                    &entry.session_id,
                                    message,
                                ),
                            )
                            .await;
                        if outcome.is_some() {
                            let fired_at = Utc::now().to_rfc3339();
                            if let Ok(conn) = db.lock() {
                                if let Err(e) = opencrab_db::queries::set_agent_schedule_last_fired(
                                    &conn,
                                    schedule_id,
                                    &fired_at,
                                ) {
                                    tracing::error!(
                                        schedule_id,
                                        "scheduler: schedule last_fired_at の更新に失敗: {e}"
                                    );
                                }
                            }
                            true
                        } else {
                            false
                        }
                    }
                };

                if fired {
                    // 正常発火: attempt を除去（次回 base は truthful な last_fired へ戻る）。
                    // missed-run は base が最新の last_fired になるため 1 回に圧縮される（§8）。
                    attempts.lock().unwrap().remove(&entry.key);
                } else {
                    // 発火できず（文脈失敗 / ターン失敗）。last_fired は刻まない。spawn 時に打った
                    // attempt が残り、interval ぶん backoff して即再試行ループを避ける（§3.7 N-a）。
                    tracing::debug!(
                        agent_id = %entry.agent_id,
                        session_id = %entry.session_id,
                        "scheduler: 発火ターンを開始/完了できず（backoff）"
                    );
                }
                // ここで _guard が drop され、in-flight 除去 + wake。
            });
        }

        // 次に眠る先を決める（A1: in-flight を除外）。走行中エントリは完了 wake で拾うので
        // 候補に入れず、sleep(0) スピンを避ける。
        let sleep_secs = {
            let set = in_flight.lock().unwrap();
            next_sleep_secs(&entries, &set, now)
        };
        sleep_or_wake(sleep_secs, &wake).await;
    }
}

/// 次に眠る秒数を決める純粋関数（設計 §3.2 A1・ビジーループ回避の核心）。
///
/// **in-flight エントリを候補から除外する**（走行中エントリの `next_fire` は `<= now` に
/// 貼り付くが、完了まで `last_fired` を進めない〔N2〕ので、含めると sleep(0) スピンになる）。
/// 除外した上で「未来（`> now`）の最小 next_fire」まで眠る。候補が無ければ `MAX_SLEEP`
/// （全て in-flight / 起点なしで due 済みの状態でも 0 秒スピンしない）。上限は `MAX_SLEEP`
/// で頭打ち（NTP ジャンプ・DST・notify 取りこぼしの安全網。判定は再ループで `<= now` 再評価）。
fn next_sleep_secs(entries: &[Entry], in_flight: &HashSet<EntryKey>, now: DateTime<Utc>) -> u64 {
    let next = entries
        .iter()
        .filter(|e| !in_flight.contains(&e.key))
        .filter_map(|e| e.next_fire_at)
        .filter(|t| *t > now)
        .min();
    match next {
        Some(t) => (t - now).num_seconds().clamp(0, MAX_SLEEP_SECS as i64) as u64,
        None => MAX_SLEEP_SECS,
    }
}

/// `sleep_secs` 秒眠るか、`wake` が鳴るまで待つ（どちらか早い方）。
async fn sleep_or_wake(sleep_secs: u64, wake: &tokio::sync::Notify) {
    tokio::select! {
        _ = tokio::time::sleep(tokio::time::Duration::from_secs(sleep_secs)) => {}
        _ = wake.notified() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_UUID: &str = "6b79ac3a-7f17-4618-a827-5bda992a3698";

    fn settled(session_id: &str, kind: SettleKind, exit_reason: &str) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-a".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: exit_reason.to_string(),
            kind,
            reply_target: None,
            caller: CallerIdentity::Agent,
        }
    }

    /// #440: 自分の発火セッションの**完了** subtask だけが継続ターンを起こす。
    #[test]
    fn resume_continuation_starts_only_for_own_completed_subtask() {
        // 自分のセッションの完了 → 継続する。
        let c = resume_continuation(
            "nostr-agent-a",
            &settled("nostr-agent-a", SettleKind::Completed, "completed"),
        );
        assert!(
            matches!(c, Some(SubtaskContinuation { ref subtask_id, ref exit_reason }) if subtask_id == "st-1" && exit_reason == "completed")
        );
        // 進捗通知では継続しない（走行中の run へ二重応答しない）。
        assert!(resume_continuation(
            "nostr-agent-a",
            &settled("nostr-agent-a", SettleKind::Progress, "completed")
        )
        .is_none());
        // 別セッションの決着は拾わない（横取り防止）。
        assert!(resume_continuation(
            "nostr-agent-a",
            &settled("discord-other-9-9", SettleKind::Completed, "completed")
        )
        .is_none());
    }

    /// #443: 継続の前置は subtask_completed マーカーを持ち、**未完了の決着で「完了」と断言しない**。
    #[test]
    fn continuation_suffix_marks_subtask_and_never_claims_false_completion() {
        let completed = continuation_prompt_suffix(&SubtaskContinuation {
            subtask_id: "st-1".to_string(),
            exit_reason: "completed".to_string(),
        });
        assert!(completed.contains("[subtask_completed: subtask_id=st-1, exit_reason=completed]"));
        assert!(completed.contains("完了しました"));
        // timeout / error は「完了」と言わない（同 prompt 内のマーカーと食い違わせない）。
        for (reason, expected) in [
            ("timeout", "時間切れで打ち切られました"),
            ("error", "エラーで失敗しました"),
            ("stopped_by_limit", "反復上限に達して途中で打ち切られました"),
            ("weird_new_reason", "終了しました"),
        ] {
            let s = continuation_prompt_suffix(&SubtaskContinuation {
                subtask_id: "st-1".to_string(),
                exit_reason: reason.to_string(),
            });
            assert!(
                !s.contains("完了しました"),
                "exit_reason={reason} で完了を断言している: {s}"
            );
            assert!(
                s.contains(expected),
                "exit_reason={reason} の決着が伝わらない: {s}"
            );
            assert!(
                s.contains(&format!("exit_reason={reason}]")),
                "生の exit_reason をマーカーへ載せる: {s}"
            );
        }
    }

    /// #501: 指示文の整形は発火経路で決まる `channel_name` を差し込むだけ。Nostr（ラベル）と
    /// Discord（チャンネル名）で正しい文面になること。
    #[test]
    fn format_heartbeat_prompt_embeds_channel_name_per_fire_path() {
        // NostrBroadcast は `run_one_heartbeat` が HEARTBEAT_NOSTR_CHANNEL_LABEL を channel_name に使う。
        let nostr =
            format_heartbeat_prompt(crate::HEARTBEAT_NOSTR_CHANNEL_LABEL, "巡回してね", false);
        assert!(
            nostr.contains("現在の会話「（自律ハートビート）」。巡回してね"),
            "Nostr 経路の文面が違う: {nostr}"
        );
        // DiscordChannel はチャンネル設定名を channel_name に使う。
        let discord = format_heartbeat_prompt("雑談", "静かにね", true);
        assert!(
            discord.contains("現在の会話「雑談」。静かにね"),
            "Discord 経路の文面が違う: {discord}"
        );
    }

    /// #588 Stage 3: 規約は**通常のターンへ寄せる**。撤去した SPEAK/LEARN/IDLE の語彙が文面に
    /// 残っておらず、沈黙は通常ターンと同じ `NO_REPLY` で表せることを、両 transport で担保する。
    #[test]
    fn format_heartbeat_prompt_uses_no_reply_and_drops_speak_learn_idle() {
        for posts_body in [true, false] {
            let p = format_heartbeat_prompt("雑談", "静かにね", posts_body);
            assert!(
                p.contains("NO_REPLY"),
                "沈黙を通常ターンと同じ NO_REPLY で表す規約が無い（posts_body={posts_body}）: {p}"
            );
            for retired in ["SPEAK", "LEARN", "IDLE"] {
                assert!(
                    !p.contains(retired),
                    "撤去した語彙 {retired} が指示文に残っている（posts_body={posts_body}）: {p}"
                );
            }
            assert!(
                p.contains("spawn_subtask"),
                "実作業をサブタスクで起動する誘導が無い（posts_body={posts_body}）: {p}"
            );
        }
    }

    /// #588 Stage 3（オーナー判断）: 誘導は発火元 transport で変わる。
    /// - Discord: 応答本文がそのまま投稿される旨を伝える。
    /// - ブロードキャスト（Nostr）: 応答本文は投稿されず、投稿はツールで行う旨を伝える。
    ///
    /// どちらも**定型の宣言文をハードコードしない**（issue に 3 回出てくる例示を埋め込まない）。
    #[test]
    fn format_heartbeat_prompt_guidance_varies_by_transport_without_hardcoding_a_declaration() {
        // Discord: 応答本文が投稿される。
        let discord = format_heartbeat_prompt("雑談", "静かにね", true);
        assert!(
            discord.contains("この応答がそのままチャンネルへ投稿される"),
            "Discord は応答本文が投稿される旨を伝えていない: {discord}"
        );
        assert!(
            discord.contains("自分の言葉で"),
            "Discord は宣言をエージェント自身の言葉で書かせていない: {discord}"
        );

        // ブロードキャスト（Nostr）: 応答本文は投稿されない。投稿はツールで。
        let broadcast = format_heartbeat_prompt("（自律ハートビート）", "巡回してね", false);
        assert!(
            broadcast.contains("投稿されません"),
            "ブロードキャストで「応答本文は投稿されない」旨が無い（二重投稿の道を塞ぐ要）: {broadcast}"
        );
        assert!(
            broadcast.contains("投稿ツール"),
            "ブロードキャストで投稿はツールで行う誘導が無い: {broadcast}"
        );
        assert!(
            !broadcast.contains("この応答がそのままチャンネルへ投稿される"),
            "ブロードキャストで自動投稿を示唆している（本文は投稿されないはず）: {broadcast}"
        );

        // どちらも定型の宣言文はハードコードしない。
        for p in [&discord, &broadcast] {
            assert!(
                !p.contains("作業するね"),
                "定型の宣言文がハードコードされている（#588: 例示は実装すべき文字列ではない）: {p}"
            );
        }
    }

    #[test]
    fn resolve_fire_target_nostr() {
        assert_eq!(
            resolve_fire_target(&format!("nostr-{AGENT_UUID}"), AGENT_UUID),
            Some(FireTarget::NostrBroadcast)
        );
    }

    #[test]
    fn resolve_fire_target_discord_strips_uuid_prefix() {
        // agent_id が UUID（ハイフン入り）でも保存済み agent_id で剥がすので割れない。
        let sid = format!("discord-{AGENT_UUID}-1001-2002");
        assert_eq!(
            resolve_fire_target(&sid, AGENT_UUID),
            Some(FireTarget::DiscordChannel {
                guild_id: "1001".to_string(),
                channel_id: "2002".to_string(),
            })
        );
    }

    #[test]
    fn resolve_fire_target_fail_closed_on_unknown_and_nonnumeric() {
        // 発火経路を持たない種別 → None。
        assert_eq!(
            resolve_fire_target(&format!("web-{AGENT_UUID}"), AGENT_UUID),
            None
        );
        assert_eq!(
            resolve_fire_target(&format!("heartbeat-{AGENT_UUID}-2002"), AGENT_UUID),
            None
        );
        // 非数値 guild/channel → None（fail-closed）。
        assert_eq!(
            resolve_fire_target(&format!("discord-{AGENT_UUID}-guild-chan"), AGENT_UUID),
            None
        );
        // 別 agent_id の session を渡しても剥がれない → None。
        assert_eq!(
            resolve_fire_target(&format!("discord-{AGENT_UUID}-1001-2002"), "other-agent"),
            None
        );
    }

    #[test]
    fn later_of_picks_the_later() {
        let a = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let b = DateTime::parse_from_rfc3339("2026-01-01T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(later_of(Some(a), Some(b)), Some(b));
        assert_eq!(later_of(Some(b), Some(a)), Some(b));
        assert_eq!(later_of(Some(a), None), Some(a));
        assert_eq!(later_of(None, Some(b)), Some(b));
        assert_eq!(later_of(None, None), None);
    }

    // ---- rebuild / 発火集合の不変条件（設計 §4.2 / §5・#5） ----

    use chrono::Duration;
    use opencrab_db::queries::{AgentScheduleRow, SessionHeartbeatConfigRow};
    use std::collections::HashMap;

    const AGENT_B: &str = "c56f19e0-1111-2222-3333-444455556666";

    fn hb_key(session: &str) -> EntryKey {
        EntryKey::Heartbeat {
            session_id: session.to_string(),
        }
    }

    fn row(
        agent: &str,
        session: &str,
        enabled: bool,
        interval: Option<i64>,
        anchor: Option<String>,
        last: Option<String>,
    ) -> SessionHeartbeatConfigRow {
        SessionHeartbeatConfigRow {
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            enabled,
            interval_secs: interval,
            anchor_at: anchor,
            last_fired_at: last,
        }
    }

    fn conn_with(rows: &[SessionHeartbeatConfigRow]) -> rusqlite::Connection {
        let conn = opencrab_db::init_memory().unwrap();
        for r in rows {
            opencrab_db::queries::upsert_session_heartbeat_config(&conn, r).unwrap();
        }
        conn
    }

    fn session_ids(entries: &[Entry]) -> std::collections::BTreeSet<String> {
        entries.iter().map(|e| e.session_id.clone()).collect()
    }

    /// 本番の発火集合（Nostr 2 + Discord 1）を模した fixture で、**live G が
    /// `discord-` だけをゲートし `nostr-` は非依存**であることを固定する（不変条件 #5）。
    #[test]
    fn firing_set_gates_discord_by_live_g_only() {
        let past = (Utc::now() - Duration::hours(12)).to_rfc3339();
        let nostr_a = format!("nostr-{AGENT_UUID}");
        let nostr_b = format!("nostr-{AGENT_B}");
        let discord_c = format!("discord-{AGENT_UUID}-1001-2002");
        let rows = [
            row(
                AGENT_UUID,
                &nostr_a,
                true,
                Some(18000),
                Some(past.clone()),
                None,
            ),
            row(
                AGENT_B,
                &nostr_b,
                true,
                Some(1200),
                Some(past.clone()),
                None,
            ),
            row(
                AGENT_UUID,
                &discord_c,
                true,
                Some(10800),
                Some(past.clone()),
                None,
            ),
            // opt-in 抑止で enabled=0 に焼かれた Discord 行（発火しない・list_enabled が返さない）。
            row(
                AGENT_B,
                &format!("discord-{AGENT_B}-1001-3003"),
                false,
                Some(600),
                None,
                None,
            ),
        ];
        let conn = conn_with(&rows);
        let attempts = HashMap::new();

        // G = true: 3 セッションすべて。
        let with_g = rebuild_entries(&conn, true, 1800, 300, &attempts);
        assert_eq!(
            session_ids(&with_g),
            [nostr_a.clone(), nostr_b.clone(), discord_c.clone()]
                .into_iter()
                .collect(),
            "G=true では nostr 2 + discord 1 が発火対象"
        );
        assert!(with_g
            .iter()
            .all(|e| e.next_fire_at.map(|t| t <= Utc::now()).unwrap_or(true)));

        // G = false: discord はゲートで消え、nostr 2 だけ残る（nostr は G 非依存）。
        let without_g = rebuild_entries(&conn, false, 1800, 300, &attempts);
        assert_eq!(
            session_ids(&without_g),
            [nostr_a, nostr_b].into_iter().collect(),
            "G=false では discord- がゲートされ nostr- のみ発火（nostr は G 非依存）"
        );
    }

    /// 壊れた session_id / interval は fail-closed で発火集合に入らない。
    #[test]
    fn rebuild_skips_malformed_and_broken_interval() {
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        let good = format!("nostr-{AGENT_UUID}");
        let rows = [
            row(AGENT_UUID, &good, true, Some(600), Some(past.clone()), None),
            row(
                AGENT_UUID,
                &format!("web-{AGENT_UUID}"),
                true,
                Some(600),
                Some(past.clone()),
                None,
            ),
            row(
                AGENT_B,
                &format!("nostr-{AGENT_B}"),
                true,
                Some(0),
                Some(past.clone()),
                None,
            ),
        ];
        let conn = conn_with(&rows);
        let entries = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert_eq!(session_ids(&entries), [good].into_iter().collect());
    }

    // ---- ビジーループ回避（A1・#6） ----

    fn entry_at(session: &str, next: Option<DateTime<Utc>>) -> Entry {
        Entry {
            key: hb_key(session),
            agent_id: AGENT_UUID.to_string(),
            session_id: session.to_string(),
            next_fire_at: next,
            kind: FireKind::Heartbeat {
                target: FireTarget::NostrBroadcast,
            },
        }
    }

    /// in-flight エントリは sleep 候補から除外され、0 秒スピンしない（A1）。
    #[test]
    fn sleep_excludes_in_flight_and_never_spins_to_zero() {
        let now = Utc::now();
        let mut in_flight = HashSet::new();
        in_flight.insert(hb_key("s-running"));

        let running_future = entry_at("s-running", Some(now + Duration::seconds(10)));
        let idle_future = entry_at("s-future", Some(now + Duration::seconds(120)));
        assert_eq!(
            next_sleep_secs(&[running_future, idle_future], &in_flight, now),
            120,
            "in-flight の未来エントリを除外し、非 in-flight の 120s まで眠る"
        );

        let running_stuck = entry_at("s-running", Some(now - Duration::seconds(30)));
        assert_eq!(
            next_sleep_secs(&[running_stuck], &in_flight, now),
            MAX_SLEEP_SECS,
            "走行中 due だけなら 0 秒スピンせず MAX_SLEEP（完了 wake で拾う）"
        );

        let far = entry_at("s-far", Some(now + Duration::hours(2)));
        assert_eq!(
            next_sleep_secs(&[far], &HashSet::new(), now),
            MAX_SLEEP_SECS
        );
    }

    /// schedule のキーは同一セッションでも別スケジュールを誤ブロックしない（EntryKey enum の要点）。
    #[test]
    fn schedule_keys_do_not_cross_block_same_session() {
        let now = Utc::now();
        let session = format!("nostr-{AGENT_UUID}");
        let mut in_flight = HashSet::new();
        in_flight.insert(EntryKey::Schedule { schedule_id: 1 });

        let running = Entry {
            key: EntryKey::Schedule { schedule_id: 1 },
            agent_id: AGENT_UUID.to_string(),
            session_id: session.clone(),
            next_fire_at: Some(now - Duration::seconds(5)),
            kind: FireKind::ScheduledMessage {
                message: "a".into(),
            },
        };
        // 同一セッションの別スケジュール（id=2）は in_flight に居ないので sleep 候補に残る。
        let sibling = Entry {
            key: EntryKey::Schedule { schedule_id: 2 },
            agent_id: AGENT_UUID.to_string(),
            session_id: session,
            next_fire_at: Some(now + Duration::seconds(90)),
            kind: FireKind::ScheduledMessage {
                message: "b".into(),
            },
        };
        assert_eq!(
            next_sleep_secs(&[running, sibling], &in_flight, now),
            90,
            "id=1 が走行中でも id=2（同一セッション）は別キーなので候補に残る"
        );
    }

    // ---- missed-run 圧縮 / アンカーの向き（§8 / §4.4 / §3.7 N-a） ----

    #[test]
    fn missed_run_compresses_to_one_and_success_pushes_forward() {
        let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
        let sid = format!("nostr-{AGENT_UUID}");
        let conn = conn_with(&[row(AGENT_UUID, &sid, true, Some(600), Some(long_ago), None)]);

        let before = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert_eq!(before.len(), 1);
        assert!(before[0]
            .next_fire_at
            .map(|t| t <= Utc::now())
            .unwrap_or(true));

        let now_str = Utc::now().to_rfc3339();
        opencrab_db::queries::set_session_last_fired(&conn, AGENT_UUID, &sid, &now_str).unwrap();

        let after = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert_eq!(after.len(), 1);
        assert!(
            after[0]
                .next_fire_at
                .map(|t| t > Utc::now())
                .unwrap_or(false),
            "発火成功後は next_fire が未来へ後退する（密にしない）"
        );
    }

    #[test]
    fn last_attempt_backoff_defers_next_fire_without_touching_last_fired() {
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        let sid = format!("nostr-{AGENT_UUID}");
        let conn = conn_with(&[row(AGENT_UUID, &sid, true, Some(600), Some(past), None)]);

        let due = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert!(due[0].next_fire_at.map(|t| t <= Utc::now()).unwrap_or(true));

        let mut attempts = HashMap::new();
        attempts.insert(hb_key(&sid), Utc::now());
        let deferred = rebuild_entries(&conn, true, 1800, 300, &attempts);
        assert!(
            deferred[0]
                .next_fire_at
                .map(|t| t > Utc::now())
                .unwrap_or(false),
            "last_attempt が next_fire を interval ぶん後ろへ逃がす"
        );
    }

    // ---- panic 経路の統合確認（§6 / §3.7 N-a） ----

    #[tokio::test]
    async fn panic_in_fire_clears_in_flight_keeps_attempt_and_wakes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let in_flight: Arc<Mutex<HashSet<EntryKey>>> = Arc::new(Mutex::new(HashSet::new()));
        let attempts: Arc<Mutex<HashMap<EntryKey, DateTime<Utc>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let wake = Arc::new(tokio::sync::Notify::new());
        let key = hb_key(&format!("nostr-{AGENT_UUID}"));

        in_flight.lock().unwrap().insert(key.clone());
        attempts.lock().unwrap().insert(key.clone(), Utc::now());

        let woken = Arc::new(AtomicBool::new(false));
        {
            let w = wake.clone();
            let woken2 = woken.clone();
            tokio::spawn(async move {
                w.notified().await;
                woken2.store(true, Ordering::SeqCst);
            });
        }
        tokio::task::yield_now().await;

        let jh = {
            let in_flight = in_flight.clone();
            let wake = wake.clone();
            let key = key.clone();
            tokio::spawn(async move {
                let _guard = InFlightGuard {
                    key,
                    in_flight,
                    wake,
                };
                panic!("boom: 発火ターン内 panic");
            })
        };
        let res = jh.await;
        assert!(res.is_err(), "spawn した発火ターンは panic するはず");

        assert!(
            !in_flight.lock().unwrap().contains(&key),
            "panic 後も in_flight に残る（Drop ガードが効いていない）"
        );
        assert!(
            attempts.lock().unwrap().contains_key(&key),
            "panic 時に attempt が消える（backoff が効かず即再発火スピンになる）"
        );
        tokio::task::yield_now().await;
        assert!(
            woken.load(Ordering::SeqCst),
            "panic 後の完了 wake が鳴っていない（rebuild が促されない）"
        );
    }

    // ---- #438 回帰: 固定グリッドをやめ設定 interval どおりに発火する ----

    #[test]
    fn next_fire_honors_exact_interval_without_grid_rounding() {
        let anchor = DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let sid = format!("nostr-{AGENT_UUID}");
        let conn = conn_with(&[row(
            AGENT_UUID,
            &sid,
            true,
            Some(1200),
            Some(anchor.to_rfc3339()),
            None,
        )]);
        let entries = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].next_fire_at,
            Some(anchor + Duration::seconds(1200)),
            "次回発火は anchor + 設定 interval(1200s)。1800 グリッドへ丸めない（#438）"
        );
    }

    #[test]
    fn sleep_targets_exact_remaining_not_fixed_grid() {
        let now = Utc::now();
        let e = entry_at("nostr-x", Some(now + Duration::seconds(250)));
        assert_eq!(
            next_sleep_secs(&[e], &HashSet::new(), now),
            250,
            "残り 250 秒ちょうど眠る（固定グリッドへ戻さない・#438）"
        );
    }

    // ---- #455 schedule: rebuild へ載る・G 非依存・cron/@every・missed-run・enabled ----

    fn sched(
        agent: &str,
        session: &str,
        cron: &str,
        enabled: bool,
        anchor: Option<&str>,
        last: Option<&str>,
    ) -> AgentScheduleRow {
        AgentScheduleRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            cron_expr: cron.to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "定時のメッセージ".to_string(),
            enabled,
            anchor_at: anchor.map(|s| s.to_string()),
            last_fired_at: last.map(|s| s.to_string()),
        }
    }

    fn schedule_keys(entries: &[Entry]) -> std::collections::BTreeSet<i64> {
        entries
            .iter()
            .filter_map(|e| match e.key {
                EntryKey::Schedule { schedule_id } => Some(schedule_id),
                _ => None,
            })
            .collect()
    }

    /// enabled な cron / `@every` の両方が rebuild に載る。disabled は載らない（enabled=false で停止）。
    #[test]
    fn schedules_enabled_both_kinds_load_disabled_excluded() {
        let conn = opencrab_db::init_memory().unwrap();
        let sid = format!("nostr-{AGENT_UUID}");
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        // cron（enabled）。
        let id_cron = opencrab_db::queries::insert_agent_schedule(
            &conn,
            &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&past), None),
        )
        .unwrap();
        // @every（enabled）。
        let id_every = opencrab_db::queries::insert_agent_schedule(
            &conn,
            &sched(AGENT_UUID, &sid, "@every 3h", true, Some(&past), None),
        )
        .unwrap();
        // disabled は列挙されない。
        let _id_off = opencrab_db::queries::insert_agent_schedule(
            &conn,
            &sched(AGENT_UUID, &sid, "@every 1h", false, Some(&past), None),
        )
        .unwrap();

        let entries = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert_eq!(
            schedule_keys(&entries),
            [id_cron, id_every].into_iter().collect(),
            "cron と @every の enabled 2 件だけが載る（disabled は停止）"
        );
        // 両種とも next_fire_at が算出されている（cron/@every のどちらの経路も通る）。
        // due か未来かは現在時刻に依存するので、ここでは「算出できたこと」だけを固定する
        // （missed-run の due は schedule_missed_run_compresses_to_one で固定）。
        for e in entries
            .iter()
            .filter(|e| matches!(e.key, EntryKey::Schedule { .. }))
        {
            assert!(
                e.next_fire_at.is_some(),
                "cron/@every とも next_fire_at を算出できる"
            );
        }
    }

    /// schedule は live G の対象外（G=false でも発火対象に残る・統括裁定 §10.1）。
    #[test]
    fn schedules_are_not_gated_by_g() {
        let conn = opencrab_db::init_memory().unwrap();
        // discord- セッションの schedule（HB なら G=false で消える種別）。
        let sid = format!("discord-{AGENT_UUID}-1001-2002");
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        let id = opencrab_db::queries::insert_agent_schedule(
            &conn,
            &sched(AGENT_UUID, &sid, "@every 30m", true, Some(&past), None),
        )
        .unwrap();

        // G=false でも schedule は残る（HB の discord- は消えるが schedule は G 非依存）。
        let entries = rebuild_entries(&conn, false, 1800, 300, &HashMap::new());
        assert_eq!(
            schedule_keys(&entries),
            [id].into_iter().collect(),
            "G=false でも discord- 宛 schedule は発火対象（G は heartbeat のスイッチ）"
        );
    }

    /// 長時間ダウン後の cron schedule も 1 エントリ・due（missed-run 1 回圧縮・§8）。
    #[test]
    fn schedule_missed_run_compresses_to_one() {
        let conn = opencrab_db::init_memory().unwrap();
        let sid = format!("nostr-{AGENT_UUID}");
        let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
        opencrab_db::queries::insert_agent_schedule(
            &conn,
            &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&long_ago), None),
        )
        .unwrap();

        let before = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        let sched_entries: Vec<_> = before
            .iter()
            .filter(|e| matches!(e.key, EntryKey::Schedule { .. }))
            .collect();
        assert_eq!(
            sched_entries.len(),
            1,
            "3 日過ぎても 1 エントリ（多重発火しない）"
        );
        assert!(
            sched_entries[0]
                .next_fire_at
                .map(|t| t <= Utc::now())
                .unwrap_or(true),
            "過ぎたスロットは due（1 回だけ発火）"
        );
    }

    /// 解釈できない cron 式は fail-closed で skip（外部へ影響しない）。
    #[test]
    fn schedule_unparseable_expr_is_skipped() {
        let conn = opencrab_db::init_memory().unwrap();
        let sid = format!("nostr-{AGENT_UUID}");
        opencrab_db::queries::insert_agent_schedule(
            &conn,
            &sched(AGENT_UUID, &sid, "totally not a cron", true, None, None),
        )
        .unwrap();
        let entries = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert!(
            schedule_keys(&entries).is_empty(),
            "解釈不能な式は発火集合に入らない（fail-closed）"
        );
    }

    /// 発火成功で last_fired を刻むと、次回 rebuild で cron の次スロット（未来）へ後退する
    /// （二重実行しない・向きは後ろ・§8 / §4.4）。
    #[test]
    fn schedule_success_pushes_next_forward() {
        let conn = opencrab_db::init_memory().unwrap();
        let sid = format!("nostr-{AGENT_UUID}");
        let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
        let id = opencrab_db::queries::insert_agent_schedule(
            &conn,
            &sched(AGENT_UUID, &sid, "0 7 * * *", true, Some(&long_ago), None),
        )
        .unwrap();

        // 発火成功を模して last_fired=now を刻む。
        opencrab_db::queries::set_agent_schedule_last_fired(&conn, id, &Utc::now().to_rfc3339())
            .unwrap();

        let after = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        let e = after
            .iter()
            .find(|e| matches!(e.key, EntryKey::Schedule { .. }))
            .unwrap();
        assert!(
            e.next_fire_at.map(|t| t > Utc::now()).unwrap_or(false),
            "発火成功後は次スロット（未来）へ後退＝直後の rebuild で再発火しない（二重実行しない）"
        );
    }
}
