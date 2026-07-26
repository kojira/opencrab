//! デフォルト webhook の **新規作成つき** ゲートウェイアクション（`ensure_*`）。
//!
//! `ensure_webhook`（既定 `family='activity'`）/ `ensure_subtask_webhook`（既定
//! `family='subtask'`）は、既存デフォルトがあれば返す（読み取り権限で可）。無ければ
//! owner 限定で `channel_id` から webhook を作成して upsert する。`family` 引数で
//! ファミリを明示上書きできる。
//!
//! raw url/token は決して返さない・記録しない（`redact_webhook_url` のみ）。
//!
//! # なぜこの 2 本だけ Discord に残るのか（#157 S5）
//!
//! 可視化・管理の 6 本（`get/set_default_[subtask_]webhook` / `list_[subtask_]webhooks`）は
//! DB と設定ファイル由来の既定値しか触らないため、gateway 非依存層
//! （`crates/server/src/webhook_targets.rs`）へ移設した。一方この 2 本は、既存デフォルトが
//! 無いときに `execute_discord_create_webhook`（serenity 依存の transport 固有処理）を呼ぶ。
//! 「解決部分は下位層・作成部分は Discord」に割ることは可能だが、実装が 1 つしか無い抽象
//! を増やすだけなので S5 では行わない。
//!
//! activity family のツール活動配送は、spawn_subtask の sub-engine（depth >= 1）だけでなく
//! depth0/メインエージェントの executor にも `ToolEventSink` を挿して行う
//! （`spawn_activity_tool_event_sink` を message_loop 側 executor へ配線）。
//!
//! # 残課題
//!
//! - **`output_mode` の適用**: DB には保存・list で返すが、整形側（build_tool_event_message）
//!   は常に summary 相当。`full` ストリーミングは未実装。`max_chars` はクランプに適用済み。

use opencrab_gateway::{GatewayActionResult, GatewayCallContext, GatewayCaller};
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
pub(crate) fn reject(msg: impl Into<String>) -> GatewayActionResult {
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

    /// 後方互換: 既定 subtask family の ensure。
    pub(crate) async fn execute_ensure_subtask_webhook(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        self.execute_ensure_webhook_impl(args, ctx, "subtask").await
    }

    /// 汎用: 既定 activity family の ensure。
    pub(crate) async fn execute_ensure_webhook(
        &self,
        args: &serde_json::Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        self.execute_ensure_webhook_impl(args, ctx, "activity")
            .await
    }

    async fn execute_ensure_webhook_impl(
        &self,
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

        let family = args
            .get("family")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(default_family)
            .to_string();
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
                .unwrap_or(&ctx.agent_id)
                .to_string()
        };
        let tool_name =
            Self::default_tool_name(&scope, args.get("tool_name").and_then(|v| v.as_str()));

        // 既存デフォルトを解決する（webhook キー無し）。family で解決経路を分ける。
        let resolve_args = json!({});
        let existing = {
            let conn = self.db.lock().unwrap();
            if family == "activity" {
                webhook::resolve_activity_webhook(&conn, &agent_id, &tool_name)
            } else {
                webhook::resolve_subtask_webhook(
                    &conn,
                    &agent_id,
                    &tool_name,
                    &resolve_args,
                    self.default_subtask_webhook.as_ref(),
                )
            }
        };

        if let WebhookResolution::Use { config, source } = existing {
            return GatewayActionResult {
                success: true,
                data: Some(json!({
                    "scope": scope_for_source(source),
                    "source": source.as_str(),
                    "family": family,
                    "enabled": true,
                    "events": config.events,
                    "redacted_url": redact_webhook_url(&config.url),
                    "created": false,
                })),
                error: None,
            };
        }

        // 作成が必要 → owner 限定 + channel_id 必須。
        if ctx.caller != GatewayCaller::Owner {
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
            kind: family.clone(),
            url: raw_url.clone(),
            events_json,
            enabled: true,
            name,
            created_by: Some(ctx.caller.label().to_string()),
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
                    caller = %ctx.caller.label(),
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
                        "family": family,
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
