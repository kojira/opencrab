//! エージェントが**自分自身の**時間トリガー（#455 / #612）を登録・照会するツール。
//!
//! 間隔実行（`@every 30m`）も定時実行（cron）も同じ `agent_schedules` の 1 行で、同じセッションに
//! 複数登録でき、行ごとに `message`（発火時に自分へ渡されるプロンプト）を持つ（#612）。
//!
//! - `set_my_schedule`: cron / `@every` のスケジュールを登録する。既定はいま話しているセッション、
//!   `session_id` を渡すと自分の別のセッションに登録する（D-1147）。
//! - `get_my_schedules`: 自分の全セッションのスケジュールを、次回発火時刻・セッション名付きで列挙する
//!   （D-1147。`session_id` で絞り込める）。
//! - `update_my_schedule`: `get_my_schedules` が返した id のスケジュールを部分更新する
//!   （`enabled=false` で「止める」・cron/message/timezone の変更で「間隔を変える」）。
//! - `delete_my_schedule`: `get_my_schedules` が返した id のスケジュールを消す（履歴も残さない）。
//! - `run_my_schedule`: `get_my_schedules` が返した id のスケジュールを今すぐ手動発火する
//!   （**オーナー / co_agent 限定**・定時発火と同じ経路・`last_fired_at` は更新しない）。
//!
//! # id の所属チェック（#477 / D-1147）
//!
//! `update_my_schedule` / `delete_my_schedule` / `run_my_schedule` は id を取る。**id を推測して
//! 他エージェントのスケジュールを触れてはいけない**ので、対象行は `ctx.agent_id` に一致する場合だけ
//! 操作できる（`api::schedules` 側の `load_owned_schedule` が所属チェックを握る）。どのセッションの行
//! でもよく、変更はその行のセッションに対して効く（発火先は行のセッションのまま）。一致しない／
//! 存在しない id は**存在を明かさず**同じ文言で拒否する。
//!
//! # なぜエージェント自身に開くか（設計 §7.4 の制約撤回・オーナー裁定 2026-08-09）
//!
//! **sample-source の巡回指示ループを閉じる**ため（巡回指示が webhook で届いても本人がスケジュールを
//! 作れないと、毎回オーナーが dashboard から登録することになる）。**増えるのは「何ができるか」では
//! なく「いつ動くかを自分で決められるか」だけ**。
//!
//! # 発火経路（#628）
//!
//! 登録先は自分の、発火経路を持つセッション（登録済み transport のセッション）だけ。判定は scheduler と
//! 同じ `resolve_persisted_target` 1 か所。それ以外を指したら **fail-closed で拒否し remedy を返す**
//! （「設定できたのに永遠に発火しない行」を作らせない）。
//!
//! # 権限
//!
//! get/set/update/delete は **owner 限定にはしない**（自分の定時実行を自分で決めるのが目的）が、
//! 素の `Agent`（未信頼の外部ユーザー由来ターン）からは見えないよう `TRUSTED_ONLY_ACTIONS` に入れ、
//! ハンドラ内でも同じ検査をする（多層防御）。`run_my_schedule` は `OWNER_ONLY_ACTIONS`。
//! サブタスクは親ターンの呼び出し元を継承するので、親と同じ範囲で扱える。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::api::schedules::{
    create_schedule_core, delete_schedule_core, list_agent_schedules_core, load_owned_schedule,
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

/// 廃止したスコープ引数（`scope`）を拒否する（#456 と同じ語彙統一）。
fn reject_scope_arg(args: &serde_json::Value) -> Option<GatewayActionResult> {
    args.get("scope").map(|_| {
        err("scopeは指定できません。対象のセッションは session_id（set_my_schedule / get_my_schedules）か id で指定してください。")
    })
}

/// id で対象を決めるツール（update/delete/run）に `session_id` が来たら拒否する。対象のセッションは
/// id の行が持っているので、別の値を受けると「どちらが効いたか」が曖昧になる。
fn reject_session_id_with_id(args: &serde_json::Value) -> Option<GatewayActionResult> {
    // 未指定（省略・null・空文字）は拒否しない。値が入っているときだけ曖昧なので断る。
    match args.get("session_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => None,
        Some(_) => Some(err("session_idは指定できません。id のスケジュールはそれぞれ自分のセッションを持っているので、id だけを渡してください。")),
    }
}

