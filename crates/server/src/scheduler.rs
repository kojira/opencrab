//! 中央スケジューラ（#439 / #437 / #438 / #455 / #612・設計 §3・§7）。
//!
//! # なぜ中央スケジューラか
//!
//! 旧実装は**エージェントごとに** `core::heartbeat::heartbeat_loop` を 1 本立て、固定
//! グリッド（グローバル 1800 秒に丸めた sleep）で目を覚ましていた。位相はメモリ（`Instant`）
//! だけで持ち、再起動で消えて（#439-1）、設定変更はループを張り直すまで効かず（#437）、
//! sleep グリッドと設定間隔が食い違っていた（#438）。
//!
//! ここでは **単一のタスク**が `agent_schedules`（enabled 行）を毎ウェイクで DB から読み直し、
//! **永続アンカー**（`anchor_at`/`last_fired_at`・壁時計）から**正確な次回発火時刻**を算出し、
//! **最も近い次回発火まで眠る**。設定変更・発火ターン完了は `scheduler_wake`
//! （[`AppState::scheduler_wake`]）で起こして rebuild させる（即時反映・#437）。位相は DB に
//! 永続するので再起動で伸びない（#439-1）。
//!
//! # 時間トリガーは 1 種類（#612・設計 §8.1）
//!
//! 間隔（`@every`）も定時（cron）も `agent_schedules` の 1 行で、**発火経路も 1 本**:
//! 時刻が来たら行の `message` をハートビートの枠で包んだプロンプトを TimedFire sink へ投げる
//! （[`run_one_heartbeat`]）。直列化・配送・記録・継続は受け取ったゲートウェイのループが回す。
//! 同じセッションで同時刻に立った行は、それぞれ別のターンとして共有 `SessionLocks` の下で
//! 直列に走る（捨てない・束ねない・順序は保証しない・設計 §8.2）。
//!
//! **キーは行の id**（[`EntryKey::Schedule`]）: 同一セッションに複数行がぶら下がるので、
//! in-flight / attempts を session_id で持つと別の行を誤ブロックする。
//!
//! # ビジーループを作らない（設計 §3.2 A1 / §6）
//!
//! 走行中（in-flight）エントリは (1) sleep の `min` 候補から除外し、(2) 完了で wake する。
//! よって走行中は 0 秒スピンせず、完了した瞬間に rebuild して truthful に刻んだ
//! `last_fired_at` から次回を計算する。`last_fired_at` は**成功発火時だけ**刻み（skip /
//! 異常終了では刻まない・§6 N2）、異常終了は**メモリの last_attempt** で 1 周期ぶん backoff
//! して再発火ループを止める（§3.7 N-a）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

// #588 TimedFire の発火本体は lib（`opencrab_server::heartbeat_fire`）にあり、scheduler（時刻発火）と
// `run_my_schedule`（手動発火）が**同じ 1 つの関数**を呼ぶ。テスト用の別経路を作らない。
use opencrab_server::heartbeat_fire::run_one_heartbeat;
use opencrab_server::AppState;

/// sleep の頭打ち（秒）。NTP ジャンプ・DST・notify 取りこぼしの安全網（設計 §3.4）。
/// これで頭打ちしても判定は必ず `<= now` を再評価するので、遅れて拾うだけ。
const MAX_SLEEP_SECS: u64 = 300;

/// セッションの発火先（`session_id` から transport descriptor が解決する・#628）。
///
/// scheduler は登録簿（[`opencrab_actions::TimedFireRouter`]）へ問い合わせるだけで、transport の
/// 名前も ID 書式も知らない。発火（scheduler）と受理判定（`set_my_schedule`）は同じ登録簿を引くので
/// 「設定できたのに永遠に発火しない行」ができない（設計 §13.1）。
use opencrab_actions::FireTarget;

/// in-flight / attempts のキー（設計 §3.1）。行の id 単位。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum EntryKey {
    /// `agent_schedules` の 1 行。
    Schedule { schedule_id: i64 },
}

