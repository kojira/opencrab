//! エージェントが**自分自身の**ハートビート設定を読み書きするツール（#247 / #336）。
//!
//! - `get_my_heartbeat`: 現在の有効/無効と間隔、そして境界値（下限・上限・既定）を返す。
//! - `set_my_heartbeat`: 有効/無効と間隔を更新する。
//!
//! # スコープ（#336）
//!
//! 既定は `scope="agent"`（エージェント単位、`agent_heartbeat_config`）。`scope="channel"`
//! を渡すと **そのチャンネルの** 有効/無効と間隔（`discord_channel_config` の
//! `heartbeat_enabled` / `heartbeat_interval_secs`）を触る。指示文
//! （`read/update_heartbeat_instructions`）の scope の扱いに揃えてある（channel scope は
//! `discord_channel_config` を読み書きする）。チャンネル発火時の間隔は
//! `resolve_channel_heartbeat_interval`（channel → agent → 運用者既定、下限クランプ）で
//! 解決される。
//!
//! 発火形態の関係（重要）: エージェント単位で `enabled=true` にすると発火は**エージェント
//! 単位で 1 回**（空 `channel_id` の単一発話。Discord の代表チャンネル選択は別 PR / 現状は
//! ログのみに縮退）になり、チャンネル単位設定は使われない（#238 の
//! precedence）。チャンネルごとに間隔・有効を効かせたい場合はエージェント単位を
//! `enabled` にせず、`scope="channel"` で各チャンネルを設定する。Nostr など channel の
//! 概念が無い gateway は agent スコープのまま（channel scope は Discord 用）。
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

/// `error` だけを持つ失敗レスポンスを組む短縮子。
fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

/// `scope` を読む（既定は `"agent"`）。
fn scope_arg(args: &serde_json::Value) -> &str {
    args.get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("agent")
}

/// `enabled` 引数を解釈する。`None` = 未指定 / `Some(b)` = 明示。型違いは Err。
fn parse_enabled_arg(args: &serde_json::Value) -> Result<Option<bool>, GatewayActionResult> {
    match args.get("enabled") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(err("enabledは真偽値で指定してください")),
    }
}

/// `interval_secs` 引数を解釈する。
/// - `None` = 未指定（現在値を保つ）
/// - `Some(None)` = 明示 null（= 運用者既定に戻す）
/// - `Some(Some(secs))` = 明示値（下限・上限・正整数を検証済み）
///
/// 下限より短い / 上限より長い / 非正整数は**拒否**する（丸めない）。agent / channel の
/// どちらの scope でも同じ床・天井を効かせる（#336 決定3: 下限をチャンネル単位でも維持）。
fn parse_interval_arg(
    args: &serde_json::Value,
    min: u64,
    max: u64,
) -> Result<Option<Option<i64>>, GatewayActionResult> {
    match args.get("interval_secs") {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(v) => match v.as_i64() {
            Some(secs) if secs > 0 => {
                if (secs as u64) < min {
                    return Err(err(format!(
                        "interval_secsが短すぎます（最小{min}秒。指定値: {secs}秒）"
                    )));
                }
                if (secs as u64) > max {
                    return Err(err(format!(
                        "interval_secsが長すぎます（最大{max}秒。指定値: {secs}秒）"
                    )));
                }
                Ok(Some(Some(secs)))
            }
            _ => Err(err("interval_secsは正の整数（秒）で指定してください")),
        },
    }
}

/// agent scope の現在の設定 + 境界値を 1 つの JSON にまとめる。読み出しも更新後も同じ形。
fn agent_state_payload(
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
        "scope": "agent",
        "enabled": resolved.enabled,
        "interval_secs": resolved.interval_secs,
        "configured_interval_secs": configured,
        "source": resolved.source,
        "min_interval_secs": limits.effective_min(),
        "max_interval_secs": crate::config::HeartbeatLimits::MAX_INTERVAL_SECS,
        "default_interval_secs": limits.default_interval_secs,
    })
}

