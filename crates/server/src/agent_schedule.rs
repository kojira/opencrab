//! エージェントが**自分自身の**定時実行（#455）を登録・照会するツール。
//!
//! - `set_my_schedule`: いま話しているセッションに対して cron / `@every` のスケジュールを登録する。
//! - `get_my_schedules`: いま話しているセッションのスケジュールを、次回発火時刻付きで列挙する。
//! - `update_my_schedule`: `get_my_schedules` が返した id のスケジュールを部分更新する
//!   （`enabled=false` で「止める」・cron/message/timezone の変更で「間隔を変える」）。
//! - `delete_my_schedule`: `get_my_schedules` が返した id のスケジュールを消す（履歴も残さない）。
//!
//! # id の所属チェック（#477）
//!
//! `update_my_schedule` / `delete_my_schedule` は id を取る。**id を推測して他エージェント・他
//! セッションのスケジュールを触れてはいけない**ので、対象行は `ctx.agent_id`＋現在のセッションの
//! 両方に一致する場合だけ操作できる（`api::schedules` 側の `load_owned_schedule` が所属チェック
//! を握る）。一致しない／存在しない id は**存在を明かさず**同じ文言で拒否する。
//!
//! # なぜエージェント自身に開くか（設計 §7.4 の制約撤回・オーナー裁定 2026-08-09）
//!
//! 当初の設計は「新しい自己設定ツールは追加しない」としていたが、これは issue #455 に無い制約で、
//! **omoikane の巡回指示ループを閉じられない**（巡回指示が webhook で届いても本人がスケジュールを
//! 作れず、毎回オーナーが dashboard から登録することになる）。ハートビート（「いつ動くか」）は既に
//! 本人が `set_my_heartbeat` で設定できる（#456）ので、schedule だけ人の承認を要求する理由が実測に
//! 無い。**増えるのは「何ができるか」ではなく「いつ動くかを自分で決められるか」だけ**（作用面は
//! HB と同一・オーナー裁定 A 案）。
//!
//! # セッション単位（`set_my_heartbeat` と同じ流儀・#456）
//!
//! **スコープは無い。** 対象は常に `ctx.session_id`（いま話しているセッション）。発火経路を持つのは
//! `nostr-` / `discord-` セッションだけなので、それ以外（`web-` 等）で呼ばれたら **fail-closed で
//! 拒否し remedy（どこで実行すればよいか）を返す**。「設定できたのに永遠に発火しない行」を作らせない。
//!
//! # 権限
//!
//! `set_my_heartbeat` と同じく **owner 限定にはしない**（自分の定時実行を自分で決めるのが目的）が、
//! 素の `Agent`（未信頼の外部ユーザー由来ターン）からは見えないよう `TRUSTED_ONLY_ACTIONS` に入れ、
//! ハンドラ内でも同じ検査をする（多層防御）。