/// rebuild で組む 1 エントリ。
#[derive(Debug, Clone)]
struct Entry {
    /// in-flight / attempts / sleep 除外のキー（設計 §3.1）。
    key: EntryKey,
    agent_id: String,
    /// 発火先セッション。
    session_id: String,
    /// `None` = 起点（anchor/last_fired）が無い＝即発火可（設計 §4.3）。
    next_fire_at: Option<DateTime<Utc>>,
    /// 発火先（登録簿が解決した binding / session）。
    target: FireTarget,
    /// この行のプロンプト本文。
    message: String,
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

/// enabled なスケジュールから発火エントリを組む（設計 §3.2 rebuild）。
///
/// - `attempts`: 異常終了の last_attempt（メモリ・§3.7 N-a）。base を後ろへ逃がして backoff。
///
/// cron/`@every` の next は照会時算出（キャッシュ列なし）。解釈不能な式・発火先を解決できない
/// 行は fail-closed で skip（CRUD が 400 で弾くので通常ここには来ないが、DB を手で壊しても
/// 外部へ影響しないための保険）。
fn rebuild_entries(
    router: &opencrab_actions::TimedFireRouter,
    conn: &rusqlite::Connection,
    attempts: &HashMap<EntryKey, DateTime<Utc>>,
) -> Vec<Entry> {
    let mut entries = Vec::new();
    let rows = match opencrab_db::queries::list_enabled_agent_schedules(conn) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("scheduler: enabled schedule の列挙に失敗: {e}");
            return entries;
        }
    };
    for row in rows {
        let Some(schedule_id) = row.id else {
            continue; // DB 由来は必ず Some。
        };
        let Some(target) = router.resolve_persisted_target(conn, &row.session_id, &row.agent_id)
        else {
            tracing::warn!(
                agent_id = %row.agent_id,
                session_id = %row.session_id,
                schedule_id,
                "scheduler: schedule target could not be resolved; skipping"
            );
            continue;
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
            target,
            message: row.message,
        });
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

/// 中央スケジューラの本体ループ。プロセス寿命で回り続ける（設計 §3.2）。
///
/// `wake`（[`AppState::scheduler_wake`]）: schedule CRUD / ターン完了で rebuild を促す。
pub(crate) async fn run_scheduler(state: AppState) {
    let wake = state.scheduler_wake.clone();
    let db = state.db.clone();

    let in_flight: Arc<Mutex<HashSet<EntryKey>>> = Arc::new(Mutex::new(HashSet::new()));
    // 異常終了（panic / 発火不能）の last_attempt（メモリ・§3.7 N-a）。成功で除去。
    let attempts: Arc<Mutex<HashMap<EntryKey, DateTime<Utc>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tracing::info!("中央スケジューラを開始（agent_schedules）");

    loop {
        let now = Utc::now();

        let entries = {
            let Ok(conn) = db.lock() else {
                tracing::error!("scheduler: db lock 取得に失敗。MAX_SLEEP 眠って再試行");
                sleep_or_wake(MAX_SLEEP_SECS, &wake).await;
                continue;
            };
            let attempts_snapshot = attempts.lock().unwrap();
            rebuild_entries(&state.timed_fire_router, &conn, &attempts_snapshot)
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
            // next_fire が後ろへ逃げ再発火ループを止める・§3.7 N-a）。成功で除去。
            attempts.lock().unwrap().insert(entry.key.clone(), now);

            let entry = entry.clone();
            let state = state.clone();
            let db = db.clone();
            let in_flight = in_flight.clone();
            let attempts = attempts.clone();
            let wake = wake.clone();
            tokio::spawn(async move {
                // 完了（成功/失敗/パニック）で in-flight 除去 + wake（Drop ガード）。
                let _guard = InFlightGuard {
                    key: entry.key.clone(),
                    in_flight,
                    wake,
                };
                let EntryKey::Schedule { schedule_id } = entry.key;
                // #588 TimedFire: 発火先ゲートウェイのループへイベントを 1 本流すだけ
                // （ロック・配送・記録・継続はループが回す）。成功時だけ truthful に last_fired を刻む。
                let outcome = run_one_heartbeat(
                    &state,
                    &entry.agent_id,
                    &entry.target,
                    schedule_id,
                    &entry.message,
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
                    // 正常発火: attempt を除去（次回 base は truthful な last_fired へ戻る）。
                    // missed-run は base が最新の last_fired になるため 1 回に圧縮される（§8）。
                    attempts.lock().unwrap().remove(&entry.key);
                } else {
                    // 発火できず。last_fired は刻まない。spawn 時に打った attempt が残り、
                    // 1 周期ぶん backoff して即再試行ループを避ける（§3.7 N-a）。
                    tracing::debug!(
                        agent_id = %entry.agent_id,
                        session_id = %entry.session_id,
                        schedule_id,
                        "scheduler: 発火できず（backoff）"
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
