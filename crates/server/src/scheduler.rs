//! 中央ハートビートスケジューラ（#439 / #437 / #438 / 設計 §3）。
//!
//! # なぜ中央スケジューラか
//!
//! 旧実装は**エージェントごとに** `core::heartbeat::heartbeat_loop` を 1 本立て、固定
//! グリッド（グローバル 1800 秒に丸めた sleep）で目を覚まし、コールバック内の
//! `resolve_agent_heartbeat` + `heartbeat_firing_plan` で発火可否を判定していた。位相は
//! メモリ（`Instant`）だけで持ち、再起動で消えて（#439-1）、設定変更はループを張り直す
//! まで効かず（#437）、sleep グリッドと設定間隔が食い違っていた（#438）。
//!
//! ここでは **単一のタスク**が、`session_heartbeat_config`（enabled 行）を毎ウェイクで
//! DB から読み直し、**永続アンカー**（`anchor_at`/`last_fired_at`・壁時計）から
//! **正確な次回発火時刻**を算出し、**最も近い次回発火まで眠る**。設定変更・発火ターン
//! 完了・global config 変更は `scheduler_wake`（[`AppState::scheduler_wake`]）で起こして
//! rebuild させる（即時反映・#437）。位相は DB に永続するので再起動で伸びない（#439-1）。
//!
//! # 発火集合（不変条件・設計 §4.2 / §5）
//!
//! **実発火 = `enabled AND (session が nostr- OR live G)`**。`discord-` セッションは
//! 発火時に live G（`cfg.agent.heartbeat_enabled`・hot-reload 追従）でゲートし、`nostr-`
//! は G 非依存。**whitelist ゲートは現行 HB 発火経路に存在しないので掛けない**（掛けると
//! PR1 が確立した「発火集合を変えない」不変条件と食い違う。設計 §5 N3）。
//!
//! # ビジーループを作らない（設計 §3.2 A1 / §6）
//!
//! 走行中（in-flight）セッションは (1) sleep の `min` 候補から除外し、(2) ターン完了で
//! wake する。よって走行中は 0 秒スピンせず、完了した瞬間に rebuild してそのターンが
//! truthful に刻んだ `last_fired_at` から次回を計算する。`last_fired_at` は**成功発火時
//! だけ**刻み（skip / 異常終了では刻まない・§6 N2）、異常終了は**メモリの last_attempt**
//! で interval ぶん backoff して再発火ループを止める（§3.7 N-a）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use opencrab_core::heartbeat::{HeartbeatConfig, HeartbeatDecision};
use tokio::sync::watch;

use crate::heartbeat_turn::{HeartbeatTarget, HeartbeatTurnRunner, TurnOrigin};
use opencrab_server::AppState;

/// sleep の頭打ち（秒）。NTP ジャンプ・DST・notify 取りこぼしの安全網（設計 §3.4）。
/// これで頭打ちしても判定は必ず `<= now` を再評価するので、遅れて拾うだけ。
const MAX_SLEEP_SECS: u64 = 300;

/// セッションの発火先（`session_id` 接頭辞から導く・設計 §3.6）。
///
/// 発火経路を持たないセッション種別（`web-`/`heartbeat-`/`agent-msg-` 等）や壊れた
/// session_id は `None` = **fail-closed**（発火せず warn）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum FireTarget {
    /// `nostr-{agent}`: Nostr broadcast（G ゲート対象外）。
    NostrBroadcast,
    /// `discord-{agent}-{guild}-{channel}`: その channel（発火時 live G でゲート）。
    DiscordChannel {
        guild_id: String,
        channel_id: String,
    },
}

/// `session_id` を保存済み `agent_id` で剥がして発火先を導く（設計 §3.6・B4）。
///
/// **naive な `split('-')` は禁止**（`agent_id` は UUID でハイフンを含む）。保存済み
/// `agent_id` で接頭辞を剥がし、残りの guild/channel が数値（ハイフン無し）であることを
/// 確認する。合致しなければ `None`（fail-closed）。
fn resolve_fire_target(session_id: &str, agent_id: &str) -> Option<FireTarget> {
    if session_id == format!("nostr-{agent_id}") {
        return Some(FireTarget::NostrBroadcast);
    }
    let discord_prefix = format!("discord-{agent_id}-");
    if let Some(rest) = session_id.strip_prefix(&discord_prefix) {
        // rest = "{guild}-{channel}"。guild/channel は数値（ハイフン無し）なので rsplit_once 安全。
        if let Some((guild, channel)) = rest.rsplit_once('-') {
            let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            if numeric(guild) && numeric(channel) {
                return Some(FireTarget::DiscordChannel {
                    guild_id: guild.to_string(),
                    channel_id: channel.to_string(),
                });
            }
        }
    }
    None
}

