//! 通知先（webhook）の管理ツールの gateway 非依存実装（#157 S5）。
//!
//! 汎用名（`set_default_webhook` / `get_default_webhook` / `list_webhooks`）は既定で
//! `family='activity'`（一般ツール/コマンド活動）を扱う。後方互換の `*_subtask_webhook`
//! 名は既定で `family='subtask'`（subtask ライフサイクル）を扱う。いずれも `family`
//! 引数で明示上書きできる。
//!
//! - `get_default_*`: owner/trusted_user/co_agent。実際に使われるデフォルトを解決して
//!   redacted で返す（family で解決順序が変わる: activity は activity 行のみ、subtask は
//!   subtask>lifecycle>activity + 設定ファイル由来のフォールバック）。
//! - `set_default_*`: scope ごとのデフォルトを upsert する（url 空/省略は enabled=false の
//!   auditable disable）。owner は全 scope を、agent 自身は自分の agent-scope のみ管理できる。
//! - `list_*`: owner/trusted_user/co_agent。設定一覧を redacted で返す（`family` で絞り込み可）。
//!
//! いずれも raw url/token は決して返さない・記録しない（`redact_webhook_url` のみ）。
//!
//! # 移設の経緯と、Discord に残したもの
//!
//! 旧実装は Discord ゲートウェイ（`crates/discord` の `gateway_actions/subtask_webhook.rs`）
//! にあり、**DB と設定ファイル由来の既定値しか触らない**のに Discord 経由のターンでしか
//! 露出しなかった（#157 / #155）。この 6 本をそのまま `SystemGatewayActions` の own ツール
//! へ移し、web / Nostr / REST / heartbeat の全ターンで使えるようにする。
//!
//! 一方 `ensure_webhook` / `ensure_subtask_webhook` は **Discord に残す**。既存デフォルトが
//! 無いときに `discord_create_webhook`（serenity 依存の transport 固有処理）を呼んで
//! webhook を新規作成するためで、「解決部分は下位層・作成部分は Discord」に割る設計は
//! 実装が 1 つしか無い抽象を生むので S5 では行わない。
//!
//! # 移設で維持している不変条件（順に対応するテストがある）
//!
//! - **レスポンス JSON のキーと全エラー文言**を旧実装と 1 バイトも変えない。
//! - **秘匿処理**: 生の URL / トークンは応答にも監査ログにも出さない。
//! - **URL のホスト許可リスト**（`validate_webhook_url`）をそのまま使う（緩めない）。
//! - **ハンドラ内の権限検査**（read = owner/co_agent/trusted_user、set = owner または
//!   自分の agent-scope のみの agent）。bridge 側にこのゲートは無いので**単層**である。
//! - 権限拒否は構造マーカー付き（`REJECTION_CODE_PREFIX`）で返す。合成層の汎用エラー
//!   ヘルパはマーカーを付けないため、素朴に置き換えると拒否がツール失敗として記録され
//!   観測性が落ちる（#199）。ここでは旧実装と同じ形を保つ。
//! - **分類の所属**（6 本とも inline）を変えない。

use serde_json::json;

use opencrab_actions::webhook_target::{
    redact_webhook_url, resolve_activity_webhook, resolve_subtask_webhook, validate_webhook_url,
    WebhookResolution, WebhookSource,
};
use opencrab_actions::REJECTION_CODE_PREFIX;
use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};

use crate::AppState;

fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

/// 権限ポリシーによる拒否（実行に到達しない）を構造的に表す。
///
/// 分類器（`opencrab_actions::is_rejection`）が安定して rejected と判定できるよう、
/// 文言の先頭へ構造マーカー（`REJECTION_CODE_PREFIX`）を付ける。説明文はそのまま
/// 残すので、`forbidden_scope` / `requires owner` 等の既存トークンも保持される。
/// raw な機微情報（url/token）は決して載せない。実行に到達しないこの拒否を
/// ログでも観測可能にする。
fn reject(msg: impl Into<String>) -> GatewayActionResult {
    let msg = msg.into();
    tracing::debug!(
        target: "webhook_audit",
        reason = %msg,
        "gateway action rejected by permission policy"
    );
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(format!("{REJECTION_CODE_PREFIX}{msg}")),
    }
}

