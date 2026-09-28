//! エージェントが**自分自身の**時間トリガー（#455 / #612）を登録・照会するツール。
//!
//! 間隔実行（`@every 30m`）も定時実行（cron）も同じ `agent_schedules` の 1 行で、同じセッションに
//! 複数登録でき、行ごとに `message`（発火時に自分へ渡されるプロンプト）を持つ（#612）。
//!
//! - `set_my_schedule`: いま話しているセッションに対して cron / `@every` のスケジュールを登録する。
//! - `get_my_schedules`: いま話しているセッションのスケジュールを、次回発火時刻付きで列挙する。
//! - `update_my_schedule`: `get_my_schedules` が返した id のスケジュールを部分更新する
//!   （`enabled=false` で「止める」・cron/message/timezone の変更で「間隔を変える」）。
//! - `delete_my_schedule`: `get_my_schedules` が返した id のスケジュールを消す（履歴も残さない）。
//! - `run_my_schedule`: `get_my_schedules` が返した id のスケジュールを今すぐ手動発火する
//!   （**オーナー / co_agent 限定**・定時発火と同じ経路・`last_fired_at` は更新しない）。
//!
//! # id の所属チェック（#477）
//!
//! `update_my_schedule` / `delete_my_schedule` / `run_my_schedule` は id を取る。**id を推測して
//! 他エージェント・他セッションのスケジュールを触れてはいけない**ので、対象行は `ctx.agent_id`＋
//! 現在のセッションの両方に一致する場合だけ操作できる（`api::schedules` 側の `load_owned_schedule`
//! が所属チェックを握る）。一致しない／存在しない id は**存在を明かさず**同じ文言で拒否する。
//!
//! # なぜエージェント自身に開くか（設計 §7.4 の制約撤回・オーナー裁定 2026-08-09）
//!
//! **sample-source の巡回指示ループを閉じる**ため（巡回指示が webhook で届いても本人がスケジュールを
//! 作れないと、毎回オーナーが dashboard から登録することになる）。**増えるのは「何ができるか」では
//! なく「いつ動くかを自分で決められるか」だけ**。
//!
//! # セッション単位（#456）
//!
//! **スコープは無い。** 対象は常に `ctx.session_id`（いま話しているセッション）。発火経路を持つのは
//! 登録済み transport のセッションだけ（#628）なので、それ以外で呼ばれたら **fail-closed で拒否し
//! remedy（どこで実行すればよいか）を返す**。「設定できたのに永遠に発火しない行」を作らせない。
//!
//! # 権限
//!
//! get/set/update/delete は **owner 限定にはしない**（自分の定時実行を自分で決めるのが目的）が、
//! 素の `Agent`（未信頼の外部ユーザー由来ターン）からは見えないよう `TRUSTED_ONLY_ACTIONS` に入れ、
//! ハンドラ内でも同じ検査をする（多層防御）。`run_my_schedule` は `OWNER_ONLY_ACTIONS`。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::api::schedules::{
    create_schedule_core, delete_schedule_core, list_session_schedules_core, load_owned_schedule,
    update_schedule_core, ScheduleOpError, SchedulePatch,
};
use crate::AppState;

/// 呼び出し元権限の検査（多層防御）。bridge の `TRUSTED_ONLY_ACTIONS` と同じ範囲。
fn ensure_trusted(ctx: &GatewayCallContext) -> Option<GatewayActionResult> {
    if matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
        return None;
    }
    Some(err("このアクションは信頼済みの呼び出し元のみ実行できます"))
}

/// 呼び出し元権限の検査（多層防御）。bridge の `OWNER_ONLY_ACTIONS` と同じ範囲
/// （オーナー / co_agent のみ）。`run_my_schedule` 用。
fn ensure_owner_or_coagent(ctx: &GatewayCallContext) -> Option<GatewayActionResult> {
    if matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. }
    ) {
        return None;
    }
    Some(err(
        "このアクションはオーナーまたは co_agent のみ実行できます",
    ))
}

/// 他エージェントを指そうとする引数を拒否する（このツールは `ctx.agent_id` しか見ない）。
fn reject_foreign_target(args: &serde_json::Value) -> Option<GatewayActionResult> {
    for key in ["agent_id", "target_agent_id", "agent"] {
        if args.get(key).is_some() {
            return Some(err(format!(
                "{key}は指定できません（このツールは呼び出し元エージェント自身のスケジュールだけを扱います）"
            )));
        }
    }
    None
}