/// rebuild で組む 1 エントリ。
#[derive(Debug, Clone)]
struct Entry {
    agent_id: String,
    /// 発火先セッション（`nostr-…` / `discord-…`）。in-flight キー・DB 更新キーに使う。
    session_id: String,
    target: FireTarget,
    /// `None` = 起点（anchor/last_fired）が無い＝即発火可（設計 §4.3）。
    /// `interval_secs` は算出済みなので保持しない（次回は完了後の rebuild で再計算する）。
    next_fire_at: Option<DateTime<Utc>>,
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

/// enabled セッションから発火エントリを組む（設計 §3.2 rebuild）。
///
/// - `live_g`: 発火時に読んだ live G（`discord-` のマスタゲート）。`nostr-` は非依存。
/// - `attempts`: 異常終了の last_attempt（メモリ・§3.7 N-a）。base を後ろへ逃がして backoff。
///
/// 期待集合を手書きしない（`list_enabled_session_heartbeat_configs` をそのまま解決）。
fn rebuild_entries(
    conn: &rusqlite::Connection,
    live_g: bool,
    default_interval_secs: u64,
    min_interval_secs: u64,
    attempts: &HashMap<String, DateTime<Utc>>,
) -> Vec<Entry> {
    let rows = match opencrab_db::queries::list_enabled_session_heartbeat_configs(conn) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("scheduler: enabled セッションの列挙に失敗: {e}");
            return vec![];
        }
    };
    let mut entries = Vec::with_capacity(rows.len());
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
        let anchor = parse_wall_clock(&row.anchor_at);
        let db_last = parse_wall_clock(&row.last_fired_at);
        // base = max(last_fired_at, last_attempt_at)。truthful な last_fired は保ちつつ、
        // 異常終了時の attempt で next_fire を interval ぶん後ろへ逃がす（§3.7 N-a）。
        let effective_last = later_of(db_last, attempts.get(&row.session_id).copied());
        let next_fire_at =
            opencrab_db::queries::heartbeat_next_fire_at(anchor, effective_last, interval_secs);
        entries.push(Entry {
            agent_id: row.agent_id,
            session_id: row.session_id,
            target,
            next_fire_at,
        });
    }
    entries
}

/// in-flight 除去 + wake を**パニックでも**確実に行う Drop ガード（設計 §6 / §3.5d）。
///
/// 完了で in-flight を外し、同時に `scheduler_wake` を鳴らす。これで走行中に眠っていた
/// スケジューラが即座に rebuild して、完了ターンが刻んだ `last_fired_at` から次回を計算
/// できる（A1 のスピン回避と truthfulness の両立）。異常終了時の backoff は spawn 時に
/// 打った `attempts[session_id]` が担う（このガードは触らない）。
struct InFlightGuard {
    session_id: String,
    in_flight: Arc<Mutex<HashSet<String>>>,
    wake: Arc<tokio::sync::Notify>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(&self.session_id);
        }
        // 完了 wake。取りこぼしても §MAX_SLEEP で再ループするので正しさは rebuild が担保。
        self.wake.notify_one();
    }
}