/// channel scope の現在の設定 + 境界値をまとめる（#336）。`enabled` / `configured_interval_secs`
/// は当該エージェントの `(channel_id, agent_id)` 行の値（行が無ければ enabled=false /
/// interval=null）。`interval_secs` は `resolve_channel_heartbeat_interval`（channel → agent →
/// 既定, 下限クランプ）の実効値。
///
/// 注意（既存 precedence / #238）: ここは `(channel_id, agent_id)` 固有行だけを読む。global 行
/// （agent_id=""）しか無いチャンネルでは `enabled=false` を返すが、実発火は
/// `list_whitelisted_heartbeat_channels` 経由で global 行により起こりうる（get の表示と実発火が
/// 乖離する edge）。本 PR では precedence を変えないため、この乖離は残る。
fn channel_state_payload(
    state: &AppState,
    conn: &rusqlite::Connection,
    agent_id: &str,
    channel_id: &str,
) -> serde_json::Value {
    let limits = state.heartbeat_limits;
    let existing = opencrab_db::queries::get_channel_config_for_agent(conn, channel_id, agent_id)
        .ok()
        .flatten();
    let configured = existing.as_ref().and_then(|c| c.heartbeat_interval_secs);
    let enabled = existing
        .as_ref()
        .map(|c| c.heartbeat_enabled)
        .unwrap_or(false);
    let resolved = opencrab_db::queries::resolve_channel_heartbeat_interval(
        conn,
        agent_id,
        configured,
        limits.default_interval_secs,
        limits.min_interval_secs,
    );
    json!({
        "agent_id": agent_id,
        "scope": "channel",
        "channel_id": channel_id,
        "enabled": enabled,
        "interval_secs": resolved.interval_secs,
        "configured_interval_secs": configured,
        "source": resolved.source,
        "min_interval_secs": limits.effective_min(),
        "max_interval_secs": crate::config::HeartbeatLimits::MAX_INTERVAL_SECS,
        "default_interval_secs": limits.default_interval_secs,
    })
}

/// 自分のハートビート設定を読み出す。`scope="channel"` なら当該チャンネルの設定を返す。
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
    match scope_arg(args) {
        "agent" => GatewayActionResult {
            success: true,
            data: Some(agent_state_payload(state, &conn, &ctx.agent_id)),
            error: None,
        },
        "channel" => {
            let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
                Some(id) if !id.is_empty() => id,
                _ => return err("scope=channelのときはchannel_idが必要です"),
            };
            GatewayActionResult {
                success: true,
                data: Some(channel_state_payload(
                    state,
                    &conn,
                    &ctx.agent_id,
                    channel_id,
                )),
                error: None,
            }
        }
        other => err(format!("不明なscope: {other}（agent または channel）")),
    }
}

/// 自分のハートビート設定を更新する。`scope="channel"` なら当該チャンネルの設定を更新する。
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

    let enabled_arg = match parse_enabled_arg(args) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let interval_arg = match parse_interval_arg(args, min, max) {
        Ok(v) => v,
        Err(e) => return e,
    };

    if enabled_arg.is_none() && interval_arg.is_none() {
        return err("enabled か interval_secs のどちらかが必要です");
    }

    match scope_arg(args) {
        "agent" => set_agent_scope(state, ctx, enabled_arg, interval_arg),
        "channel" => set_channel_scope(state, args, ctx, enabled_arg, interval_arg),
        other => err(format!("不明なscope: {other}（agent または channel）")),
    }
}