/// 廃止したスコープ引数（`scope` / `channel_id` / `guild_id`）を拒否する（#456 と同じ語彙統一）。
fn reject_removed_scope_args(args: &serde_json::Value) -> Option<GatewayActionResult> {
    for key in ["scope", "channel_id", "guild_id", "session_id"] {
        if args.get(key).is_some() {
            return Some(err(format!(
                "{key}は指定できません。スケジュールは常に「いま話しているセッション」に対して登録・照会されます。"
            )));
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

/// 現在のセッションを発火先へ解決する（scheduler と**同じ登録簿を引く**・#628）。
///
/// セッション文脈が無い / 発火経路の無い種別（登録済み descriptor がどれも名乗らない）→
/// fail-closed で **remedy 付き**エラー。
fn current_session_target(
    state: &AppState,
    ctx: &GatewayCallContext,
) -> Result<(String, opencrab_actions::FireTarget), GatewayActionResult> {
    let session_id = match ctx.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            // remedy は登録済み transport から生成する（#628・手書きしない）。
            return Err(err(format!(
                "このセッションからは定時実行を設定・照会できません（セッション文脈がありません）。設定したい対象のセッション——{}——で実行してください。",
                state.timed_fire_router.fire_target_hint()
            )));
        }
    };
    let target = {
        let conn = state.db.lock().map_err(|error| {
            tracing::error!(%error, agent_id = %ctx.agent_id, "schedule: persisted target DB lock failed");
            err("定時実行の発火先を確認できませんでした。しばらくしてから再試行してください")
        })?;
        state
            .timed_fire_router
            .resolve_persisted_target(&conn, session_id, &ctx.agent_id)
    };
    match target {
        Some(target) => Ok((session_id.to_string(), target)),
        None => Err(err(format!(
            "このセッションからは定時実行を設定・照会できません（このセッション種別には発火経路がありません）。設定したい対象のセッション——{}——で実行してください。",
            state.timed_fire_router.fire_target_hint()
        ))),
    }
}

/// 現在のセッションが発火経路を持つかを確認し、session_id を返す（[`current_session_target`]）。
fn current_session(
    state: &AppState,
    ctx: &GatewayCallContext,
) -> Result<String, GatewayActionResult> {
    current_session_target(state, ctx).map(|(session_id, _)| session_id)
}

/// 必須の整数 id 引数を取り出す（数値、または数値へ解釈できる文字列を許す）。
///
/// LLM が id を文字列で渡す実測があるので、`"5"` のような整数文字列も受ける（暗黙の
/// フォールバックではなく素直な入力解釈）。欠落・非整数・型違いは remedy 付きエラー。
fn required_i64(args: &serde_json::Value, key: &str) -> Result<i64, GatewayActionResult> {
    match args.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_i64().ok_or_else(|| {
            err(format!(
                "{key}は整数で指定してください（get_my_schedules が返した id をそのまま渡してください）。"
            ))
        }),
        Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().map_err(|_| {
            err(format!(
                "{key}は整数で指定してください（get_my_schedules が返した id をそのまま渡してください）。"
            ))
        }),
        _ => Err(err(format!(
            "{key}は必須です。get_my_schedules が返した id をそのまま渡してください。"
        ))),
    }
}

/// 必須の文字列引数を取り出す（欠落 / 空 / 型違いは remedy 付きエラー）。
fn required_str(
    args: &serde_json::Value,
    key: &str,
    remedy: &str,
) -> Result<String, GatewayActionResult> {
    match args.get(key) {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        _ => Err(err(format!("{key}は必須です。{remedy}"))),
    }
}

