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
//!   **通常ルートと同じ 1 ターン**を走らせる（`run_one_heartbeat`。caller=Owner・
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

use opencrab_actions::{CallerIdentity, RunRequest};
// #588 TimedFire の発火本体は lib（`opencrab_server::heartbeat_fire`）へ移し、scheduler（時刻発火）と
// `run_my_heartbeat`（手動発火・#599）が**同じ 1 つの関数**を呼ぶ。テスト用の別経路を作らない。
use opencrab_server::heartbeat_fire::run_one_heartbeat;
use opencrab_server::AppState;

/// sleep の頭打ち（秒）。NTP ジャンプ・DST・notify 取りこぼしの安全網（設計 §3.4）。
/// これで頭打ちしても判定は必ず `<= now` を再評価するので、遅れて拾うだけ。
const MAX_SLEEP_SECS: u64 = 300;

/// セッションの発火先（`session_id` から transport descriptor が解決する・#628）。
///
/// **発火先の知識は各 transport の `TransportFire` descriptor へ移した**（旧 db 層の
/// `SessionFireTarget` enum を撤去）。scheduler は登録簿（[`opencrab_actions::TimedFireRouter`]）へ
/// 問い合わせるだけで、transport の名前も ID 書式も知らない。発火（scheduler）と受理判定・
/// ゲート理由表示（`set_my_heartbeat` / `get_my_heartbeat`）は同じ登録簿を引くので「設定できた
/// のに永遠に発火しない行」ができない（設計 §13.1）。
use opencrab_actions::FireTarget;

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
    /// heartbeat: 発火先セッション上で通常ルートと同じ 1 ターンを走らせる（`run_one_heartbeat`）。
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
    router: &opencrab_actions::TimedFireRouter,
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
                let Some(target) = router.resolve_target(&row.session_id, &row.agent_id) else {
                    // 未知/解釈不能な session_id → 発火しない（壊れた行で外部へ publish しない）。
                    tracing::warn!(
                        agent_id = %row.agent_id,
                        session_id = %row.session_id,
                        "scheduler: 発火先を解決できない session_id を skip（fail-closed）"
                    );
                    continue;
                };
                // G ゲート: G ゲート対象の transport（Discord）は live G が false なら発火しない
                // （§4.2 ランタイム G ゲート）。対象外（Nostr / web）は G 非依存。**whitelist ゲートは
                // 掛けない**（設計 §5 N3・現行経路に無い）。対象か否かは descriptor が名乗る（#628）。
                let g_gated = router
                    .descriptor(target.kind)
                    .map(|d| d.is_g_gated())
                    .unwrap_or(false);
                if g_gated && !live_g {
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

/// #455 schedule 発火（[`run_one_schedule`]）の文脈組み立て。LLM 確認 → `build_agent_context`
/// （caller=Owner）→ モデル解決 → 予算計算 → `build_conversation_string` → `prepend_runtime_context`。
/// `system_suffix` が非空なら system プロンプトへ足す（schedule は `""`＝入力は呼び出し側が会話へ
/// speech 注入済み・#501）。
///
/// #588 TimedFire 以降、heartbeat はこの関数を通らない: 時刻発火はゲートウェイのループへ委譲され、
/// 文脈組み立て・RunRequest・配送・記録は**そのループの通常ルート**が行う。scheduler 側でこの
/// 「発火ターンを自前で回す」形が残るのは schedule（#455）だけ。
///
/// 戻り値 `(system_prompt, agent_name, conversation)`。`None` = **ターンを開始できない**
/// （LLM 無し / 会話組み立て失敗）→ 呼び出し側は発火扱いしない。
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
    let runtime_text = opencrab_server::process::prepend_runtime_context("", runtime_context);
    let functions_tokens = match opencrab_server::process::core_functions_tokens() {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(
                agent_id,
                session_id,
                error_name = e.name(),
                "scheduler: {name}: {e}",
                name = e.name()
            );
            return None;
        }
    };
    let env = match opencrab_server::process::resolve_agent_request_envelope(
        opencrab_server::process::RequestEnvelopeArgs {
            conn: &conn,
            agent_id,
            session_id,
            default_model: &state.default_model,
            policy: &state.context_budget_policy(),
            system_prompt: &base_prompt,
            runtime_context_text: &runtime_text,
            functions_tokens,
            entrypoint: "scheduler",
        },
    ) {
        Ok(env) => env,
        Err(e) => {
            tracing::error!(
                agent_id,
                session_id,
                error_name = e.name(),
                "scheduler: {name}: {e}",
                name = e.name()
            );
            return None;
        }
    };
    let raw = match opencrab_server::process::build_conversation_string_with_waters(
        &conn,
        session_id,
        agent_id,
        env.conversation_high,
        env.conversation_low,
        opencrab_server::process::include_memory_index(&env),
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

/// 1 発火分（#455 schedule）: `message` を対象セッションへ注入し、通常メッセージ処理経路で
/// 1 ターン走らせる（設計 §7.3・統括裁定）。**caller=Owner**（HB tick と同じ自己実行）。
///
/// `run_one_heartbeat` と同じく `run_agent_response` を直呼びする scheduler 発火だが、#455 は
/// **応答を生成・記録するが自動配送はしない**——外界への出力はエージェントが自分のツール域で
/// 行う（#458 intake と同型・外部作用面は設計 §10.1）。ハートビートが Discord へ応答テキストを
/// 配送する（`run_one_heartbeat`）のと違い、schedule はチャンネルへ自動投稿しない。
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
    // #899 §12.6: 保存前に NO_REPLY 終端解釈（単一実装）を通す。沈黙は speech を残さない。
    if let Some(body) = opencrab_actions::visible_speech_after_markers(
        &engine_result.response,
        opencrab_actions::DeliveryContext {
            session_id,
            agent_id,
            origin: "schedule",
        },
    ) {
        if let Ok(conn) = db.lock() {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                log_type: "speech".to_string(),
                content: body,
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
    }
    Some(())
}

/// 中央スケジューラの本体ループ。プロセス寿命で回り続ける（設計 §3.2）。
///
/// - `config_rx`: hot-reload が push する `HeartbeatConfig`。**発火時に `.borrow().enabled`
///   を読んで live G を得る**（起動時スナップショットにしない・§4.2 の kill-switch ライブ性）。
/// - `wake`: [`AppState::scheduler_wake`]。set/CRUD/global 変更/ターン完了で rebuild を促す。
pub(crate) async fn run_scheduler(state: AppState, config_rx: watch::Receiver<HeartbeatConfig>) {
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
                &state.timed_fire_router,
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
                        // #588 TimedFire: 発火先ゲートウェイのループへイベントを 1 本流すだけ
                        // （ロック・配送・記録・継続はループが回す）。scheduler は turn を回さない。
                        let outcome = run_one_heartbeat(&state, &entry.agent_id, target).await;
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
mod tests;
