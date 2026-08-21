//! 定時実行（#455）の cron / `@every` 解決（純粋関数・設計 §7.2）。
//!
//! **中央スケジューラ（bin）と CRUD 検証（lib `api`）の両方**がここを使う。両者が別々に
//! cron を解釈すると「登録できたのに発火しない/その逆」の乖離が出るため、解釈は 1 か所に集約する。
//!
//! # 表現（統括裁定・設計 §7.2）
//! - **標準 5 フィールド cron**（例 `0 7 * * *`）は [`croner`] で解釈し、`timezone`
//!   （既定 `Asia/Tokyo`・[`chrono_tz`]）で評価して UTC へ戻す。
//! - **`@every <dur>`**（例 `@every 3h` / `@every 1h30m`）は cron ライブラリに寄せず
//!   **自前パーサ**でアンカー方式に解決する（`base + dur`。heartbeat の §4.3 と同型）。
//!
//! # 持ち方（統括裁定）
//! next 時刻は**列に持たず照会時算出**（stale フリー）。base は `last_fired_at.or(anchor_at)`。
//!
//! # 制約を足さない（設計 §9 / §10.1）
//! **最小間隔の下限クランプは設けない**（余計な制約を足さない）。多重実行は in-flight 集合が
//! 防ぐので、短周期でもターン完了までは重複発火しない。ただし `@every 0`（周期ゼロ）は
//! 完了直後に即再発火して連続ターンの暴走になるため**解釈不能として拒否**する（policy の床では
//! なく fail-closed）。

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use croner::Cron;
use std::str::FromStr;

/// cron / `@every` / timezone の解釈不能（CRUD は 400、実行時は発火対象外＝fail-closed）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleParseError {
    /// タイムゾーン名が不正（chrono-tz が知らない）。
    BadTimezone(String),
    /// 標準 cron 式として解釈できない。
    BadCron(String),
    /// `@every <dur>` の期間が解釈できない（空・単位不正・ゼロ以下）。
    BadEvery(String),
}

impl std::fmt::Display for ScheduleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduleParseError::BadTimezone(s) => write!(f, "不正なタイムゾーン: {s}"),
            ScheduleParseError::BadCron(s) => write!(f, "不正な cron 式: {s}"),
            ScheduleParseError::BadEvery(s) => write!(f, "不正な @every 期間: {s}"),
        }
    }
}

impl std::error::Error for ScheduleParseError {}

/// `@every <dur>` の期間を [`Duration`] へ解釈する。
///
/// - `@every` 接頭辞が無ければ `Ok(None)`（標準 cron として扱う合図）。
/// - `@every 3h` / `@every 1h30m` / `@every 90s` / `@every 2d12h` などを受ける。
/// - 単位は `d`/`h`/`m`/`s`。空・単位不正・合計 0 以下は `Err(BadEvery)`（fail-closed）。
pub fn parse_every(expr: &str) -> Result<Option<Duration>, ScheduleParseError> {
    let trimmed = expr.trim();
    let Some(rest) = trimmed.strip_prefix("@every") else {
        return Ok(None);
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(ScheduleParseError::BadEvery(expr.to_string()));
    }
    // `<num><unit>` の連なりを畳む。数字とその直後の単位以外が来たら不正。
    let mut total_secs: i64 = 0;
    let mut num: Option<i64> = None;
    for ch in rest.chars() {
        if ch.is_ascii_digit() {
            let d = (ch as u8 - b'0') as i64;
            num = Some(num.unwrap_or(0).saturating_mul(10).saturating_add(d));
        } else {
            let unit_secs = match ch {
                'd' => 86_400,
                'h' => 3_600,
                'm' => 60,
                's' => 1,
                _ => return Err(ScheduleParseError::BadEvery(expr.to_string())),
            };
            let n = num
                .take()
                .ok_or_else(|| ScheduleParseError::BadEvery(expr.to_string()))?;
            total_secs = total_secs.saturating_add(n.saturating_mul(unit_secs));
        }
    }
    // 数字が単位を伴わず余る（例 `@every 3`）→ 不正。
    if num.is_some() {
        return Err(ScheduleParseError::BadEvery(expr.to_string()));
    }
    // 周期ゼロ以下は暴走のもと → fail-closed（policy の床ではなく解釈不能扱い）。
    if total_secs <= 0 {
        return Err(ScheduleParseError::BadEvery(expr.to_string()));
    }
    Ok(Some(Duration::seconds(total_secs)))
}