/// 自分の定時実行スケジュールを登録する（常に現在のセッションが対象）。
///
/// cron 式が不正ならその場でエラー（実行時に黙って発火しないのが最悪なので、同じターンで直せる）。
/// 成功後は中央スケジューラを起こして再起動なしで即時反映する（#437・共有コアが担う）。
pub(crate) fn set_my_schedule(
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
    if let Some(denied) = reject_removed_scope_args(args) {
        return denied;
    }

    let session_id = match current_session(state, ctx) {
        Ok(s) => s,
        Err(e) => return e,
    };

    let cron_expr = match required_str(
        args,
        "cron_expr",
        "cron 5 フィールド（例: 0 7 * * * = 毎朝 7 時）か @every 形式（例: @every 3h）で指定してください。",
    ) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let message = match required_str(
        args,
        "message",
        "発火時にエージェント（自分）へ渡す指示文を書いてください（例: ニュースを巡回してまとめを書く）。",
    ) {
        Ok(s) => s,
        Err(e) => return e,
    };
    // enabled は省略時 true（sample-source: 「登録したらそのまま回る」）。型違いは拒否。
    let enabled = match args.get("enabled") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Bool(b)) => *b,
        Some(_) => return err("enabledは真偽値で指定してください"),
    };
    // timezone は省略時 Asia/Tokyo。
    let timezone = match args.get("timezone") {
        None | Some(serde_json::Value::Null) => "Asia/Tokyo".to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(_) => return err("timezoneは文字列（IANA 名・例 Asia/Tokyo）で指定してください"),
    };

    match create_schedule_core(
        state,
        &ctx.agent_id,
        &session_id,
        &cron_expr,
        &timezone,
        &message,
        enabled,
    ) {
        Ok(dto) => {
            tracing::info!(
                agent_id = %ctx.agent_id,
                session_id = %session_id,
                schedule_id = dto.id,
                caller = %ctx.caller.label(),
                "エージェントが自分の定時実行スケジュールを登録した"
            );
            let mut data = serde_json::to_value(&dto).unwrap_or_else(|_| json!({}));
            if let Some(obj) = data.as_object_mut() {
                obj.insert("success".to_string(), json!(true));
            }
            GatewayActionResult {
                success: true,
                data: Some(data),
                error: None,
            }
        }
        // BadRequest の文言は remedy を含む（cron 不正・発火経路なし・message 空）。
        Err(ScheduleOpError::BadRequest(m)) | Err(ScheduleOpError::Internal(m)) => err(m),
    }
}

/// 自分の定時実行スケジュールを列挙する（常に現在のセッションが対象）。
pub(crate) fn get_my_schedules(
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

    let session_id = match current_session(state, ctx) {
        Ok(s) => s,
        Err(e) => return e,
    };

    match list_session_schedules_core(state, &ctx.agent_id, &session_id) {
        Ok(schedules) => {
            let count = schedules.len();
            GatewayActionResult {
                success: true,
                data: Some(json!({
                    "session_id": session_id,
                    "schedules": schedules,
                    "count": count,
                })),
                error: None,
            }
        }
        Err(ScheduleOpError::BadRequest(m)) | Err(ScheduleOpError::Internal(m)) => err(m),
    }
}

/// 自分の定時実行スケジュールを **id 指定で**部分更新する（常に現在のセッションが対象）。
///
/// `id` は `get_my_schedules` が返したもの。所属チェック（`ctx.agent_id`＋現在のセッション）を
/// 通った行だけを更新できる。`enabled=false` で「止める」（行は残り履歴が追える）、cron/message/
/// timezone の変更で「間隔を変える」。cron 式が不正ならその場でエラー（同ターンで直せる）。
pub(crate) fn update_my_schedule(
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
    // session_id 等のスコープ引数は禁止（対象は常に現在のセッション・付け替えさせない）。
    if let Some(denied) = reject_removed_scope_args(args) {
        return denied;
    }

    let session_id = match current_session(state, ctx) {
        Ok(s) => s,
        Err(e) => return e,
    };

    let id = match required_i64(args, "id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    // 変更フィールドは任意（省略時は現状維持）。型違いは拒否。
    let cron_expr = match args.get("cron_expr") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return err("cron_exprは文字列で指定してください（省略すると現在の値を保ちます）")
        }
    };
    let message = match args.get("message") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => return err("messageは文字列で指定してください（省略すると現在の値を保ちます）"),
    };
    let timezone = match args.get("timezone") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => return err("timezoneは文字列（IANA 名・例 Asia/Tokyo）で指定してください"),
    };
    let enabled = match args.get("enabled") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Bool(b)) => Some(*b),
        Some(_) => return err("enabledは真偽値で指定してください"),
    };

    // 変更項目が 1 つも無い呼び出しは、何も起きないのに成功に見えるので拒否する（暗黙の no-op を作らない）。
    if cron_expr.is_none() && message.is_none() && timezone.is_none() && enabled.is_none() {
        return err(
            "変更する項目を 1 つ以上指定してください（cron_expr / message / timezone / enabled）。止めたいだけなら enabled=false、消すなら delete_my_schedule を使ってください。",
        );
    }

    match update_schedule_core(
        state,
        &ctx.agent_id,
        &session_id,
        id,
        SchedulePatch {
            cron_expr: cron_expr.as_deref(),
            timezone: timezone.as_deref(),
            message: message.as_deref(),
            enabled,
        },
    ) {
        Ok(dto) => {
            tracing::info!(
                agent_id = %ctx.agent_id,
                session_id = %session_id,
                schedule_id = id,
                enabled = dto.enabled,
                caller = %ctx.caller.label(),
                "エージェントが自分の定時実行スケジュールを更新した"
            );
            let mut data = serde_json::to_value(&dto).unwrap_or_else(|_| json!({}));
            if let Some(obj) = data.as_object_mut() {
                obj.insert("success".to_string(), json!(true));
            }
            GatewayActionResult {
                success: true,
                data: Some(data),
                error: None,
            }
        }
        Err(ScheduleOpError::BadRequest(m)) | Err(ScheduleOpError::Internal(m)) => err(m),
    }
}