use serde_json::json;

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::api::schedules::{
    create_schedule_core, delete_schedule_core, list_session_schedules_core, update_schedule_core,
    ScheduleOpError, SchedulePatch,
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

/// 現在のセッションが発火経路を持つかを確認する（`agent_heartbeat` と**同じ種別集合**）。
///
/// セッション文脈が無い / 発火経路の無い種別（`web-` 等）→ fail-closed で **remedy 付き**エラー。
fn current_session(ctx: &GatewayCallContext) -> Result<String, GatewayActionResult> {
    let session_id = match ctx.session_id.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            return Err(err(
                "このセッションからは定時実行を設定・照会できません（セッション文脈がありません）。設定したい対象のセッション——Discord のチャンネル、または Nostr の自発投稿——で実行してください。",
            ));
        }
    };
    match opencrab_db::queries::resolve_session_fire_target(session_id, &ctx.agent_id) {
        Some(_) => Ok(session_id.to_string()),
        None => Err(err(
            "このセッションからは定時実行を設定・照会できません（このセッション種別には発火経路がありません）。設定したい対象のセッション——Discord のチャンネル、または Nostr の自発投稿——で実行してください。",
        )),
    }
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

    let session_id = match current_session(ctx) {
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
    // enabled は省略時 true（omoikane: 「登録したらそのまま回る」）。型違いは拒否。
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

    let session_id = match current_session(ctx) {
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

    let session_id = match current_session(ctx) {
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

    let session_id = match current_session(ctx) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(session_id: &str) -> GatewayCallContext {
        let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
        c.session_id = Some(session_id.to_string());
        c
    }

    /// 別エージェント（agent-y）の文脈。他人の id を渡す攻撃の再現に使う。
    fn ctx_for(agent_id: &str, session_id: &str) -> GatewayCallContext {
        let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, agent_id);
        c.session_id = Some(session_id.to_string());
        c
    }

    /// set_my_schedule は **ctx.session_id** に対して作成する（スコープ引数なし）。
    #[tokio::test]
    async fn set_creates_on_current_session_and_get_lists_it() {
        let state = crate::test_app_state();
        let c = ctx("nostr-agent-x");
        let res = set_my_schedule(
            &state,
            &json!({"cron_expr": "@every 3h", "message": "巡回してまとめを書く"}),
            &c,
        );
        assert!(res.success, "作成成功: {:?}", res.error);
        let data = res.data.unwrap();
        assert_eq!(data["session_id"], "nostr-agent-x");
        assert!(data["id"].as_i64().unwrap() > 0);
        assert_eq!(data["enabled"], true, "enabled 省略時 true");
        // next_fire_at が照会時算出される（@every 3h・anchor=now → 未来）。
        assert!(data["next_fire_at"].is_string(), "next_fire_at を返す");

        // get_my_schedules は同一セッションのものを列挙し next_fire_at を含む。
        let got = get_my_schedules(&state, &json!({}), &c);
        assert!(got.success);
        let gd = got.data.unwrap();
        assert_eq!(gd["count"], 1);
        assert!(gd["schedules"][0]["next_fire_at"].is_string());
    }

    /// 発火経路の無いセッション（`web-`）は fail-closed + **remedy** で拒否する。
    #[tokio::test]
    async fn set_rejects_non_firing_session_with_remedy() {
        let state = crate::test_app_state();
        let res = set_my_schedule(
            &state,
            &json!({"cron_expr": "@every 3h", "message": "x"}),
            &ctx("web-agent-x"),
        );
        assert!(!res.success);
        let e = res.error.unwrap();
        assert!(e.contains("発火経路"), "理由: {e}");
        assert!(e.contains("実行してください"), "remedy: {e}");
    }

    /// cron 式が不正ならその場でエラー（remedy 付き）。
    #[tokio::test]
    async fn set_rejects_invalid_cron_in_the_same_turn() {
        let state = crate::test_app_state();
        let res = set_my_schedule(
            &state,
            &json!({"cron_expr": "totally not cron", "message": "x"}),
            &ctx("nostr-agent-x"),
        );
        assert!(!res.success);
        let e = res.error.unwrap();
        assert!(
            e.contains("cron") || e.contains("@every") || e.contains("不正"),
            "cron 不正 remedy: {e}"
        );
    }

    /// スコープ引数（session_id 等）は明示拒否（#456 の語彙統一）。
    #[tokio::test]
    async fn set_rejects_scope_style_args() {
        let state = crate::test_app_state();
        let res = set_my_schedule(
            &state,
            &json!({"session_id": "nostr-other", "cron_expr": "@every 3h", "message": "x"}),
            &ctx("nostr-agent-x"),
        );
        assert!(!res.success);
        assert!(res.error.unwrap().contains("session_id"));
    }

    /// 必須引数（cron_expr / message）欠落は remedy 付きエラー。
    #[tokio::test]
    async fn set_requires_cron_and_message() {
        let state = crate::test_app_state();
        let res = set_my_schedule(&state, &json!({"message": "x"}), &ctx("nostr-agent-x"));
        assert!(!res.success);
        assert!(res.error.unwrap().contains("cron_expr"));
    }

    // ---- #477: update / delete ----

    /// 自分のスケジュールを作って id を取り出すヘルパ。
    fn create_one(state: &AppState, c: &GatewayCallContext) -> i64 {
        let res = set_my_schedule(
            state,
            &json!({"cron_expr": "@every 3h", "message": "巡回してまとめを書く"}),
            c,
        );
        assert!(res.success, "作成成功: {:?}", res.error);
        res.data.unwrap()["id"].as_i64().unwrap()
    }

    /// update に enabled=false を渡すと「止まる」が**行は残る**（履歴が追える）。
    #[tokio::test]
    async fn update_disable_stops_but_keeps_row() {
        let state = crate::test_app_state();
        let c = ctx("nostr-agent-x");
        let id = create_one(&state, &c);

        let res = update_my_schedule(&state, &json!({"id": id, "enabled": false}), &c);
        assert!(res.success, "更新成功: {:?}", res.error);
        assert_eq!(res.data.unwrap()["enabled"], false, "enabled=false で停止");

        // 行は残る（delete と違い列挙に出続ける）。
        let got = get_my_schedules(&state, &json!({}), &c);
        let gd = got.data.unwrap();
        assert_eq!(gd["count"], 1, "止めても行は残る（履歴が追える）");
        assert_eq!(gd["schedules"][0]["enabled"], false);
    }

    /// update で cron を変えると「間隔を変える」が実現し、id は変わらない。
    #[tokio::test]
    async fn update_changes_interval_same_id() {
        let state = crate::test_app_state();
        let c = ctx("nostr-agent-x");
        let id = create_one(&state, &c);

        let res = update_my_schedule(&state, &json!({"id": id, "cron_expr": "0 7 * * *"}), &c);
        assert!(res.success, "更新成功: {:?}", res.error);
        let data = res.data.unwrap();
        assert_eq!(
            data["id"].as_i64().unwrap(),
            id,
            "同じ id を更新（付け替えない）"
        );
        assert_eq!(data["cron_expr"], "0 7 * * *");
    }

    /// 変更フィールドが 1 つも無い update は暗黙の no-op を避けて拒否する。
    #[tokio::test]
    async fn update_rejects_no_fields() {
        let state = crate::test_app_state();
        let c = ctx("nostr-agent-x");
        let id = create_one(&state, &c);
        let res = update_my_schedule(&state, &json!({"id": id}), &c);
        assert!(!res.success);
        assert!(res.error.unwrap().contains("変更する項目"));
    }

    /// update の cron 不正は同ターンでエラー（直して呼び直せる）。
    #[tokio::test]
    async fn update_rejects_invalid_cron_in_the_same_turn() {
        let state = crate::test_app_state();
        let c = ctx("nostr-agent-x");
        let id = create_one(&state, &c);
        let res = update_my_schedule(
            &state,
            &json!({"id": id, "cron_expr": "totally not cron"}),
            &c,
        );
        assert!(!res.success);
        assert!(res.error.unwrap().contains("不正"), "cron 不正 remedy");
    }

    /// delete は行ごと消す（以後 list に出ない）。
    #[tokio::test]
    async fn delete_removes_row() {
        let state = crate::test_app_state();
        let c = ctx("nostr-agent-x");
        let id = create_one(&state, &c);

        let res = delete_my_schedule(&state, &json!({"id": id}), &c);
        assert!(res.success, "削除成功: {:?}", res.error);
        assert_eq!(res.data.unwrap()["id"].as_i64().unwrap(), id);

        let got = get_my_schedules(&state, &json!({}), &c);
        assert_eq!(got.data.unwrap()["count"], 0, "削除後は列挙に出ない");
    }

    /// 存在しない id の delete は remedy 付きエラー（成功しない）。
    #[tokio::test]
    async fn delete_missing_id_fails() {
        let state = crate::test_app_state();
        let res = delete_my_schedule(&state, &json!({"id": 999999}), &ctx("nostr-agent-x"));
        assert!(!res.success);
        assert!(res.error.unwrap().contains("見つかりません"));
    }

    /// **所属チェック（#477 決定事項 1）**: 他エージェント（agent-y）が agent-x の id を推測して
    /// 渡しても、update / delete は失敗し、agent-x の行は無傷で残る。
    ///
    /// このテストは所属チェックの変異検出用: `load_owned_schedule` の agent_id 一致条件を外すと
    /// delete が通り、`victim_survives` が赤くなる。
    #[tokio::test]
    async fn foreign_agent_cannot_touch_others_schedule() {
        let state = crate::test_app_state();
        let victim = ctx("nostr-agent-x"); // agent-x
        let id = create_one(&state, &victim);

        // agent-y が自分のセッション（発火経路あり）から victim の id を渡す。
        let attacker = ctx_for("agent-y", "nostr-agent-y");

        let del = delete_my_schedule(&state, &json!({"id": id}), &attacker);
        assert!(!del.success, "他エージェントの id は削除できない");
        assert!(
            del.error.unwrap().contains("見つかりません"),
            "存在を明かさない文言"
        );

        let upd = update_my_schedule(&state, &json!({"id": id, "enabled": false}), &attacker);
        assert!(!upd.success, "他エージェントの id は更新できない");

        // victim の行は無傷（削除も更新もされていない）。
        let got = get_my_schedules(&state, &json!({}), &victim);
        let gd = got.data.unwrap();
        assert_eq!(gd["count"], 1, "victim の行は残っている");
        assert_eq!(
            gd["schedules"][0]["enabled"], true,
            "victim の行は更新されていない"
        );
    }

    /// **セッション所属チェック**: 同じ agent でも別セッション（この agent の Discord チャンネル）
    /// からは、Nostr セッションの id を触れない。`load_owned_schedule` の session_id 一致条件を
    /// 外すとこのテストが赤くなる。
    #[tokio::test]
    async fn other_session_of_same_agent_cannot_touch() {
        let state = crate::test_app_state();
        let nostr = ctx("nostr-agent-x");
        let id = create_one(&state, &nostr);

        // 同じ agent-x だが別セッション（Discord）。発火経路はあるが所属が違う。
        let discord = ctx("discord-agent-x-111-222");

        let del = delete_my_schedule(&state, &json!({"id": id}), &discord);
        assert!(!del.success, "別セッションからは削除できない");

        // Nostr 側の行は残る。
        let got = get_my_schedules(&state, &json!({}), &nostr);
        assert_eq!(got.data.unwrap()["count"], 1);
    }

    /// id を文字列で渡しても受け付ける（LLM が数値を文字列化する実測に対応）。
    #[tokio::test]
    async fn update_accepts_stringified_id() {
        let state = crate::test_app_state();
        let c = ctx("nostr-agent-x");
        let id = create_one(&state, &c);
        let res = update_my_schedule(&state, &json!({"id": id.to_string(), "enabled": false}), &c);
        assert!(res.success, "文字列 id を受ける: {:?}", res.error);
    }
}
