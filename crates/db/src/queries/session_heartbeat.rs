use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// Session Heartbeat Config（セッション単位ハートビート / #439 × #456）
// ============================================
//
// agent スコープ（`agent_heartbeat_config`）と channel スコープ
// （`discord_channel_config.heartbeat_*`）を畳んだ後継。発火先（Nostr broadcast /
// Discord channel）は `session_id` 接頭辞から導くので列に持たない。
//
// **この PR（PR1）では発火経路はまだこの表を読まない**（中央スケジューラへの切替は PR2）。
// ここではスキーマ・移行・クエリ関数までを用意する。

/// セッション単位ハートビート設定 1 行（`session_heartbeat_config`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeartbeatConfigRow {
    pub agent_id: String,
    /// `nostr-{agent}` / `discord-{agent}-{guild}-{channel}`。
    pub session_id: String,
    pub enabled: bool,
    /// 生値。`None` = 未設定（運用者の既定に従う）。
    ///
    /// **`u64` にしない**（`agent_heartbeat_config` と同方針）。壊れた値（0 以下）を
    /// `as u64` で巨大な正の数へ化けさせず、そのまま発火時の解決へ渡して fail-closed にする。
    pub interval_secs: Option<i64>,
    /// 永続アンカー（rfc3339 の壁時計）。`None` = 未設定（有効化時に打つ）。
    pub anchor_at: Option<String>,
    /// 最終発火時刻（rfc3339 の壁時計）。`None` = 未発火。
    pub last_fired_at: Option<String>,
}

/// `(agent_id, session_id)` で設定を取得する。行が無ければ `None`。
pub fn get_session_heartbeat_config(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<SessionHeartbeatConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id, session_id, enabled, interval_secs, anchor_at, last_fired_at
         FROM session_heartbeat_config WHERE agent_id = ?1 AND session_id = ?2",
        params![agent_id, session_id],
        |row| {
            Ok(SessionHeartbeatConfigRow {
                agent_id: row.get(0)?,
                session_id: row.get(1)?,
                enabled: row.get(2)?,
                interval_secs: row.get(3)?,
                anchor_at: row.get(4)?,
                last_fired_at: row.get(5)?,
            })
        },
    );
    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 設定行を作成/更新する（`updated_at` は現在時刻で更新）。