/// 任意の文字列引数を取り出す。省略・null・空文字は `None`（LLM は「指定しない」を `""` で
/// 送ることが多く、ほかのツールも `""` を未指定として扱う）。文字列以外は remedy 付きエラー。
fn optional_str(
    args: &serde_json::Value,
    key: &str,
    remedy: &str,
) -> Result<Option<String>, GatewayActionResult> {
    match args.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(err(format!("{key}は文字列で指定してください。{remedy}"))),
    }
}

/// `error` だけを持つ失敗レスポンスを組む短縮子。
fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

/// このエージェントの `session_id` を発火先へ解決する（scheduler と**同じ登録簿を引く**・#628）。
///
/// 自分のセッションでない / 発火経路の無いセッション → fail-closed で **remedy 付き**エラー。
fn session_target(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
) -> Result<opencrab_actions::FireTarget, GatewayActionResult> {
    let target = {
        let conn = state.db.lock().map_err(|error| {
            tracing::error!(%error, agent_id, "schedule: persisted target DB lock failed");
            err("定時実行の発火先を確認できませんでした。しばらくしてから再試行してください")
        })?;
        state
            .timed_fire_router
            .resolve_persisted_target(&conn, session_id, agent_id)
    };
    target.ok_or_else(|| {
        err(format!(
            "セッション {session_id} には定時実行を設定できません（あなたのセッションでないか、このセッション種別には発火経路がありません）。対象にできるのは、あなたの——{}——のセッションです（get_my_schedules で各スケジュールの session_id を確認できます）。",
            state.timed_fire_router.fire_target_hint()
        ))
    })
}

/// 登録先セッションを決める: `session_id` 引数があればそれ、無ければ現在のセッション。
/// どちらも [`session_target`] で「自分の・発火経路のあるセッション」かを確かめる。
fn registration_session(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> Result<String, GatewayActionResult> {
    let requested = optional_str(
        args,
        "session_id",
        "省略するといま話しているセッションに登録します。",
    )?;
    let session_id = match requested {
        Some(s) => s,
        None => match ctx.session_id.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return Err(err(format!(
                    "このセッションからは登録先が決まりません（セッション文脈がありません）。session_id に、あなたの——{}——のセッションを指定してください。",
                    state.timed_fire_router.fire_target_hint()
                )))
            }
        },
    };
    session_target(state, &ctx.agent_id, &session_id)?;
    Ok(session_id)
}

/// セッションの人が読める名前（`sessions.theme`）。名前が無い（空・id と同じ）ときは `None`。
fn session_title(conn: &rusqlite::Connection, session_id: &str) -> Option<String> {
    opencrab_db::queries::get_session(conn, session_id)
        .ok()
        .flatten()
        .map(|s| s.theme)
        .filter(|theme| !theme.trim().is_empty() && theme != session_id)
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

/// 自分の定時実行スケジュールを登録する（既定は現在のセッション、`session_id` で自分の別セッション）。
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
    if let Some(denied) = reject_scope_arg(args) {
        return denied;
    }

    let session_id = match registration_session(state, args, ctx) {
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

/// 自分の定時実行スケジュールを、自分の全セッション分列挙する（`session_id` で絞り込める）。
///
/// 各要素に `session_title`（セッション名・無ければ null）と `is_current_session` を足し、
/// 応答に `current_session_id` を載せる。発火経路の無いセッション（サブタスク等）からでも読める。
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
    if let Some(denied) = reject_scope_arg(args) {
        return denied;
    }
    let filter = match optional_str(
        args,
        "session_id",
        "省略すると全セッションのスケジュールを返します。",
    ) {
        Ok(v) => v,
        Err(e) => return e,
    };

    let schedules = match list_agent_schedules_core(state, &ctx.agent_id) {
        Ok(v) => v,
        Err(ScheduleOpError::BadRequest(m)) | Err(ScheduleOpError::Internal(m)) => return err(m),
    };
    let current_session_id = ctx.session_id.clone().filter(|s| !s.is_empty());
    let mut titles: std::collections::HashMap<String, Option<String>> =
        std::collections::HashMap::new();
    let items: Vec<serde_json::Value> = {
        let conn = match state.db.lock() {
            Ok(conn) => conn,
            Err(_) => {
                return err(
                    "スケジュールを読み出せませんでした。しばらくしてから再試行してください",
                )
            }
        };
        schedules
            .into_iter()
            .filter(|dto| filter.as_deref().is_none_or(|f| dto.session_id == f))
            .map(|dto| {
                let title = titles
                    .entry(dto.session_id.clone())
                    .or_insert_with(|| session_title(&conn, &dto.session_id))
                    .clone();
                let is_current = current_session_id.as_deref() == Some(dto.session_id.as_str());
                let mut item = serde_json::to_value(&dto).unwrap_or_else(|_| json!({}));
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("session_title".to_string(), json!(title));
                    obj.insert("is_current_session".to_string(), json!(is_current));
                }
                item
            })
            .collect()
    };
    let count = items.len();
    GatewayActionResult {
        success: true,
        data: Some(json!({
            "current_session_id": current_session_id,
            "schedules": items,
            "count": count,
        })),
        error: None,
    }
}