/// cron 式 + timezone が解釈可能かを検証する（CRUD 入力検証用）。
///
/// `@every` はアンカー方式なので timezone を使わないが、CRUD では一律に timezone も
/// 検証しておく（無効な tz を保存させない）。
pub fn validate_schedule(cron_expr: &str, timezone: &str) -> Result<(), ScheduleParseError> {
    // timezone は常に検証（保存前に弾く）。
    Tz::from_str(timezone).map_err(|_| ScheduleParseError::BadTimezone(timezone.to_string()))?;
    if parse_every(cron_expr)?.is_some() {
        return Ok(());
    }
    // 標準 cron。
    Cron::from_str(cron_expr)
        .map_err(|e| ScheduleParseError::BadCron(format!("{cron_expr}: {e}")))?;
    Ok(())
}

/// 次回発火時刻を UTC で算出する（真実は再計算・キャッシュ列なし・設計 §7.2）。
///
/// - `base = last_fired_at.or(anchor_at)`。
/// - `@every dur`: `base + dur`。`base` が無ければ `None`（＝起点なし＝即発火可）。
/// - 標準 cron: `base`（無ければ `None`＝即発火可）以降の**最初のスロット**を `timezone` で
///   評価して返す。croner は渡した `DateTime` の tz で評価するので、`base` を `timezone` へ
///   変換してから `find_next_occurrence(.., inclusive=false)` を呼び、結果を UTC へ戻す。
///
/// **向き（設計 §4.4）**: `base` を後ろへ進めるだけ（位相を前へ引かない）。missed-run は
/// 「base 以降の最初のスロット」が過去でも 1 つだけ返るので、スケジューラ側で 1 回に圧縮される。
pub fn schedule_next_fire_at(
    cron_expr: &str,
    timezone: &str,
    anchor_at: Option<DateTime<Utc>>,
    last_fired_at: Option<DateTime<Utc>>,
) -> Result<Option<DateTime<Utc>>, ScheduleParseError> {
    let base = last_fired_at.or(anchor_at);

    if let Some(dur) = parse_every(cron_expr)? {
        // `@every`: 起点 + 周期。起点が無ければ即発火可（None）。
        return Ok(base.map(|b| b + dur));
    }

    // 標準 cron。
    let tz = Tz::from_str(timezone)
        .map_err(|_| ScheduleParseError::BadTimezone(timezone.to_string()))?;
    let cron = Cron::from_str(cron_expr)
        .map_err(|e| ScheduleParseError::BadCron(format!("{cron_expr}: {e}")))?;
    let Some(base) = base else {
        // 起点が無い → 即発火可（発火後 last_fired=now が刻まれ、以後は正規の cron スロット）。
        return Ok(None);
    };
    let base_in_tz = base.with_timezone(&tz);
    // inclusive=false: base ちょうどのスロットは含めない（発火直後の base==last_fired で
    // 同一スロットを二度返さない）。
    let next = cron
        .find_next_occurrence(&base_in_tz, false)
        .map_err(|e| ScheduleParseError::BadCron(format!("{cron_expr}: {e}")))?;
    Ok(Some(next.with_timezone(&Utc)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    // ---- @every パーサ ----

    #[test]
    fn parse_every_accepts_units_and_combos() {
        assert_eq!(parse_every("@every 3h").unwrap(), Some(Duration::hours(3)));
        assert_eq!(
            parse_every("@every 1h30m").unwrap(),
            Some(Duration::minutes(90))
        );
        assert_eq!(
            parse_every("@every 90s").unwrap(),
            Some(Duration::seconds(90))
        );
        assert_eq!(
            parse_every("@every 2d12h").unwrap(),
            Some(Duration::hours(60))
        );
        // 余分な空白も許容。
        assert_eq!(
            parse_every("  @every   45m  ").unwrap(),
            Some(Duration::minutes(45))
        );
    }

    #[test]
    fn parse_every_returns_none_for_standard_cron() {
        assert_eq!(parse_every("0 7 * * *").unwrap(), None);
    }

    #[test]
    fn parse_every_rejects_garbage_and_zero() {
        assert!(parse_every("@every").is_err());
        assert!(parse_every("@every ").is_err());
        assert!(parse_every("@every 3").is_err()); // 単位なし
        assert!(parse_every("@every 3x").is_err()); // 不正単位
        assert!(parse_every("@every 0s").is_err()); // 周期ゼロ（暴走）
        assert!(parse_every("@every 0h0m").is_err());
        assert!(parse_every("@every h").is_err()); // 数字なし
    }

    // ---- validate ----

    #[test]
    fn validate_accepts_cron_and_every_rejects_bad() {
        assert!(validate_schedule("0 7 * * *", "Asia/Tokyo").is_ok());
        assert!(validate_schedule("@every 3h", "Asia/Tokyo").is_ok());
        // 不正 tz。
        assert!(matches!(
            validate_schedule("0 7 * * *", "Mars/Phobos"),
            Err(ScheduleParseError::BadTimezone(_))
        ));
        // 不正 cron。
        assert!(matches!(
            validate_schedule("not a cron", "Asia/Tokyo"),
            Err(ScheduleParseError::BadCron(_))
        ));
        // 不正 @every。
        assert!(matches!(
            validate_schedule("@every nonsense", "Asia/Tokyo"),
            Err(ScheduleParseError::BadEvery(_))
        ));
    }

    // ---- next fire: @every ----

    #[test]
    fn next_fire_every_is_base_plus_dur() {
        let anchor = utc("2026-08-09T00:00:00Z");
        // last_fired 無し → base=anchor。
        assert_eq!(
            schedule_next_fire_at("@every 3h", "Asia/Tokyo", Some(anchor), None).unwrap(),
            Some(anchor + Duration::hours(3))
        );
        // last_fired 優先。
        let last = utc("2026-08-09T05:00:00Z");
        assert_eq!(
            schedule_next_fire_at("@every 3h", "Asia/Tokyo", Some(anchor), Some(last)).unwrap(),
            Some(last + Duration::hours(3))
        );
        // 起点なし → None（即発火可）。
        assert_eq!(
            schedule_next_fire_at("@every 3h", "Asia/Tokyo", None, None).unwrap(),
            None
        );
    }

    // ---- next fire: 標準 cron（JST 評価）----

    #[test]
    fn next_fire_cron_evaluates_in_timezone() {
        // base = 2026-08-09 06:00 JST（前日 21:00 UTC）。次の "0 7 * * *"(JST) は同日 07:00 JST。
        let base_jst_0600 = utc("2026-08-08T21:00:00Z"); // = 2026-08-09 06:00 +09:00
        let next = schedule_next_fire_at("0 7 * * *", "Asia/Tokyo", Some(base_jst_0600), None)
            .unwrap()
            .unwrap();
        // 07:00 JST = 22:00 UTC 前日。
        assert_eq!(next, utc("2026-08-08T22:00:00Z"));
    }

    #[test]
    fn next_fire_cron_missed_slot_is_in_the_past_and_singular() {
        // base が 3 日前 → 次スロットは過去（due・missed）。1 つだけ返る（スケジューラが圧縮）。
        let base = Utc::now() - Duration::days(3);
        let next = schedule_next_fire_at("0 7 * * *", "Asia/Tokyo", Some(base), None)
            .unwrap()
            .unwrap();
        assert!(next < Utc::now(), "過ぎたスロットが 1 つ返る（missed-run）");
    }

    #[test]
    fn next_fire_cron_timezone_differs_from_utc() {
        // 同じ base・同じ cron でも tz が変われば結果が変わる（JST 評価の証拠）。
        let base = utc("2026-08-08T21:00:00Z");
        let jst = schedule_next_fire_at("0 7 * * *", "Asia/Tokyo", Some(base), None).unwrap();
        let utc_tz = schedule_next_fire_at("0 7 * * *", "UTC", Some(base), None).unwrap();
        assert_ne!(jst, utc_tz, "tz が発火時刻に効いている");
    }

    #[test]
    fn next_fire_cron_none_base_is_immediate() {
        assert_eq!(
            schedule_next_fire_at("0 7 * * *", "Asia/Tokyo", None, None).unwrap(),
            None
        );
    }
}
