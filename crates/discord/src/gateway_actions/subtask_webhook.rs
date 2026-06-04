//! subtask webhook デフォルトの可視化・管理ゲートウェイアクション。
//!
//! - `get_default_subtask_webhook`: owner/trusted_user/co_agent。実際に使われる
//!   デフォルトを解決して redacted で返す。
//! - `set_default_subtask_webhook`: owner 限定。scope ごとのデフォルトを upsert する
//!   （url 空/省略は enabled=false の auditable disable）。
//! - `ensure_subtask_webhook`: 既存デフォルトがあれば返す（読み取り権限で可）。無ければ
//!   owner 限定で channel_id から webhook を作成して upsert する。
//! - `list_subtask_webhooks`: owner/trusted_user/co_agent。設定一覧を redacted で返す。
//!
//! いずれも raw url/token は決して返さない・記録しない（redact_webhook_url のみ）。
//!
//! # フォローアップ（意図的に本 PR の範囲外）
//!
//! 以下は活動 webhook の将来拡張として残す。安全に小さく出せる範囲ではないため、
//! 別 PR で扱う:
//! - **depth0 のツール活動ストリーミング**: 現状 `ToolEventSink` は spawn_subtask の
//!   sub-engine（depth >= 1）にのみ挿す。メインエージェント自身（depth0）のツール
//!   呼び出しは配送しない。実装には message_loop 側の executor 配線が必要。
//! - **汎用 API 名のエイリアス**: `set_default_webhook` / `get_default_webhook` /
//!   `ensure_webhook` / `list_webhooks`。現状は `*_subtask_webhook`（activity family を
//!   `family='activity'` で包含）。汎用名はツール面を増やすため別途設計する。
//! - **`output_mode` の適用**: DB には保存・list で返すが、整形側（build_tool_event_message）
//!   は常に summary 相当。`full` ストリーミングは未実装。`max_chars` はクランプに適用済み。

use opencrab_gateway::GatewayActionResult;
use serde_json::json;

use super::webhook::{
    self, redact_webhook_url, validate_webhook_url, WebhookResolution, WebhookSource,
};
use super::DiscordGatewayActions;

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
        error: Some(format!(
            "{}{}",
            opencrab_actions::REJECTION_CODE_PREFIX,
            msg
        )),
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

impl DiscordGatewayActions {
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