/// 自分の定時実行スケジュールを **id 指定で**部分更新する（どのセッションの行でもよい）。
///
/// `id` は `get_my_schedules` が返したもの。所属チェック（`ctx.agent_id`）を通った行だけを更新でき、
/// 行のセッション（発火先）は変わらない。`enabled=false` で「止める」（行は残り履歴が追える）、cron/message/
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
    if let Some(denied) = reject_scope_arg(args) {
        return denied;
    }
    // 行のセッションは付け替えない（session_id は id の行が持つ）。
    if let Some(denied) = reject_session_id_with_id(args) {
        return denied;
    }

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
                session_id = %dto.session_id,
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

/// 自分の定時実行スケジュールを **id 指定で**削除する（どのセッションの行でもよい）。
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
    if let Some(denied) = reject_scope_arg(args) {
        return denied;
    }
    if let Some(denied) = reject_session_id_with_id(args) {
        return denied;
    }

    let id = match required_i64(args, "id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    match delete_schedule_core(state, &ctx.agent_id, id) {
        Ok(()) => {
            tracing::info!(
                agent_id = %ctx.agent_id,
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
/// 対象は所属チェック（[`load_owned_schedule`]・`ctx.agent_id`）を通った行だけで、発火先はその行の
/// セッション（[`session_target`] で解決）。
///
/// # `last_fired_at` は更新しない
///
/// 手動発火は定時発火の位相をずらさないため `last_fired_at` を刻まない（刻むのはスケジューラの
/// 発火ループだけ・I2）。
///
/// # 自己デッドロックを避ける（#599）
///
/// このツールは呼び出しターンの中で走り、行のセッションが現在のセッションなら、そのターンは既に
/// 直列化ロックを保持している。発火は `spawn` して**即座に「投げた」を返し**、実際のターンは今のターンが終わってから走る。
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
    if let Some(denied) = reject_scope_arg(args) {
        return denied;
    }
    if let Some(denied) = reject_session_id_with_id(args) {
        return denied;
    }

    let id = match required_i64(args, "id") {
        Ok(v) => v,
        Err(e) => return e,
    };

    let row = match load_owned_schedule(state, &ctx.agent_id, id) {
        Ok(row) => row,
        Err(ScheduleOpError::BadRequest(m)) | Err(ScheduleOpError::Internal(m)) => return err(m),
    };
    let session_id = row.session_id.clone();
    let target = match session_target(state, &ctx.agent_id, &session_id) {
        Ok(target) => target,
        Err(e) => return e,
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
            "note": "スケジュールを発火しました。実際のターンはそのスケジュールのセッション（session_id）で走ります。いまのセッションなら今のターンが終わってから走ります（last_fired_at は更新しません）。",
        })),
        error: None,
    }
}

#[cfg(test)]
#[path = "agent_schedule/review_tests.rs"]
mod review_tests;

#[cfg(test)]
#[path = "agent_schedule/cross_session_tests.rs"]
mod cross_session_tests;

#[cfg(test)]
mod tests;