/// events_json (Option<String>) を JSON 配列値へ変換する（無ければ Null）。
fn events_value(events_json: &Option<String>) -> serde_json::Value {
    match events_json
        .as_ref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    {
        Some(v) if v.is_array() => v,
        _ => serde_json::Value::Null,
    }
}

/// scope に対する DB 行のデフォルト tool_name を決める。
fn default_tool_name(scope: &str, provided: Option<&str>) -> String {
    match provided {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            if scope == "tool" {
                "spawn_subtask".to_string()
            } else {
                String::new()
            }
        }
    }
}

/// WebhookSource をスコープ文字列に写像する（表示用）。
fn scope_for_source(source: WebhookSource) -> serde_json::Value {
    match source {
        WebhookSource::ToolDefault => json!("tool"),
        WebhookSource::AgentDefault => json!("agent"),
        WebhookSource::GlobalDefault => json!("global"),
        WebhookSource::Explicit => json!("explicit"),
        WebhookSource::EnvConfig => json!("env_config"),
    }
}

/// 後方互換: subtask family の get。
pub(crate) fn get_default_subtask_webhook(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    get_default_webhook_impl(state, args, ctx, "subtask")
}

/// 汎用: 既定 activity family の get。
pub(crate) fn get_default_webhook(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    get_default_webhook_impl(state, args, ctx, "activity")
}

fn get_default_webhook_impl(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
    default_family: &str,
) -> GatewayActionResult {
    if !matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
        return reject("redacted read requires owner/trusted_user/co_agent");
    }
    if args
        .get("include_secret")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return err("include_secret is not supported");
    }

    let family = args
        .get("family")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(default_family);
    let agent_id = args
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&ctx.agent_id)
        .to_string();
    let tool_name = args.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");

    // "webhook" キーを持たない args で解決し、実際に使われるデフォルトを得る。
    // activity family は activity 行のみを固定順序（tool>agent>global）で解決する。
    // subtask family は subtask>lifecycle>activity + 設定ファイル由来のフォールバックを含む。
    let resolve_args = json!({});
    let resolution = {
        let conn = state.db.lock().unwrap();
        if family == "activity" {
            resolve_activity_webhook(&conn, &agent_id, tool_name)
        } else {
            resolve_subtask_webhook(
                &conn,
                &agent_id,
                tool_name,
                &resolve_args,
                state.default_subtask_webhook.as_ref(),
            )
        }
    };

    let data = match resolution {
        WebhookResolution::Use { config, source } => json!({
            "scope": scope_for_source(source),
            "source": source.as_str(),
            "enabled": true,
            "events": config.events,
            "redacted_url": redact_webhook_url(&config.url),
            "status": "ok",
        }),
        WebhookResolution::Disabled { source } => json!({
            "scope": scope_for_source(source),
            "source": source.as_str(),
            "enabled": false,
            "events": serde_json::Value::Null,
            "redacted_url": serde_json::Value::Null,
            "status": "disabled",
        }),
        WebhookResolution::None => json!({
            "scope": serde_json::Value::Null,
            "source": serde_json::Value::Null,
            "enabled": false,
            "events": serde_json::Value::Null,
            "redacted_url": serde_json::Value::Null,
            "status": "none",
        }),
        WebhookResolution::Error {
            code,
            message,
            source,
        } => json!({
            "scope": scope_for_source(source),
            "source": source.as_str(),
            "enabled": true,
            "events": serde_json::Value::Null,
            "redacted_url": serde_json::Value::Null,
            "status": "error",
            "error": format!("{code}: {message}"),
        }),
    };
    let mut data = data;
    if let Some(obj) = data.as_object_mut() {
        obj.insert("family".to_string(), json!(family));
    }

    GatewayActionResult {
        success: true,
        data: Some(data),
        error: None,
    }
}

/// 後方互換: 既定 subtask family の set。
pub(crate) fn set_default_subtask_webhook(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    set_default_webhook_impl(state, args, ctx, "subtask")
}

/// 汎用: 既定 activity family の set。
pub(crate) fn set_default_webhook(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    set_default_webhook_impl(state, args, ctx, "activity")
}