/// 1 発火分の準備（HB セッション作成・指示解決・プロンプト挿入）→ ターン実行。
///
/// 旧 `make_heartbeat_callback` の per-target 本体（session 作成・instructions・プロンプト
/// 挿入・`run_turn`）と同じことを、session モデルの 1 エントリに対して行う。`None` は
/// 「ターンを開始できなかった」（文脈組み立て失敗）で、呼び出し側は last_fired を刻まない。
async fn run_one_fire(
    runner: &Arc<HeartbeatTurnRunner>,
    db: &opencrab_db::Db,
    entry: &Entry,
    tick: u64,
) -> Option<HeartbeatDecision> {
    // 発火先セッションの種別から、HB ターンの宛先フィールドを導く。
    let (channel_id, guild_id, channel_name) = match &entry.target {
        FireTarget::NostrBroadcast => (
            String::new(),
            String::new(),
            crate::HEARTBEAT_AGENT_SCOPED_LABEL.to_string(),
        ),
        FireTarget::DiscordChannel {
            guild_id,
            channel_id,
        } => {
            // 表示名は channel 設定から引く（無ければ channel_id をそのまま使う）。
            // 発火集合には影響しない表示専用フィールド。
            let name = {
                let conn = db.lock().ok()?;
                opencrab_db::queries::get_channel_config_for_agent(
                    &conn,
                    channel_id,
                    &entry.agent_id,
                )
                .ok()
                .flatten()
                .map(|r| r.channel_name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| channel_id.clone())
            };
            (channel_id.clone(), guild_id.clone(), name)
        }
    };

    // HB 専用セッション（`heartbeat-{agent}-{channel}`。nostr は channel_id 空）。
    let hb_session_id = crate::get_or_create_heartbeat_session(db, &entry.agent_id, &channel_id);

    // 指示文を解決し、HB プロンプトを HB セッションへ挿入する（run_turn が文脈へ読む）。
    let instructions_source = {
        let conn = db.lock().ok()?;
        let resolved = opencrab_db::queries::resolve_heartbeat_instructions(
            &conn,
            &entry.agent_id,
            &channel_id,
        );
        let prompt = format!(
            "[ハートビート] 現在の会話「{}」。{}\n出力形式: SPEAK/LEARN/IDLE のいずれか。SPEAKの場合のみ 'SPEAK: <メッセージ>' の形式で一言。",
            channel_name, resolved.text
        );
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: entry.agent_id.clone(),
            session_id: hb_session_id.clone(),
            log_type: "system".to_string(),
            content: prompt,
            speaker_id: Some("heartbeat".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            tracing::error!(agent_id = %entry.agent_id, "scheduler: HB プロンプトの挿入に失敗: {e}");
            return None;
        }
        resolved.source
    };

    let target = HeartbeatTarget {
        agent_id: entry.agent_id.clone(),
        session_id: hb_session_id,
        channel_id,
        guild_id,
        instructions_source,
    };
    // 唯一の入口。同一 HB セッションのロック下で 1 ターン走らせる（直列化）。
    runner.run_turn(&target, TurnOrigin::Tick { tick }).await
}

