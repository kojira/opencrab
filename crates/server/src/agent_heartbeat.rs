//! エージェントが**自分自身の**ハートビート設定を読み書きするツール（#247 段階 2）。
//!
//! - `get_my_heartbeat`: 現在の有効/無効と間隔、そして境界値（下限・上限・既定）を返す。
//! - `set_my_heartbeat`: 有効/無効と間隔を更新する。
//!
//! # 「自分のだけ」をどう保証しているか
//!
//! **引数に `agent_id` が無い。** 対象は常に `ctx.agent_id`（bridge が実行境界で組む
//! 呼び出し文脈で、ツール引数 JSON からは触れない / `GatewayCallContext` の doc）。
//! さらに `agent_id`（および紛らわしい別名）が引数に現れたら**明示エラーで拒否**する。
//! 無視して黙って自分の設定を書き換えると、エージェントは「他のエージェントの設定を
//! 変えた」と思い込んだままになる。
//!
//! # 権限
//!
//! **オーナー限定にしない**（自分の設定を自分で触れることがこの機能の目的）。
//! ただし素の `Agent`（= 未信頼の外部ユーザー由来のターン）からは見えないよう
//! `TRUSTED_ONLY_ACTIONS` に入れる。エージェントが自分の意思で触るターン
//! （ハートビート tick / ダッシュボード / オーナーとの会話）は全て `Owner` なので
//! 自己設定は妨げられない。逆にここを開けると、外部ユーザーが会話で
//! 「ハートビートを最短で有効にして」と言うだけで自律実行を起動できてしまう。
//! ハンドラ内でも同じ検査をする（多層防御 / `heartbeat_instructions` と同じ流儀）。
//!
//! # 指示文は触らない
//!
//! ハートビートの**指示文**（`update_heartbeat_instructions`）はオーナー限定のまま。
//! 「いつ動くか」を自分で決めるのと「動いたとき何をするか」を自分で書き換えるのは
//! 意味が違う（#247 の設計判断）。
//!
//! # 発火の判定はまだ切り替えない
//!
//! 段階 2 の時点では、実際に tick を発火させているのは従来どおりチャンネル単位の
//! 設定（`discord_channel_config.heartbeat_enabled`）である。ここで保存した値を
//! 発火の判定に使うのは段階 3（別 issue）。ツールの説明文にもその旨を書いてある。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

/// 呼び出し元権限の検査（多層防御）。bridge の `TRUSTED_ONLY_ACTIONS` と同じ範囲。
fn ensure_trusted(ctx: &GatewayCallContext) -> Option<GatewayActionResult> {
    if matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
        return None;
    }
    Some(GatewayActionResult {
        success: false,
        data: None,
        error: Some("このアクションは信頼済みの呼び出し元のみ実行できます".to_string()),
    })
}

/// 他エージェントを指そうとする引数を拒否する。
///
/// このツールは `ctx.agent_id` しか見ないので、指定しても**効かない**。効かない指定を
/// 黙って捨てると「他のエージェントの設定を変えた」という誤解が残るため、明示的に落とす。
fn reject_foreign_target(args: &serde_json::Value) -> Option<GatewayActionResult> {
    for key in ["agent_id", "target_agent_id", "agent"] {
        if args.get(key).is_some() {
            return Some(GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!(
                    "{key}は指定できません（このツールは呼び出し元エージェント自身の設定だけを扱います）"
                )),
            });
        }
    }
    None
}

/// 現在の設定 + 境界値を 1 つの JSON にまとめる。読み出しも更新後も同じ形を返す。
fn state_payload(
    state: &AppState,
    conn: &rusqlite::Connection,
    agent_id: &str,
) -> serde_json::Value {
    let limits = state.heartbeat_limits;
    let resolved = opencrab_db::queries::resolve_agent_heartbeat(
        conn,
        agent_id,
        limits.default_interval_secs,
        limits.min_interval_secs,
    );
    let configured = opencrab_db::queries::get_agent_heartbeat_config(conn, agent_id)
        .ok()
        .flatten()
        .and_then(|r| r.interval_secs);
    json!({
        "agent_id": agent_id,
        "enabled": resolved.enabled,
        "interval_secs": resolved.interval_secs,
        "configured_interval_secs": configured,
        "source": resolved.source,
        "min_interval_secs": limits.effective_min(),
        "max_interval_secs": crate::config::HeartbeatLimits::MAX_INTERVAL_SECS,
        "default_interval_secs": limits.default_interval_secs,
    })
}

