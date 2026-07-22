//! エージェント（owner 限定）が OpenCrab 自体の設定を変更するためのサーバ内ツール源。
//!
//! `AppState`（db / llm_router / llm_config）を必要とするため、素の dispatcher
//! アクションでは配線できない。`GatewayActions` として実装し、`BridgedExecutor` の
//! 単一 `gateway_actions` スロットに載せる。既存の gateway（Discord/Nostr 等）を
//! `inner` として保持し、自分が扱わないツールは委譲する（composite）ことで、
//! transport 非依存に「設定ツール」を全ターンへ供給する。
//!
//! owner ゲートは bridge の `OWNER_ONLY_ACTIONS`（可視性 + 実行の双方）が担うが、
//! 多層防御として本ハンドラでも caller を確認する（fail-closed）。

use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::{
    GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext, GatewayCaller,
};
use serde_json::{json, Value};

use crate::AppState;

/// `configure_llm_provider` などのサーバ内設定ツールを提供する `GatewayActions`。
pub struct SystemGatewayActions {
    state: AppState,
    /// transport 固有の gateway（Discord/Nostr 等）。自分が扱わないツールを委譲する。
    inner: Option<Arc<dyn GatewayActions>>,
}

impl SystemGatewayActions {
    pub fn new(state: AppState, inner: Option<Arc<dyn GatewayActions>>) -> Self {
        Self { state, inner }
    }

    /// 本ツール源が直接提供するツール定義。
    fn own_definitions() -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "configure_llm_provider".to_string(),
                description:
                    "LLM プロバイダの設定を即時適用する（owner 限定）。DB オーバーライドに\
                保存してルーターをホットスワップするため再起動は不要。codex/cursor は適用後に\
                起動確認（health_check）を行い、失敗した場合は自動的に直前の設定へロールバック\
                して結果で通知する。acp と API キー型は自動ロールバックの対象外（acp の起動確認は\
                ネットワーク依存で誤判定しうるため。ダッシュボードの接続テストで明示的に確認する）。\
                各フィールドは三値: 省略=変更しない / null=オーバーライド解除（TOML に戻す）/ 値=上書き。\
                api_key はこのツールでは変更できない（ダッシュボードから設定する）。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "provider": {
                            "type": "string",
                            "description": "対象プロバイダ名（例: acp, codex, cursor, openai）。"
                        },
                        "enabled": {
                            "type": ["boolean", "null"],
                            "description": "有効/無効。null で解除。"
                        },
                        "default_model": {
                            "type": ["string", "null"],
                            "description": "既定モデル。null で解除。"
                        },
                        "binary_path": {
                            "type": ["string", "null"],
                            "description": "起動バイナリ（subprocess）。空文字/null で解除。"
                        },
                        "args": {
                            "type": ["array", "null"],
                            "items": { "type": "string" },
                            "description": "起動引数（subprocess）。null で解除。"
                        },
                        "working_dir": {
                            "type": ["string", "null"],
                            "description": "作業ディレクトリ。空文字/null で解除。"
                        },
                        "timeout_secs": {
                            "type": ["integer", "null"],
                            "description": "タイムアウト秒。null で解除。"
                        },
                        "reasoning_effort": {
                            "type": ["string", "null"],
                            "description": "推論強度（low/medium/high 等）。空文字/null で解除。"
                        },
                        "base_url": {
                            "type": ["string", "null"],
                            "description": "API ベース URL。null で解除。"
                        }
                    },
                    "required": ["provider"]
                }),
            },
            GatewayActionDef {
                name: "manage_allowed_commands".to_string(),
                description:
                    "自分（このエージェント）が execute_shell で実行できる許可コマンドを\
                管理する（owner 限定）。許可コマンドの追加はシェル実行範囲を広げるため owner のみ。"
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["list", "add", "remove"],
                            "description": "list=一覧 / add=追加 / remove=削除。"
                        },
                        "command": {
                            "type": "string",
                            "description": "add/remove 対象のコマンド（例: git, cargo）。list では不要。"
                        }
                    },
                    "required": ["action"]
                }),
            },
        ]
    }

    async fn configure_llm_provider(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("configure_llm_provider requires owner".to_string());
        }
        let Some(provider) = args.get("provider").and_then(|v| v.as_str()) else {
            return err("provider is required".to_string());
        };
        let provider = provider.to_string();

        // LLM 由来の args から許可フィールドだけを抜き出して三値ボディを組む。
        // api_key は意図的に受け付けない（秘密情報を LLM 経路・ログに載せない）。
        let mut body = serde_json::Map::new();
        for key in [
            "enabled",
            "default_model",
            "binary_path",
            "args",
            "working_dir",
            "timeout_secs",
            "reasoning_effort",
            "base_url",
        ] {
            if let Some(v) = args.get(key) {
                body.insert(key.to_string(), v.clone());
            }
        }

        match crate::api::providers::apply_provider_override_with_rollback(
            &self.state,
            &provider,
            &body,
        )
        .await
        {
            Ok(outcome) => {
                let data = json!({
                    "provider": provider,
                    "applied": outcome.applied,
                    "test_ok": outcome.test_ok,
                    "rolled_back": outcome.rolled_back,
                });
                if outcome.rolled_back {
                    // 適用したが起動確認に失敗 → 元に戻した。エージェントに明示的に伝える。
                    GatewayActionResult {
                        success: false,
                        data: Some(data),
                        error: Some(format!(
                            "'{provider}' の設定を適用しましたが起動確認に失敗したため、\
                             直前の設定へ自動ロールバックしました。binary_path/args/working_dir を確認してください。"
                        )),
                    }
                } else {
                    GatewayActionResult {
                        success: true,
                        data: Some(data),
                        error: None,
                    }
                }
            }
            Err((_code, msg)) => err(msg),
        }
    }

    async fn manage_allowed_commands(
        &self,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        // 多層防御: bridge が owner を強制するが、ハンドラでも fail-closed で確認する。
        if ctx.caller != GatewayCaller::Owner {
            return err("manage_allowed_commands requires owner".to_string());
        }
        let agent_id = ctx.agent_id.clone();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string());

        let conn = match self.state.db.lock() {
            Ok(c) => c,
            Err(_) => return err("db lock failed".to_string()),
        };
        match action {
            "list" => match opencrab_db::queries::list_agent_allowed_commands(&conn, &agent_id) {
                Ok(cmds) => GatewayActionResult {
                    success: true,
                    data: Some(json!({ "commands": cmds })),
                    error: None,
                },
                Err(e) => err(e.to_string()),
            },
            "add" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for add".to_string());
                };
                match opencrab_db::queries::add_agent_allowed_command(
                    &conn, &agent_id, &cmd, "owner",
                ) {
                    Ok(added) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "added": added })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            "remove" => {
                let Some(cmd) = command.filter(|s| !s.is_empty()) else {
                    return err("command is required for remove".to_string());
                };
                match opencrab_db::queries::remove_agent_allowed_command(&conn, &agent_id, &cmd) {
                    Ok(removed) => GatewayActionResult {
                        success: true,
                        data: Some(json!({ "command": cmd, "removed": removed })),
                        error: None,
                    },
                    Err(e) => err(e.to_string()),
                }
            }
            other => err(format!("unknown action: {other} (list/add/remove)")),
        }
    }
}