/// 中央スケジューラの本体ループ。プロセス寿命で回り続ける（設計 §3.2）。
///
/// - `config_rx`: hot-reload が push する `HeartbeatConfig`。**発火時に `.borrow().enabled`
///   を読んで live G を得る**（起動時スナップショットにしない・§4.2 の kill-switch ライブ性）。
/// - `wake`: [`AppState::scheduler_wake`]。set/CRUD/global 変更/ターン完了で rebuild を促す。
pub(crate) async fn run_scheduler(
    state: AppState,
    runner: Arc<HeartbeatTurnRunner>,
    config_rx: watch::Receiver<HeartbeatConfig>,
) {
    let wake = state.scheduler_wake.clone();
    let db = state.db.clone();
    let default_interval_secs = state.heartbeat_limits.default_interval_secs;
    let min_interval_secs = state.heartbeat_limits.min_interval_secs;

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

    let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    // 異常終了（panic / 文脈失敗）の last_attempt（メモリ・§3.7 N-a）。成功で除去。
    let attempts: Arc<Mutex<HashMap<String, DateTime<Utc>>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut fire_seq: u64 = 0;

    tracing::info!("中央ハートビートスケジューラを開始");

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
                if set.contains(&entry.session_id) {
                    tracing::info!(
                        agent_id = %entry.agent_id,
                        session_id = %entry.session_id,
                        "scheduler skip: 前回発火がまだ走行中"
                    );
                    continue;
                }
                set.insert(entry.session_id.clone());
            }
            // 異常終了の backoff 起点を spawn 時に打つ（panic で success 印が付かなくても
            // next_fire = attempt + interval になり再発火ループを止める・§3.7 N-a）。成功で除去。
            attempts
                .lock()
                .unwrap()
                .insert(entry.session_id.clone(), now);

            fire_seq += 1;
            let tick = fire_seq;
            let entry = entry.clone();
            let runner = runner.clone();
            let db = db.clone();
            let in_flight = in_flight.clone();
            let attempts = attempts.clone();
            let wake = wake.clone();
            tokio::spawn(async move {
                // 完了（成功/失敗/パニック）で in-flight 除去 + wake（Drop ガード）。
                let _guard = InFlightGuard {
                    session_id: entry.session_id.clone(),
                    in_flight,
                    wake,
                };
                let outcome = run_one_fire(&runner, &db, &entry, tick).await;
                match outcome {
                    Some(_) => {
                        // 正常発火: truthful に last_fired_at=now を刻み、attempt を除去する。
                        // next_fire = last_fired + interval（後ろへ・§4.4）。missed-run は
                        // 1 回に圧縮される（base が最新の last_fired になるため・§8）。
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
                        attempts.lock().unwrap().remove(&entry.session_id);
                    }
                    None => {
                        // 文脈組み立て失敗。last_fired は刻まない（発火していない）。spawn 時に
                        // 打った attempt が残り、interval ぶん backoff して即再試行ループを避ける。
                        tracing::debug!(
                            agent_id = %entry.agent_id,
                            session_id = %entry.session_id,
                            "scheduler: 発火ターンを開始できず（backoff）"
                        );
                    }
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
/// **in-flight エントリを候補から除外する**（走行中セッションの `next_fire` は `<= now` に
/// 貼り付くが、完了まで `last_fired` を進めない〔N2〕ので、含めると sleep(0) スピンになる）。
/// 除外した上で「未来（`> now`）の最小 next_fire」まで眠る。候補が無ければ `MAX_SLEEP`
/// （全て in-flight / 起点なしで due 済みの状態でも 0 秒スピンしない）。上限は `MAX_SLEEP`
/// で頭打ち（NTP ジャンプ・DST・notify 取りこぼしの安全網。判定は再ループで `<= now` 再評価）。
fn next_sleep_secs(entries: &[Entry], in_flight: &HashSet<String>, now: DateTime<Utc>) -> u64 {
    let next = entries
        .iter()
        .filter(|e| !in_flight.contains(&e.session_id))
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
    use opencrab_db::queries::SessionHeartbeatConfigRow;
    use std::collections::HashMap;

    const AGENT_B: &str = "c56f19e0-1111-2222-3333-444455556666";

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
    /// enabled=0 の抑止行（opt-in の Discord）は `list_enabled` が返さないので発火集合に
    /// 入らない。**期待集合を手書きするが、右辺は G ゲートの効果のみで左辺と差が出る**。
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
        // すべて anchor 12h 前・interval 最大 5h なので due（next_fire <= now）。
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
            // 発火経路の無い種別 → resolve_fire_target None → skip。
            row(
                AGENT_UUID,
                &format!("web-{AGENT_UUID}"),
                true,
                Some(600),
                Some(past.clone()),
                None,
            ),
            // interval <= 0（壊れた値）→ skip。
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
            agent_id: AGENT_UUID.to_string(),
            session_id: session.to_string(),
            target: FireTarget::NostrBroadcast,
            next_fire_at: next,
        }
    }

    /// in-flight エントリは sleep 候補から除外され、0 秒スピンしない（A1）。
    #[test]
    fn sleep_excludes_in_flight_and_never_spins_to_zero() {
        let now = Utc::now();
        let mut in_flight = HashSet::new();
        in_flight.insert("s-running".to_string());

        // (a) 走行中で attempt により next_fire が未来(now+10)へ押されたエントリは、完了 wake
        //     で拾うので候補から除外する。除外しないと 10s で無駄に起きてしまう（→ 120 に
        //     ならず 10 になる）。in-flight フィルタが load-bearing。
        let running_future = entry_at("s-running", Some(now + Duration::seconds(10)));
        let idle_future = entry_at("s-future", Some(now + Duration::seconds(120)));
        assert_eq!(
            next_sleep_secs(&[running_future, idle_future], &in_flight, now),
            120,
            "in-flight の未来エントリを除外し、非 in-flight の 120s まで眠る"
        );

        // (b) 走行中で next_fire が過去(<=now)に貼り付いたエントリだけ → 0 秒スピンせず MAX_SLEEP。
        let running_stuck = entry_at("s-running", Some(now - Duration::seconds(30)));
        assert_eq!(
            next_sleep_secs(&[running_stuck], &in_flight, now),
            MAX_SLEEP_SECS,
            "走行中 due だけなら 0 秒スピンせず MAX_SLEEP（完了 wake で拾う）"
        );

        // (c) 未来エントリが MAX を超えても MAX で頭打ち。
        let far = entry_at("s-far", Some(now + Duration::hours(2)));
        assert_eq!(
            next_sleep_secs(&[far], &HashSet::new(), now),
            MAX_SLEEP_SECS
        );
    }

    // ---- missed-run 圧縮 / アンカーの向き（§8 / §4.4 / §3.7 N-a） ----

    /// 長時間ダウン後も、過ぎたスロットは**1 エントリ**にまとまる（多重発火しない・§8）。
    /// 発火成功で last_fired=now を刻むと next_fire は now+interval（未来）へ後退する
    /// （位相を前に引かない＝密にしない・§4.4）。
    #[test]
    fn missed_run_compresses_to_one_and_success_pushes_forward() {
        let long_ago = (Utc::now() - Duration::days(3)).to_rfc3339();
        let sid = format!("nostr-{AGENT_UUID}");
        let conn = conn_with(&[row(AGENT_UUID, &sid, true, Some(600), Some(long_ago), None)]);

        // 3 日過ぎていても 1 エントリ・due（多重発火しない）。
        let before = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert_eq!(before.len(), 1);
        assert!(before[0]
            .next_fire_at
            .map(|t| t <= Utc::now())
            .unwrap_or(true));

        // 発火成功を模して last_fired=now を刻む。
        let now_str = Utc::now().to_rfc3339();
        opencrab_db::queries::set_session_last_fired(&conn, AGENT_UUID, &sid, &now_str).unwrap();

        // next_fire = last_fired + 600 → 未来（もう due でない・向きは後ろ）。
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

    /// 異常終了の last_attempt（メモリ）は next_fire を interval ぶん後ろへ逃がす
    /// （再発火ループを止める・§3.7 N-a）。last_fired は触らない（truthfulness）。
    #[test]
    fn last_attempt_backoff_defers_next_fire_without_touching_last_fired() {
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        let sid = format!("nostr-{AGENT_UUID}");
        let conn = conn_with(&[row(AGENT_UUID, &sid, true, Some(600), Some(past), None)]);

        // attempt 無し → due（即発火可）。
        let due = rebuild_entries(&conn, true, 1800, 300, &HashMap::new());
        assert!(due[0].next_fire_at.map(|t| t <= Utc::now()).unwrap_or(true));

        // attempt=now を打つと next_fire = now+600（未来）へ逃げる（backoff）。
        let mut attempts = HashMap::new();
        attempts.insert(sid, Utc::now());
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

    /// 発火ターンが **panic** したとき、`InFlightGuard::drop` が in_flight を除去し・完了 wake を
    /// 鳴らし・`last_attempt` を **残す**ことを実行で確認する。panic は spawn 内の
    /// `run_one_fire().await` で unwind するので、後続の Some/None 分岐（`set_session_last_fired` +
    /// `attempts.remove`）には到達しない——**attempt を消してよいのは成功 Some 分岐だけ**なので、
    /// panic では attempt が残り、rebuild が `next_fire = attempt + interval`（未来）で backoff する。
    /// これで panic → 即再発火のスピンを避ける（§6）。guard が attempt を触るように退行したり、
    /// in_flight 除去をやめたりすると、このテストが落ちる（本番の live 経路を守る）。
    #[tokio::test]
    async fn panic_in_fire_clears_in_flight_keeps_attempt_and_wakes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let attempts: Arc<Mutex<HashMap<String, DateTime<Utc>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let wake = Arc::new(tokio::sync::Notify::new());
        let sid = format!("nostr-{AGENT_UUID}");

        // 本体ループが spawn 直前に打つ状態を再現する（in_flight へ insert・attempt へ last_attempt）。
        in_flight.lock().unwrap().insert(sid.clone());
        attempts.lock().unwrap().insert(sid.clone(), Utc::now());

        // 完了 wake を観測する待ち手。
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

        // 発火ターンが panic する spawn（`run_one_fire` 内の panic を模す）。Some/None 分岐へ
        // 到達しないので `set_session_last_fired` も `attempts.remove` も走らない。
        let jh = {
            let in_flight = in_flight.clone();
            let wake = wake.clone();
            let sid = sid.clone();
            tokio::spawn(async move {
                let _guard = InFlightGuard {
                    session_id: sid,
                    in_flight,
                    wake,
                };
                panic!("boom: 発火ターン内 panic");
            })
        };
        let res = jh.await;
        assert!(res.is_err(), "spawn した発火ターンは panic するはず");

        // (1) Drop ガードが in_flight から除去した（panic の unwind 中でも走る）。
        assert!(
            !in_flight.lock().unwrap().contains(&sid),
            "panic 後も in_flight に残る（Drop ガードが効いていない＝走行中扱いで固着し二度と発火しない）"
        );
        // (2) last_attempt は残る（成功 Some 分岐に来ないので remove されない）→ backoff が効く。
        assert!(
            attempts.lock().unwrap().contains_key(&sid),
            "panic 時に attempt が消える（backoff が効かず即再発火スピンになる）"
        );
        // (3) 完了 wake が届いた（Drop ガードの notify_one）。
        tokio::task::yield_now().await;
        assert!(
            woken.load(Ordering::SeqCst),
            "panic 後の完了 wake が鳴っていない（rebuild が促されない）"
        );
    }
}