    pub(crate) fn execute_get_default_subtask_webhook(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let caller = args
            .get("__caller")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        if !matches!(caller, "owner" | "trusted_user" | "co_agent") {
            return reject("redacted read requires owner/trusted_user/co_agent");
        }
        if args
            .get("include_secret")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return err("include_secret is not supported");
        }

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.agent_id)
            .to_string();
        let tool_name = args
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // "webhook" キーを持たない args で解決し、実際に使われるデフォルトを得る。
        let resolve_args = json!({});
        let resolution = {
            let conn = self.db.lock().unwrap();
            webhook::resolve_subtask_webhook(
                &conn,
                &agent_id,
                tool_name,
                &resolve_args,
                self.default_subtask_webhook.as_ref(),
            )
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

        GatewayActionResult {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub(crate) fn execute_set_default_subtask_webhook(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let caller = args
            .get("__caller")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");

        let scope = match args.get("scope").and_then(|v| v.as_str()) {
            Some(s) if matches!(s, "agent" | "tool" | "global") => s.to_string(),
            _ => return err("scope is required: 'agent' | 'tool' | 'global'"),
        };
        // `family` を優先し、後方互換で `kind` も受ける。既定は subtask（このアクションは
        // subtask 系互換エイリアス）。activity family を設定したい場合は family='activity'。
        let kind = args
            .get("family")
            .and_then(|v| v.as_str())
            .or_else(|| args.get("kind").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .unwrap_or("subtask")
            .to_string();

        let agent_id = if scope == "global" {
            "*".to_string()
        } else {
            args.get("agent_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.agent_id)
                .to_string()
        };

        // 権限: owner は全 scope を set/disable できる。Agent は自分自身の agent scope
        // （scope='agent' かつ agent_id が自分）のみ。それ以外（tool/global/他 agent）は拒否。
        // trusted_user / co_agent は read-only（set/disable 不可）。
        match caller {
            "owner" => {}
            "agent" => {
                if scope != "agent" {
                    return reject(
                        "forbidden_scope: an agent may only set/disable its own agent-scope default webhook",
                    );
                }
                if agent_id != self.agent_id {
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
        let tool_name =
            Self::default_tool_name(&scope, args.get("tool_name").and_then(|v| v.as_str()));

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
            let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
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
        let max_chars = args.get("max_chars").and_then(|v| v.as_i64()).unwrap_or(1500);

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
            created_by: Some(caller.to_string()),
            output_mode,
            max_chars,
            updated_at: String::new(),
        };

        let result = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::upsert_agent_webhook_config(&conn, &row)
        };

        let redacted = redact_webhook_url(&url);
        match result {
            Ok(()) => {
                tracing::info!(
                    target: "webhook_audit",
                    caller = %caller,
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
                        "enabled": enabled,
                        "redacted_url": if enabled { json!(redacted) } else { serde_json::Value::Null },
                    })),
                    error: None,
                }
            }
            Err(e) => {
                tracing::info!(
                    target: "webhook_audit",
                    caller = %caller,
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

    pub(crate) async fn execute_ensure_subtask_webhook(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let caller = args
            .get("__caller")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        if !matches!(caller, "owner" | "trusted_user" | "co_agent") {
            return reject("redacted read requires owner/trusted_user/co_agent");
        }

        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .filter(|s| matches!(*s, "agent" | "tool" | "global"))
            .unwrap_or("agent")
            .to_string();
        let agent_id = if scope == "global" {
            "*".to_string()
        } else {
            args.get("agent_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.agent_id)
                .to_string()
        };
        let tool_name =
            Self::default_tool_name(&scope, args.get("tool_name").and_then(|v| v.as_str()));

        // 既存デフォルトを解決する（webhook キー無し）。
        let resolve_args = json!({});
        let existing = {
            let conn = self.db.lock().unwrap();
            webhook::resolve_subtask_webhook(
                &conn,
                &agent_id,
                &tool_name,
                &resolve_args,
                self.default_subtask_webhook.as_ref(),
            )
        };

        if let WebhookResolution::Use { config, source } = existing {
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "scope": scope_for_source(source),
                    "source": source.as_str(),
                    "enabled": true,
                    "events": config.events,
                    "redacted_url": redact_webhook_url(&config.url),
                    "created": false,
                })),
                error: None,
            };
        }

        // 作成が必要 → owner 限定 + channel_id 必須。
        if caller != "owner" {
            return reject("creating a webhook is owner-only");
        }
        let channel_id = match args.get("channel_id").and_then(|v| v.as_str()) {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => return err("channel_id required to create"),
        };
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // discord_create_webhook を呼び、raw url を取り出す（応答には返さない）。
        let create_args = json!({ "channel_id": channel_id, "name": name });
        let created = self.execute_discord_create_webhook(&create_args).await;
        if !created.success {
            return created;
        }
        let raw_url = created
            .data
            .as_ref()
            .and_then(|d| d.get("url"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if let Err(reason) = validate_webhook_url(&raw_url) {
            return err(format!("invalid_webhook_url: {reason}"));
        }

        let events = args.get("events").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        });
        let events_json = events
            .as_ref()
            .map(|e| serde_json::to_string(e).unwrap_or_default());

        let row = opencrab_db::queries::AgentWebhookConfigRow {
            scope: scope.clone(),
            agent_id: agent_id.clone(),
            tool_name: tool_name.clone(),
            kind: "subtask".to_string(),
            url: raw_url.clone(),
            events_json,
            enabled: true,
            name,
            created_by: Some(caller.to_string()),
            output_mode: "summary".to_string(),
            max_chars: 1500,
            updated_at: String::new(),
        };

        let result = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::upsert_agent_webhook_config(&conn, &row)
        };

        let redacted = redact_webhook_url(&raw_url);
        match result {
            Ok(()) => {
                tracing::info!(
                    target: "webhook_audit",
                    caller = %caller,
                    scope = %scope,
                    agent_id = %agent_id,
                    tool_name = %tool_name,
                    redacted_url = %redacted,
                    result = "created",
                    "subtask webhook action: ensure"
                );
                GatewayActionResult {
                    success: true,
                    data: Some(json!({
                        "scope": scope,
                        "agent_id": agent_id,
                        "tool_name": tool_name,
                        "enabled": true,
                        "redacted_url": redacted,
                        "created": true,
                    })),
                    error: None,
                }
            }
            Err(e) => err(format!("failed to save webhook config: {e}")),
        }
    }

    pub(crate) fn execute_list_subtask_webhooks(
        &self,
        args: &serde_json::Value,
    ) -> GatewayActionResult {
        let caller = args
            .get("__caller")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        if !matches!(caller, "owner" | "trusted_user" | "co_agent") {
            return reject("redacted read requires owner/trusted_user/co_agent");
        }

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.agent_id)
            .to_string();
        let scope_filter = args
            .get("scope")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        let include_disabled = args
            .get("include_disabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let rows = {
            let conn = self.db.lock().unwrap();
            opencrab_db::queries::list_agent_webhook_config(&conn, Some(&agent_id), include_disabled)
        };
        let rows = match rows {
            Ok(r) => r,
            Err(e) => return err(format!("failed to list webhook config: {e}")),
        };

        let items: Vec<serde_json::Value> = rows
            .into_iter()
            .filter(|r| scope_filter.as_ref().map(|s| &r.scope == s).unwrap_or(true))
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