fn err(msg: String) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg),
    }
}

#[async_trait]
impl GatewayActions for SystemGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        let mut defs = Self::own_definitions();
        if let Some(inner) = &self.inner {
            defs.extend(inner.definitions());
        }
        defs
    }

    async fn execute(
        &self,
        name: &str,
        args: &Value,
        ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        match name {
            "configure_llm_provider" => self.configure_llm_provider(args, ctx).await,
            "manage_allowed_commands" => self.manage_allowed_commands(args, ctx).await,
            // 自分が扱わないツールは inner gateway へ委譲する。
            _ => match &self.inner {
                Some(inner) => inner.execute(name, args, ctx).await,
                None => GatewayActionResult {
                    success: false,
                    data: None,
                    error: Some(format!("Unknown action: {name}")),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_definition_shape() {
        let defs = SystemGatewayActions::own_definitions();
        let d = defs
            .iter()
            .find(|d| d.name == "configure_llm_provider")
            .expect("configure_llm_provider must be defined");
        // provider は必須。
        let required = d.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "provider"));
        // 秘密情報 api_key は LLM ツールでは露出しない（ダッシュボード専用）。
        let props = d.parameters["properties"].as_object().unwrap();
        assert!(
            !props.contains_key("api_key"),
            "api_key must not be settable via the agent tool"
        );
        // 起動系フィールドは受け付ける。
        for key in ["binary_path", "args", "working_dir", "timeout_secs"] {
            assert!(props.contains_key(key), "missing property: {key}");
        }
    }
}