///
/// **`anchor_at`/`last_fired_at` はこの upsert では触らない**（呼び出し側が明示指定した値を
/// そのまま書く）。アンカーの向き（明示の有効化は `now`、非明示イベントは触らない）は
/// 呼び出し側（set_my_heartbeat / スケジューラ）が設計 §4.4 に従って決める。
pub fn upsert_session_heartbeat_config(
    conn: &Connection,
    cfg: &SessionHeartbeatConfigRow,
) -> Result<()> {
    conn.execute(
        "INSERT INTO session_heartbeat_config
            (agent_id, session_id, enabled, interval_secs, anchor_at, last_fired_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(agent_id, session_id) DO UPDATE SET
            enabled = excluded.enabled,
            interval_secs = excluded.interval_secs,
            anchor_at = excluded.anchor_at,
            last_fired_at = excluded.last_fired_at,
            updated_at = excluded.updated_at",
        params![
            cfg.agent_id,
            cfg.session_id,
            cfg.enabled,
            cfg.interval_secs,
            cfg.anchor_at,
            cfg.last_fired_at,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// **enabled = 1** のセッション設定を全件列挙する（中央スケジューラ用 / PR2）。
///
/// 発火可否の最終判定（`discord-` の G ゲート / whitelist ゲート・Nostr は対象外）は
/// スケジューラ側が握る。ここでは enabled 行を素直に返すだけ（`agent_heartbeat_config` の
/// `list_agents_with_heartbeat_enabled` と同じ二段構え）。
pub fn list_enabled_session_heartbeat_configs(
    conn: &Connection,
) -> Result<Vec<SessionHeartbeatConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, session_id, enabled, interval_secs, anchor_at, last_fired_at
         FROM session_heartbeat_config WHERE enabled = 1 ORDER BY agent_id, session_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SessionHeartbeatConfigRow {
            agent_id: row.get(0)?,
            session_id: row.get(1)?,
            enabled: row.get(2)?,
            interval_secs: row.get(3)?,
            anchor_at: row.get(4)?,
            last_fired_at: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// 発火成功時にアンカー系を更新する（設計 §4.4「発火成功」行）。
///
/// **向き**: `anchor_at` は触らず、`last_fired_at` を `now`（引数）へ進める。next_fire は
/// `last_fired + interval` で後ろへ動く（位相を前に引かない＝密にしない）。実際に発火した
/// ときだけ呼ぶ（skip では呼ばない・§6 N2 の「虚偽時刻を出さない」）。
pub fn set_session_last_fired(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    last_fired_at: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE session_heartbeat_config
         SET last_fired_at = ?3, updated_at = ?4
         WHERE agent_id = ?1 AND session_id = ?2",
        params![agent_id, session_id, last_fired_at, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// セッション行の `interval_secs`（生値）を発火に使う実効間隔へ解決する（純粋関数）。
///
/// `agent_heartbeat_config` の [`resolve_agent_heartbeat`] と**同じ fail-closed 意味論**:
/// - `None`（未設定）→ 運用者既定（下限で床上げ）。
/// - `Some(v)` かつ `v > 0` → `max(v, min)`（下限へ床上げ・費用が減る方向）。
/// - `Some(v)` かつ `v <= 0`（壊れた値）→ `None` = **発火させない**（スケジューラは skip）。
///
/// 移行（v37）は `interval_secs <= 0` の opt-in を enabled にしないので、enabled 行の
/// 生値は通常 `NULL` か正だが、壊れた行が混じっても発火側で fail-closed に倒すための保険。
pub fn resolve_session_interval_secs(
    interval_secs: Option<i64>,
    default_interval_secs: u64,
    min_interval_secs: u64,
) -> Option<u64> {
    let min = min_interval_secs.max(1);
    let fallback = default_interval_secs.max(min);
    match interval_secs {
        None => Some(fallback),
        Some(v) if v > 0 => Some((v as u64).max(min)),
        // 0 以下は壊れた値。有効化の方向へ倒さず None（発火しない）。
        Some(_) => None,
    }
}

/// 次回発火時刻を算出する純粋関数（設計 §4.3・#439-4）。
///
/// **真実は再計算**（キャッシュ列を持たない）。`base` は `last_fired_at`（実際に発火した
/// 時刻）を優先し、無ければ `anchor_at`（有効化・移行で打った起点）。どちらも無ければ
/// `None` = 「起点が無い＝即発火可」。
///
/// **向き（設計 §4.4）**: `base + interval` は常に発火起点より後ろ。位相を前へ引かない
/// （非明示イベントで密にしない）。呼び出し側が `last_fired`/`anchor` をどう更新するかで
/// 向きを制御し、この関数自体は `base` を後ろへ進めるだけ。
///
/// 異常終了時の backoff（設計 §3.7 N-a）は、呼び出し側が `last_fired` の位置に
/// `max(last_fired_at, last_attempt_at)` を渡すことで実現する（`last_fired_at` の
/// truthfulness を保ちつつ再発火ループを止める）。この関数のシグネチャは変えない。
pub fn heartbeat_next_fire_at(
    anchor_at: Option<DateTime<Utc>>,
    last_fired_at: Option<DateTime<Utc>>,
    interval_secs: u64,
) -> Option<DateTime<Utc>> {
    let base = last_fired_at.or(anchor_at)?;
    Some(base + Duration::seconds(interval_secs as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_session_interval_semantics_match_resolve_agent() {
        // None → 既定（下限で床上げ）。
        assert_eq!(resolve_session_interval_secs(None, 1800, 300), Some(1800));
        // 既定が下限未満なら下限へ。
        assert_eq!(resolve_session_interval_secs(None, 100, 300), Some(300));
        // 正の値はそのまま（下限以上）。
        assert_eq!(
            resolve_session_interval_secs(Some(1200), 1800, 300),
            Some(1200)
        );
        // 下限未満の正値は下限へ床上げ（拒否ではなく引き上げ）。
        assert_eq!(
            resolve_session_interval_secs(Some(100), 1800, 300),
            Some(300)
        );
        // 0・負は壊れた値 → None（発火しない）。
        assert_eq!(resolve_session_interval_secs(Some(0), 1800, 300), None);
        assert_eq!(resolve_session_interval_secs(Some(-5), 1800, 300), None);
    }

    #[test]
    fn next_fire_prefers_last_fired_then_anchor_else_none() {
        let anchor = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let last = DateTime::parse_from_rfc3339("2026-01-01T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // last_fired 優先。
        assert_eq!(
            heartbeat_next_fire_at(Some(anchor), Some(last), 600),
            Some(last + Duration::seconds(600))
        );
        // last_fired 無ければ anchor。
        assert_eq!(
            heartbeat_next_fire_at(Some(anchor), None, 600),
            Some(anchor + Duration::seconds(600))
        );
        // どちらも無ければ None（即発火可）。
        assert_eq!(heartbeat_next_fire_at(None, None, 600), None);
    }
}