/// agent scope の更新（`agent_heartbeat_config`）。
fn set_agent_scope(
    state: &AppState,
    ctx: &GatewayCallContext,
    enabled_arg: Option<bool>,
    interval_arg: Option<Option<i64>>,
) -> GatewayActionResult {
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
        return err(format!("ハートビート設定の保存に失敗: {e}"));
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

    let mut payload = agent_state_payload(state, &conn, &ctx.agent_id);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
        // #336: agent スコープの enabled は「エージェント単位発火（有効時はエージェント
        // 単位で 1 回、空 channel_id の単一発話）」を意味する。チャンネルごとに間隔・有効を分けたいなら
        // scope=channel を使う（agent を enabled にすると channel 設定は使われない）。
        obj.insert(
            "note".to_string(),
            json!("agent スコープを enabled にするとエージェント単位で発火する（有効時は 1 回のみ、空 channel_id の単一発話）。チャンネルごとに間隔・有効を分けたい場合は scope=channel で設定する。"),
        );
    }
    GatewayActionResult {
        success: true,
        data: Some(payload),
        error: None,
    }
}

/// channel scope の更新（`discord_channel_config` の `heartbeat_enabled` /
/// `heartbeat_interval_secs`）。指示文の scope=channel と同じテーブルを触る（#336）。
///
/// 既存行があればその設定を尊重して該当カラムだけ上書きし、無ければ既定値で新規作成する
/// （新規作成には `guild_id` が要る。指示文 scope=channel と同じ流儀）。
fn set_channel_scope(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
    enabled_arg: Option<bool>,
    interval_arg: Option<Option<i64>>,
) -> GatewayActionResult {
    let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => id,
        _ => return err("scope=channelのときはchannel_idが必要です"),
    };

    let conn = state.db.lock().unwrap();
    let existing =
        opencrab_db::queries::get_channel_config_for_agent(&conn, channel_id, &ctx.agent_id)
            .ok()
            .flatten();

    // guild_id は下の None ブランチ（新規作成）だけが使う。既存行は自分の guild_id を
    // そのまま保持する（Some(mut c) ブランチは guild_id を触らない）ので、ここで existing
    // から補完する必要はない。
    let guild_id = args
        .get("guild_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let old_enabled = existing.as_ref().map(|c| c.heartbeat_enabled);
    let old_interval = existing.as_ref().and_then(|c| c.heartbeat_interval_secs);

    // interval_arg を channel 行の `Option<u64>` へ落とす。
    //   Some(Some(secs)) = 明示値 / Some(None) = 既定に戻す(NULL) / None = 現在値を保つ
    let next_interval: Option<u64> = match interval_arg {
        Some(v) => v.map(|secs| secs as u64),
        None => old_interval,
    };

    let cfg = match existing {
        Some(mut c) => {
            if let Some(e) = enabled_arg {
                c.heartbeat_enabled = e;
            }
            c.heartbeat_interval_secs = next_interval;
            c
        }
        None => {
            let guild_id = match guild_id {
                Some(g) if !g.is_empty() => g,
                _ => return err("新規チャンネル設定の作成にはguild_idが必要です"),
            };
            opencrab_db::queries::ChannelConfigRow {
                channel_id: channel_id.to_string(),
                agent_id: ctx.agent_id.clone(),
                guild_id,
                channel_name: String::new(),
                readable: true,
                writable: true,
                whitelisted: false,
                // 新規行の既定は有効（discord_channel_config の既定・指示文 scope=channel と
                // 同じ）。enabled 明示があればそれを尊重する。
                heartbeat_enabled: enabled_arg.unwrap_or(true),
                heartbeat_interval_secs: next_interval,
                heartbeat_instructions: String::new(),
            }
        }
    };

    if let Err(e) = opencrab_db::queries::upsert_channel_config(&conn, &cfg) {
        return err(format!("チャンネルのハートビート設定の保存に失敗: {e}"));
    }

    tracing::info!(
        agent_id = %ctx.agent_id,
        channel_id = %channel_id,
        caller = %ctx.caller.label(),
        old_enabled = old_enabled,
        old_interval_secs = old_interval,
        new_enabled = cfg.heartbeat_enabled,
        new_interval_secs = cfg.heartbeat_interval_secs,
        "チャンネル単位のハートビート設定を更新した"
    );

    let mut payload = channel_state_payload(state, &conn, &ctx.agent_id, channel_id);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("success".to_string(), json!(true));
    }
    GatewayActionResult {
        success: true,
        data: Some(payload),
        error: None,
    }
}