fn set_default_webhook_impl(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
    default_family: &str,
) -> GatewayActionResult {
    let scope = match args.get("scope").and_then(|v| v.as_str()) {
        Some(s) if matches!(s, "agent" | "tool" | "global") => s.to_string(),
        _ => return err("scope is required: 'agent' | 'tool' | 'global'"),
    };
    // `family` を優先し、後方互換で `kind` も受ける。既定はこのアクションの family
    // （汎用名は activity、`*_subtask_webhook` 名は subtask）。明示上書きも可。
    let family_was_explicit = args
        .get("family")
        .and_then(|v| v.as_str())
        .or_else(|| args.get("kind").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .is_some();
    let kind = args
        .get("family")
        .and_then(|v| v.as_str())
        .or_else(|| args.get("kind").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .unwrap_or(default_family)
        .to_string();

    let agent_id = if scope == "global" {
        "*".to_string()
    } else {
        args.get("agent_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&ctx.agent_id)
            .to_string()
    };

    // 権限: owner（等価）は全 scope を set/disable できる。Agent は自分自身の agent scope
    // （scope='agent' かつ agent_id が自分）のみ。それ以外（tool/global/他 agent）は拒否。
    // trusted_user は read-only（set/disable 不可）。
    // #485: co_agent は owner 等価になったので owner と同じく全 scope を set/disable できる
    //       （唯一の源は is_owner_equivalent）。
    match &ctx.caller {
        c if c.is_owner_equivalent() => {}
        GatewayCaller::Agent => {
            if scope != "agent" {
                return reject(
                    "forbidden_scope: an agent may only set/disable its own agent-scope default webhook",
                );
            }
            if agent_id != ctx.agent_id {
                return reject(
                    "forbidden_scope: an agent may only set/disable its own agent default webhook",
                );
            }
        }
        _ => {
            return reject(
                "set/disable requires owner (agents may manage only their own agent scope)",
            );
        }
    }
    let tool_name = default_tool_name(&scope, args.get("tool_name").and_then(|v| v.as_str()));

    let raw_url = args
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");

    // url 空/省略 → enabled=false の auditable disable（空文字を保存）。
    let (url, enabled) = if raw_url.is_empty() {
        (String::new(), false)
    } else {
        if let Err(reason) = validate_webhook_url(raw_url) {
            return err(format!("invalid_webhook_url: {reason}"));
        }
        let enabled = args
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        (raw_url.to_string(), enabled)
    };

    let events = args.get("events").and_then(|v| v.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|e| e.as_str().map(|s| s.to_string()))
            .collect::<Vec<_>>()
    });
    let events_json = events
        .as_ref()
        .map(|e| serde_json::to_string(e).unwrap_or_default());
    let output_mode = args
        .get("output_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("summary")
        .to_string();
    let max_chars = args
        .get("max_chars")
        .and_then(|v| v.as_i64())
        .unwrap_or(1500);

    let row = opencrab_db::queries::AgentWebhookConfigRow {
        scope: scope.clone(),
        agent_id: agent_id.clone(),
        tool_name: tool_name.clone(),
        kind: kind.clone(),
        url: url.clone(),
        events_json,
        enabled,
        name: args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        created_by: Some(ctx.caller.label().to_string()),
        output_mode,
        max_chars,
        updated_at: String::new(),
    };

    let result = {
        let conn = state.db.lock().unwrap();
        let mut result = opencrab_db::queries::upsert_agent_webhook_config(&conn, &row);
        if result.is_ok() && default_family == "subtask" && !family_was_explicit {
            let mut activity_row = row.clone();
            activity_row.kind = "activity".to_string();
            if let Err(e) = opencrab_db::queries::upsert_agent_webhook_config(&conn, &activity_row)
            {
                tracing::warn!(
                    target: "webhook_audit",
                    caller = %ctx.caller.label(),
                    scope = %scope,
                    agent_id = %agent_id,
                    tool_name = %tool_name,
                    result = "activity_mirror_failed",
                    error = %e,
                    "set_default_subtask_webhook failed to mirror activity default"
                );
                result = Err(e);
            }
        }
        result
    };

    let redacted = redact_webhook_url(&url);
    match result {
        Ok(()) => {
            tracing::info!(
                target: "webhook_audit",
                caller = %ctx.caller.label(),
                scope = %scope,
                agent_id = %agent_id,
                tool_name = %tool_name,
                redacted_url = %redacted,
                result = "ok",
                "subtask webhook action: set_default"
            );
            GatewayActionResult {
                success: true,
                data: Some(json!({
                    "scope": scope,
                    "agent_id": agent_id,
                    "tool_name": tool_name,
                    "family": kind,
                    "enabled": enabled,
                    "redacted_url": if enabled { json!(redacted) } else { serde_json::Value::Null },
                })),
                error: None,
            }
        }
        Err(e) => {
            tracing::info!(
                target: "webhook_audit",
                caller = %ctx.caller.label(),
                scope = %scope,
                agent_id = %agent_id,
                tool_name = %tool_name,
                redacted_url = %redacted,
                result = "error",
                "subtask webhook action: set_default"
            );
            err(format!("failed to save webhook config: {e}"))
        }
    }
}

/// 後方互換: subtask 系の一覧（family 絞り込みは引数で任意指定可）。
pub(crate) fn list_subtask_webhooks(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    list_webhooks(state, args, ctx)
}

/// 汎用: webhook 設定の一覧。`family`/`kind` 引数で kind を絞り込める（既定は全件）。
pub(crate) fn list_webhooks(
    state: &AppState,
    args: &serde_json::Value,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    if !matches!(
        ctx.caller,
        GatewayCaller::Owner | GatewayCaller::CoAgent { .. } | GatewayCaller::TrustedUser
    ) {
        return reject("redacted read requires owner/trusted_user/co_agent");
    }

    let agent_id = args
        .get("agent_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&ctx.agent_id)
        .to_string();
    let scope_filter = args
        .get("scope")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    // family/kind による任意の絞り込み（省略時は全 kind を返す）。
    let kind_filter = args
        .get("family")
        .and_then(|v| v.as_str())
        .or_else(|| args.get("kind").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let include_disabled = args
        .get("include_disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let rows = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_agent_webhook_config(&conn, Some(&agent_id), include_disabled)
    };
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return err(format!("failed to list webhook config: {e}")),
    };

    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .filter(|r| scope_filter.as_ref().map(|s| &r.scope == s).unwrap_or(true))
        .filter(|r| kind_filter.as_ref().map(|k| &r.kind == k).unwrap_or(true))
        .map(|r| {
            json!({
                "scope": r.scope,
                "agent_id": r.agent_id,
                "tool_name": r.tool_name,
                "kind": r.kind,
                "enabled": r.enabled,
                "events": events_value(&r.events_json),
                "redacted_url": redact_webhook_url(&r.url),
                "name": r.name,
                "output_mode": r.output_mode,
                "max_chars": r.max_chars,
                "updated_at": r.updated_at,
            })
        })
        .collect();

    GatewayActionResult {
        success: true,
        data: Some(json!({ "webhooks": items })),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_actions::SystemGatewayActions;
    use opencrab_actions::webhook_target::WebhookConfig;
    use opencrab_gateway::GatewayActions;

    const WH_VALID_URL: &str = "https://discord.com/api/webhooks/123456789/abcSECRETtok";
    const WH_SECRET: &str = "abcSECRETtok";

    /// 応答 JSON に raw トークンが 1 度も現れないこと（秘匿処理の不変条件）。
    fn json_has_no_raw_token(v: &serde_json::Value) -> bool {
        !v.to_string().contains(WH_SECRET)
    }

    /// **transport 固有 gateway 無し**（`inner = None`）で合成 gateway を組む。
    ///
    /// これは web / REST / Nostr / heartbeat の経路、および Discord feature 無効ビルド
    /// そのもの。移設前はこの構成で 6 ツールが一切出なかった（#157 の不具合）。
    fn make_test_actions() -> (SystemGatewayActions, opencrab_db::Db) {
        make_test_actions_with_fallback(None)
    }

    /// 設定ファイル由来のフォールバックを注入した版。
    fn make_test_actions_with_fallback(
        default_subtask_webhook: Option<WebhookConfig>,
    ) -> (SystemGatewayActions, opencrab_db::Db) {
        let mut state = crate::test_app_state();
        state.default_subtask_webhook = default_subtask_webhook;
        let db = state.db.clone();
        (SystemGatewayActions::new(state, None, None, None), db)
    }

    /// テスト用の呼び出しコンテキスト（移設元の Discord テストと同じ agent_id）。
    fn tctx(caller: GatewayCaller) -> GatewayCallContext {
        GatewayCallContext::new(caller, "test-agent")
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_requires_owner() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires owner"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_self_manage_allowed() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "family": "activity",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "agent self-manage should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert_eq!(data["enabled"], true);

        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap()
        .unwrap();
        assert!(row.enabled);
        assert_eq!(row.url, WH_VALID_URL);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_can_disable_own() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": "",
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            result.success,
            "agent disable should succeed: {:?}",
            result.error
        );
        assert_eq!(result.data.unwrap()["enabled"], false);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_tool_scope() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "tool",
                    "tool_name": "execute_shell",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_global() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "global",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_agent_cannot_set_other_agent() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "agent_id": "someone-else",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("forbidden_scope"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_trusted_user_cannot_set() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires owner"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_owner_success_redacted() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": WH_VALID_URL,
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(
            result.success,
            "owner set should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert!(json_has_no_raw_token(&data), "raw token leaked in response");
        assert!(data["redacted_url"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));

        // stored in DB
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .unwrap();
        assert!(row.enabled);
        assert_eq!(row.url, WH_VALID_URL);
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_invalid_url() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({

                    "scope": "agent",
                    "url": "http://evil.com/x",
                }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("invalid_webhook_url"));
    }

    #[tokio::test]
    async fn test_set_default_subtask_webhook_empty_url_disables() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({  "scope": "agent" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let conn = db.lock().unwrap();
        let row = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .unwrap();
        assert!(!row.enabled);
    }

    #[tokio::test]
    async fn test_get_default_subtask_webhook_permission_and_redaction() {
        let (actions, db) = make_test_actions();
        // seed an agent default
        {
            let conn = db.lock().unwrap();
            let row = opencrab_db::queries::AgentWebhookConfigRow {
                scope: "agent".to_string(),
                agent_id: "test-agent".to_string(),
                tool_name: String::new(),
                kind: "subtask".to_string(),
                url: WH_VALID_URL.to_string(),
                events_json: None,
                enabled: true,
                name: None,
                created_by: Some("owner".to_string()),
                output_mode: "summary".to_string(),
                max_chars: 1500,
                updated_at: String::new(),
            };
            opencrab_db::queries::upsert_agent_webhook_config(&conn, &row).unwrap();
        }

        // bare agent denied
        let denied = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);

        // trusted_user allowed, redacted only
        let allowed = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await;
        assert!(allowed.success);
        let data = allowed.data.unwrap();
        assert!(json_has_no_raw_token(&data));
        assert_eq!(data["status"], "ok");
        assert_eq!(data["source"], "agent_default");
    }

    #[tokio::test]
    async fn test_get_default_subtask_webhook_include_secret_rejected() {
        let (actions, _db) = make_test_actions();
        let result = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({  "include_secret": true }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("include_secret"));
    }

    #[tokio::test]
    async fn test_list_subtask_webhooks_permission_and_redaction() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            let row = opencrab_db::queries::AgentWebhookConfigRow {
                scope: "agent".to_string(),
                agent_id: "test-agent".to_string(),
                tool_name: String::new(),
                kind: "subtask".to_string(),
                url: WH_VALID_URL.to_string(),
                events_json: None,
                enabled: true,
                name: None,
                created_by: Some("owner".to_string()),
                output_mode: "summary".to_string(),
                max_chars: 1500,
                updated_at: String::new(),
            };
            opencrab_db::queries::upsert_agent_webhook_config(&conn, &row).unwrap();
        }

        // bare agent denied
        let denied = actions
            .execute(
                "list_subtask_webhooks",
                &json!({}),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);

        let allowed = actions
            .execute(
                "list_subtask_webhooks",
                &json!({}),
                &tctx(GatewayCaller::CoAgent {
                    agent_id: "co-agent-1".to_string(),
                }),
            )
            .await;
        assert!(allowed.success);
        let data = allowed.data.unwrap();
        assert!(json_has_no_raw_token(&data), "raw token leaked in list");
        let hooks = data["webhooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0]["redacted_url"]
            .as_str()
            .unwrap()
            .contains("[redacted]"));
    }

    /// 汎用 set_default_webhook は既定で family='activity' の行を upsert する。
    #[tokio::test]
    async fn test_generic_set_default_webhook_defaults_to_activity_family() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(
            result.success,
            "owner set should succeed: {:?}",
            result.error
        );
        let data = result.data.unwrap();
        assert_eq!(data["family"], "activity");
        assert!(json_has_no_raw_token(&data), "raw token leaked in response");

        let conn = db.lock().unwrap();
        // activity 行が作られ、subtask 行は作られない。
        let activity = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap();
        assert!(activity.is_some(), "activity row should exist");
        assert_eq!(activity.unwrap().url, WH_VALID_URL);
        let subtask = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap();
        assert!(
            subtask.is_none(),
            "subtask row must not be created by generic name"
        );
    }

    /// 後方互換 set_default_subtask_webhook は既定で family='subtask' を返しつつ、
    /// agent の通常 tool/command activity へも効くよう activity 行も mirror する。
    #[tokio::test]
    async fn test_subtask_named_set_defaults_to_subtask_and_activity_families() {
        let (actions, db) = make_test_actions();
        let result = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.data.unwrap()["family"], "subtask");
        let conn = db.lock().unwrap();
        assert!(opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "subtask",
        )
        .unwrap()
        .is_some());
        let activity = opencrab_db::queries::get_agent_webhook_config(
            &conn,
            "agent",
            "test-agent",
            "",
            "activity",
        )
        .unwrap();
        assert!(
            activity.is_some(),
            "compat subtask default should also enable activity streaming"
        );
        let resolved = opencrab_actions::webhook_target::resolve_activity_webhook(
            &conn,
            "test-agent",
            "execute_shell",
        );
        assert!(
            matches!(
                resolved,
                opencrab_actions::webhook_target::WebhookResolution::Use { .. }
            ),
            "activity default should resolve after set_default_subtask_webhook"
        );
    }

    /// agent 自身は汎用名でも自分の agent-scope のみ設定でき、他 scope は拒否される。
    #[tokio::test]
    async fn test_generic_set_default_webhook_agent_scope_permission() {
        let (actions, _db) = make_test_actions();
        // 自分の agent-scope は許可。
        let ok = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "agent", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(
            ok.success,
            "agent self-manage should succeed: {:?}",
            ok.error
        );
        // global は拒否。
        let denied = actions
            .execute(
                "set_default_webhook",
                &json!({  "scope": "global", "url": WH_VALID_URL }),
                &tctx(GatewayCaller::Agent),
            )
            .await;
        assert!(!denied.success);
        assert!(denied.error.unwrap().contains("forbidden_scope"));
    }

    /// 汎用 get_default_webhook は activity 行のみを解決する（subtask 行は使わない）。
    #[tokio::test]
    async fn test_generic_get_default_webhook_resolves_activity_only() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            // subtask 行のみを seed。activity 行は無い。
            let row = opencrab_db::queries::AgentWebhookConfigRow {
                scope: "agent".to_string(),
                agent_id: "test-agent".to_string(),
                tool_name: String::new(),
                kind: "subtask".to_string(),
                url: WH_VALID_URL.to_string(),
                events_json: None,
                enabled: true,
                name: None,
                created_by: Some("owner".to_string()),
                output_mode: "summary".to_string(),
                max_chars: 1500,
                updated_at: String::new(),
            };
            opencrab_db::queries::upsert_agent_webhook_config(&conn, &row).unwrap();
        }
        // activity family の解決では subtask 行に fall through しない → none。
        let activity = actions
            .execute(
                "get_default_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(activity.success);
        let data = activity.data.unwrap();
        assert_eq!(data["status"], "none");
        assert_eq!(data["family"], "activity");
        // subtask family（family 明示）なら解決できる。
        let subtask = actions
            .execute(
                "get_default_webhook",
                &json!({  "family": "subtask" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert_eq!(subtask.data.unwrap()["status"], "ok");
    }

    /// 汎用 list_webhooks は family で kind を絞り込める。
    #[tokio::test]
    async fn test_generic_list_webhooks_family_filter() {
        let (actions, db) = make_test_actions();
        {
            let conn = db.lock().unwrap();
            for kind in ["subtask", "activity"] {
                let row = opencrab_db::queries::AgentWebhookConfigRow {
                    scope: "agent".to_string(),
                    agent_id: "test-agent".to_string(),
                    tool_name: String::new(),
                    kind: kind.to_string(),
                    url: WH_VALID_URL.to_string(),
                    events_json: None,
                    enabled: true,
                    name: None,
                    created_by: Some("owner".to_string()),
                    output_mode: "summary".to_string(),
                    max_chars: 1500,
                    updated_at: String::new(),
                };
                opencrab_db::queries::upsert_agent_webhook_config(&conn, &row).unwrap();
            }
        }
        // 絞り込み無し → 両方。
        let all = actions
            .execute("list_webhooks", &json!({}), &tctx(GatewayCaller::Owner))
            .await;
        assert_eq!(all.data.unwrap()["webhooks"].as_array().unwrap().len(), 2);
        // family=activity → 1 件。
        let filtered = actions
            .execute(
                "list_webhooks",
                &json!({  "family": "activity" }),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let hooks = filtered.data.unwrap();
        let arr = hooks["webhooks"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "activity");
        assert!(json_has_no_raw_token(&hooks));
    }

    // ---- #157 S5 で新規に固定する不変条件 ----

    /// **設定ファイル由来のフォールバックが Discord 以外の経路でも効く**（持ち上げの証明）。
    ///
    /// `inner = None`（transport 固有 gateway 無し = web / REST / Nostr / heartbeat、
    /// および Discord feature 無効ビルド）で、DB に行が 1 つも無い状態。持ち上げ前は
    /// この値へ到達できず `status: "none"` になっていた。
    #[tokio::test]
    async fn config_fallback_resolves_without_any_transport_gateway() {
        let (actions, _db) = make_test_actions_with_fallback(WebhookConfig::from_parts(
            WH_VALID_URL.to_string(),
            Some(vec!["started".to_string()]),
        ));
        let result = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let data = result.data.unwrap();
        assert_eq!(data["status"], "ok");
        assert_eq!(data["source"], "env_config");
        assert_eq!(data["scope"], "env_config");
        assert_eq!(data["family"], "subtask");
        assert_eq!(data["events"], json!(["started"]));
        assert!(
            json_has_no_raw_token(&data),
            "設定由来のフォールバックでも raw トークンを返さない"
        );
    }

    /// フォールバック未設定なら `status: "none"`（移設前と同じ「無い」応答）。
    #[tokio::test]
    async fn missing_config_fallback_still_reports_none() {
        let (actions, _db) = make_test_actions();
        let data = actions
            .execute(
                "get_default_subtask_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .data
            .unwrap();
        assert_eq!(data["status"], "none");
        assert_eq!(data["source"], serde_json::Value::Null);
    }

    /// activity family は設定ファイル由来のフォールバックを**使わない**（移設前と同じ）。
    ///
    /// `resolve_activity_webhook` は activity kind の DB 行しか見ない。持ち上げで
    /// うっかり activity にも効かせてしまうと、通知先を設定していないエージェントの
    /// ツール活動が全部その webhook へ流れる。
    #[tokio::test]
    async fn config_fallback_does_not_leak_into_the_activity_family() {
        let (actions, _db) = make_test_actions_with_fallback(WebhookConfig::from_parts(
            WH_VALID_URL.to_string(),
            None,
        ));
        let data = actions
            .execute(
                "get_default_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .data
            .unwrap();
        assert_eq!(data["status"], "none", "activity は env_config を使わない");
        assert_eq!(data["family"], "activity");
    }

    /// **エラー文言をバイト単位で固定する**（移設で 1 文字も変わっていないことの防波堤）。
    ///
    /// `contains` ではなく完全一致で比較する。拒否には構造マーカー
    /// （`REJECTION_CODE_PREFIX`）が付き、通常の失敗には付かないことも併せて固定する。
    #[tokio::test]
    async fn error_messages_are_byte_for_byte_unchanged() {
        let (actions, _db) = make_test_actions();

        // 読み取り権限（拒否 = マーカー付き）
        for name in [
            "get_default_subtask_webhook",
            "get_default_webhook",
            "list_subtask_webhooks",
            "list_webhooks",
        ] {
            let e = actions
                .execute(name, &json!({}), &tctx(GatewayCaller::Agent))
                .await
                .error
                .unwrap();
            assert_eq!(
                e,
                format!(
                    "{REJECTION_CODE_PREFIX}redacted read requires owner/trusted_user/co_agent"
                ),
                "{name}"
            );
        }

        // set の権限拒否 3 種（いずれもマーカー付き）
        let e = actions
            .execute(
                "set_default_subtask_webhook",
                &json!({"scope": "agent", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::TrustedUser),
            )
            .await
            .error
            .unwrap();
        assert_eq!(
            e,
            format!(
                "{REJECTION_CODE_PREFIX}set/disable requires owner (agents may manage only their own agent scope)"
            )
        );

        let e = actions
            .execute(
                "set_default_webhook",
                &json!({"scope": "global", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::Agent),
            )
            .await
            .error
            .unwrap();
        assert_eq!(
            e,
            format!(
                "{REJECTION_CODE_PREFIX}forbidden_scope: an agent may only set/disable its own agent-scope default webhook"
            )
        );

        let e = actions
            .execute(
                "set_default_webhook",
                &json!({"scope": "agent", "agent_id": "other", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::Agent),
            )
            .await
            .error
            .unwrap();
        assert_eq!(
            e,
            format!(
                "{REJECTION_CODE_PREFIX}forbidden_scope: an agent may only set/disable its own agent default webhook"
            )
        );

        // 引数エラー（拒否ではないのでマーカーは付かない）
        let e = actions
            .execute(
                "set_default_webhook",
                &json!({"url": WH_VALID_URL}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .error
            .unwrap();
        assert_eq!(e, "scope is required: 'agent' | 'tool' | 'global'");
        assert!(!e.starts_with(REJECTION_CODE_PREFIX));

        let e = actions
            .execute(
                "get_default_webhook",
                &json!({"include_secret": true}),
                &tctx(GatewayCaller::Owner),
            )
            .await
            .error
            .unwrap();
        assert_eq!(e, "include_secret is not supported");
        assert!(!e.starts_with(REJECTION_CODE_PREFIX));
    }

    /// **URL のホスト許可リストを緩めていない**。
    ///
    /// `validate_webhook_url` は下位層（#157 S4）に降りているが、set の入口でそれを
    /// 通していることをここで固定する。文言も移設前と同一。
    #[tokio::test]
    async fn host_allowlist_is_still_enforced_on_set() {
        let (actions, db) = make_test_actions();
        for bad in [
            "https://evil.example.com/api/webhooks/1/tok",
            "http://discord.com/api/webhooks/1/tok",
            "https://discord.com/not-a-webhook",
        ] {
            let result = actions
                .execute(
                    "set_default_webhook",
                    &json!({"scope": "agent", "url": bad}),
                    &tctx(GatewayCaller::Owner),
                )
                .await;
            assert!(!result.success, "{bad} が通ってしまう");
            let e = result.error.unwrap();
            assert!(
                e.starts_with("invalid_webhook_url: "),
                "{bad}: 文言が変わっている: {e}"
            );
        }
        // 拒否された URL は 1 行も保存されていない。
        let conn = db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_webhook_config(&conn, Some("test-agent"), true)
            .unwrap();
        assert!(
            rows.is_empty(),
            "検証に落ちた URL が保存されている: {rows:?}"
        );
    }

    /// **成功応答の JSON キー集合**を固定する（移設で増減していないこと）。
    #[tokio::test]
    async fn success_response_json_keys_are_unchanged() {
        let (actions, _db) = make_test_actions();

        let set = actions
            .execute(
                "set_default_webhook",
                &json!({"scope": "agent", "url": WH_VALID_URL}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let mut keys: Vec<&String> = set
            .data
            .as_ref()
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "agent_id",
                "enabled",
                "family",
                "redacted_url",
                "scope",
                "tool_name"
            ]
        );
        assert!(json_has_no_raw_token(set.data.as_ref().unwrap()));

        let get = actions
            .execute(
                "get_default_webhook",
                &json!({}),
                &tctx(GatewayCaller::Owner),
            )
            .await;
        let mut keys: Vec<&String> = get
            .data
            .as_ref()
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "enabled",
                "events",
                "family",
                "redacted_url",
                "scope",
                "source",
                "status"
            ]
        );

        let list = actions
            .execute("list_webhooks", &json!({}), &tctx(GatewayCaller::Owner))
            .await;
        let items = list.data.as_ref().unwrap()["webhooks"].as_array().unwrap();
        let mut keys: Vec<&String> = items[0].as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "agent_id",
                "enabled",
                "events",
                "kind",
                "max_chars",
                "name",
                "output_mode",
                "redacted_url",
                "scope",
                "tool_name",
                "updated_at"
            ]
        );
        assert!(json_has_no_raw_token(list.data.as_ref().unwrap()));
    }
}