/// 自分の定時実行スケジュールを **id 指定で**削除する（常に現在のセッションが対象）。
///
/// `id` は `get_my_schedules` が返したもの。所属チェックを通った行だけを消せる。「止めるだけ」で
/// 履歴を残したいなら `update_my_schedule` に `enabled=false` を渡す（削除は行ごと消す）。
pub(crate) fn delete_my_schedule(
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
    if let Some(denied) = reject_removed_scope_args(args) {
        return denied;
    }

    let session_id = match current_session(state, ctx) {
        Ok(s) => s,
        Err(e) => return e,
    };

    let id = match required_i64(args, "id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    match delete_schedule_core(state, &ctx.agent_id, &session_id, id) {
        Ok(()) => {
            tracing::info!(
                agent_id = %ctx.agent_id,
                session_id = %session_id,
                schedule_id = id,
                caller = %ctx.caller.label(),
                "エージェントが自分の定時実行スケジュールを削除した"
            );
            GatewayActionResult {
                success: true,
                data: Some(json!({
                    "success": true,
                    "id": id,
                    "message": "スケジュールを削除しました",
                })),
                error: None,
            }
        }
        Err(ScheduleOpError::BadRequest(m)) | Err(ScheduleOpError::Internal(m)) => err(m),
    }
}

/// 自分の定時実行スケジュールを **id 指定で**、次の発火時刻を待たずに手動発火する
/// （#612 D2・オーナー / co_agent 限定）。
///
/// # 定時発火とまったく同じ経路
///
/// scheduler の定時発火と**同じ関数**（[`crate::heartbeat_fire::run_one_heartbeat`]）を呼ぶ。
/// 対象は所属チェック（[`load_owned_schedule`]・`ctx.agent_id`＋現在のセッション）を通った行だけ。
///
/// # `last_fired_at` は更新しない
///
/// 手動発火は定時発火の位相をずらさないため `last_fired_at` を刻まない（刻むのはスケジューラの
/// 発火ループだけ・I2）。
///
/// # 自己デッドロックを避ける（#599）
///
/// このツールは呼び出しターンの中で走り、そのターンは既に現在セッションの直列化ロックを保持して
/// いる。発火は `spawn` して**即座に「投げた」を返し**、実際のターンは今のターンが終わってから走る。
pub(crate) fn run_my_schedule(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    // owner_only（bridge の OWNER_ONLY_ACTIONS と同ポリシー）を handler でも確認する（多層防御）。
    if let Some(denied) = ensure_owner_or_coagent(ctx) {
        return denied;
    }
    if let Some(denied) = reject_foreign_target(args) {
        return denied;
    }
    if let Some(denied) = reject_removed_scope_args(args) {
        return denied;
    }

    let (session_id, target) = match current_session_target(state, ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let id = match required_i64(args, "id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    let row = match load_owned_schedule(state, &ctx.agent_id, &session_id, id) {
        Ok(row) => row,
        Err(ScheduleOpError::BadRequest(m)) | Err(ScheduleOpError::Internal(m)) => return err(m),
    };

    if !state.timed_fire_router.has_live_sink() {
        return err("ゲートウェイが稼働していないため発火できません（受け口が未登録）。ゲートウェイの起動を確認してください。");
    }

    let fire_state = state.clone();
    let fire_agent_id = ctx.agent_id.clone();
    tokio::spawn(async move {
        crate::heartbeat_fire::run_one_heartbeat(
            &fire_state,
            &fire_agent_id,
            &target,
            id,
            &row.message,
        )
        .await;
    });

    tracing::info!(
        agent_id = %ctx.agent_id,
        session_id = %session_id,
        schedule_id = id,
        caller = %ctx.caller.label(),
        "run_my_schedule: 手動でスケジュールを発火した（last_fired_at は更新しない）"
    );

    GatewayActionResult {
        success: true,
        data: Some(json!({
            "fired": true,
            "id": id,
            "session_id": session_id,
            "note": "スケジュールを発火しました。実際のターンは今のターンが終わってから同じセッションで走ります（last_fired_at は更新しません）。",
        })),
        error: None,
    }
}

#[cfg(test)]
#[path = "agent_schedule/review_tests.rs"]
mod review_tests;

#[cfg(test)]
mod tests;