/// 自分のハートビート設定を読み出す。
pub(crate) fn get_my_heartbeat(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if let Some(denied) = ensure_trusted(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }

    let conn = state.db.lock().unwrap();
    GatewayActionResult {
        success: true,
        data: Some(state_payload(state, &conn, &ctx.agent_id)),
        error: None,
    }
}

/// 自分のハートビート設定を更新する。
///
/// 下限より短い間隔は**拒否**する（丸めない）。丸めると、エージェントは要求した値で
/// 動いていると思い込んだまま別の間隔で走る。エラーには下限を載せるので、同じターンで
/// 有効な値に直して呼び直せる（このツールを inline 分類にしている理由でもある）。
pub(crate) fn set_my_heartbeat(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if let Some(denied) = ensure_trusted(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }

    let limits = state.heartbeat_limits;
    let min = limits.effective_min();
    let max = crate::config::HeartbeatLimits::MAX_INTERVAL_SECS;

    let enabled_arg = match args.get("enabled") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(_) => {
            return GatewayActionResult {
                success: false,
                data: None,
                error: Some("enabledは真偽値で指定してください".to_string()),
            }
        }
    };

    // 間隔は「指定なし」と「明示的な null（= 既定に戻す）」を区別する。
    let interval_arg: Option<Option<i64>> = match args.get("interval_secs") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(v) => match v.as_i64() {
            Some(secs) if secs > 0 => {
                if (secs as u64) < min {
                    return GatewayActionResult {
                        success: false,
                        data: None,
                        error: Some(format!(
                            "interval_secsが短すぎます（最小{min}秒。指定値: {secs}秒）"
                        )),
                    };
                }
                if (secs as u64) > max {
                    return GatewayActionResult {
                        success: false,
                        data: None,
                        error: Some(format!(
                            "interval_secsが長すぎます（最大{max}秒。指定値: {secs}秒）"
                        )),
                    };
                }
                Some(Some(secs))
            }
            _ => {
                return GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some("interval_secsは正の整数（秒）で指定してください".to_string()),
                }
            }
        },
    };

    if enabled_arg.is_none() && interval_arg.is_none() {
        return GatewayActionResult {
            success: false,
            data: None,
            error: Some("enabled か interval_secs のどちらかが必要です".to_string()),
        };
    }

    let conn = state.db.lock().unwrap();
    let existing = opencrab_db::queries::get_agent_heartbeat_config(&conn, &ctx.agent_id)
        .ok()
        .flatten();
    // 行が無いときの土台は**無効**（#240: 設定を作っただけで自律実行が始まらない）。
    let row = opencrab_db::queries::AgentHeartbeatConfigRow {
        agent_id: ctx.agent_id.clone(),
        enabled: enabled_arg
            .unwrap_or_else(|| existing.as_ref().map(|r| r.enabled).unwrap_or(false)),
        interval_secs: match interval_arg {
            Some(v) => v,
            None => existing.as_ref().and_then(|r| r.interval_secs),
        },
    };

    if let Err(e) = opencrab_db::queries::upsert_agent_heartbeat_config(&conn, &row) {
        return GatewayActionResult {
            success: false,
            data: None,
            error: Some(format!("ハートビート設定の保存に失敗: {e}")),
        };
    }

    tracing::info!(
        agent_id = %ctx.agent_id,
        caller = %ctx.caller.label(),
        old_enabled = existing.as_ref().map(|r| r.enabled),
        old_interval_secs = existing.as_ref().and_then(|r| r.interval_secs),
        new_enabled = row.enabled,
        new_interval_secs = row.interval_secs,
        "エージェント単位のハートビート設定を更新した"
    );

    let mut payload = state_payload(state, &conn, &ctx.agent_id);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
        // 段階 2 では保存するだけ。発火はまだチャンネル単位の設定が決める（段階 3）。
        obj.insert(
            "note".to_string(),
            json!("この設定はまだ発火の判定に使われていない（段階3で切り替え予定）。現在の発火はチャンネル単位の設定が決める。"),
        );
    }
    GatewayActionResult {
        success: true,
        data: Some(payload),
        error: None,
    }
}
