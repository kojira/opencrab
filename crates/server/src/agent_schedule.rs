//! エージェントが**自分自身の**定時実行（#455）を登録・照会するツール。
//!
//! - `set_my_schedule`: いま話しているセッションに対して cron / `@every` のスケジュールを登録する。
//! - `get_my_schedules`: いま話しているセッションのスケジュールを、次回発火時刻付きで列挙する。
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

use crate::api::schedules::{create_schedule_core, list_session_schedules_core, ScheduleOpError};
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(session_id: &str) -> GatewayCallContext {
        let mut c = GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x");
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
}
