//! エージェントに露出する MCP ツール源（`GatewayActions` 実装）。
//!
//! `BridgedExecutor` が MCP スロットとしてマージし、`mcp__<server>__<tool>` を LLM の
//! ツール一覧に足す。呼び出しは対応する MCP サーバの `tools/call` へ委譲する。
//!
//! **per-turn** に構築する（本ターンの caller の信頼度を capture）。`trusted_only` な
//! サーバは、信頼された呼び出し元（owner/co_agent/trusted_user）のターンでのみ
//! 一覧に出し、実行も許可する（外部ユーザー起点のプロンプトインジェクション対策）。

use std::sync::Arc;

use async_trait::async_trait;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext};
use serde_json::json;

use crate::client::{McpClient, McpTool, McpToolResult};
use crate::config::{qualified_tool_name, split_tool_name};

/// プロバイダが必要とする MCP サーバの最小境界（`McpClient` が実装）。トレイト化して
/// おくことで、権限ゲート等のロジックをモックでユニットテストできる。
#[async_trait]
pub trait McpServer: Send + Sync {
    fn server_name(&self) -> &str;
    fn tools(&self) -> &[McpTool];
    async fn call_tool(&self, name: &str, args: serde_json::Value)
        -> anyhow::Result<McpToolResult>;
}

#[async_trait]
impl McpServer for McpClient {
    fn server_name(&self) -> &str {
        McpClient::server_name(self)
    }
    fn tools(&self) -> &[McpTool] {
        McpClient::tools(self)
    }
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<McpToolResult> {
        McpClient::call_tool(self, name, args).await
    }
}

/// 接続済みの1サーバ（server ハンドル + 権限）。
#[derive(Clone)]
pub struct ConnectedServer {
    pub server: Arc<dyn McpServer>,
    /// true なら信頼された呼び出し元のターンでのみ使える。
    pub trusted_only: bool,
}

/// 本ターン用の MCP ツール源。
pub struct McpToolProvider {
    servers: Vec<ConnectedServer>,
    caller_is_trusted: bool,
}

impl McpToolProvider {
    pub fn new(servers: Vec<ConnectedServer>, caller_is_trusted: bool) -> Self {
        Self {
            servers,
            caller_is_trusted,
        }
    }

    /// このサーバを本ターンの caller が使えるか（trusted_only ゲート）。
    fn allowed(&self, s: &ConnectedServer) -> bool {
        !s.trusted_only || self.caller_is_trusted
    }

    fn find(&self, server_name: &str) -> Option<&ConnectedServer> {
        self.servers
            .iter()
            .find(|s| s.server.server_name() == server_name)
    }
}

fn err(msg: impl Into<String>) -> GatewayActionResult {
    GatewayActionResult {
        success: false,
        data: None,
        error: Some(msg.into()),
    }
}

#[async_trait]
impl GatewayActions for McpToolProvider {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        let mut defs = Vec::new();
        for s in &self.servers {
            if !self.allowed(s) {
                continue; // trusted_only を untrusted caller には出さない
            }
            let server = s.server.server_name();
            for t in s.server.tools() {
                defs.push(GatewayActionDef {
                    name: qualified_tool_name(server, &t.name),
                    description: format!("[MCP:{server}] {}", t.description),
                    parameters: t.input_schema.clone(),
                });
            }
        }
        defs
    }

    async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        let Some((server, tool)) = split_tool_name(name) else {
            return err(format!("MCP ツール名ではありません: {name}"));
        };
        let Some(s) = self.find(server) else {
            return err(format!("MCP サーバが見つかりません: {server}"));
        };
        // 可視性と対称に、実行時も trusted_only を強制する（名前直呼び対策）。
        if !self.allowed(s) {
            return err(format!(
                "rejected: mcp__{server} は信頼された呼び出し元（owner/co_agent/trusted_user）のみ利用できます"
            ));
        }
        match s.server.call_tool(tool, args.clone()).await {
            // サーバが isError を返した場合はツールエラーとして error 経路に流す
            // （モデルには本文が渡る）。
            Ok(res) if res.is_error => GatewayActionResult {
                success: false,
                data: Some(json!({ "result": res.text.clone() })),
                error: Some(res.text),
            },
            Ok(res) => GatewayActionResult {
                success: true,
                data: Some(json!({ "result": res.text })),
                error: None,
            },
            Err(e) => err(format!("MCP {name} 実行失敗: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_gateway::GatewayCallContext;
    use serde_json::{json, Value};

    struct MockServer {
        name: String,
        tools: Vec<McpTool>,
    }

    #[async_trait]
    impl McpServer for MockServer {
        fn server_name(&self) -> &str {
            &self.name
        }
        fn tools(&self) -> &[McpTool] {
            &self.tools
        }
        async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<McpToolResult> {
            Ok(McpToolResult {
                text: format!("{name}:{args}"),
                is_error: false,
            })
        }
    }

    fn server(name: &str, trusted_only: bool) -> ConnectedServer {
        ConnectedServer {
            server: Arc::new(MockServer {
                name: name.to_string(),
                tools: vec![McpTool {
                    name: "do".to_string(),
                    description: "d".to_string(),
                    input_schema: json!({"type": "object"}),
                }],
            }),
            trusted_only,
        }
    }

    #[test]
    fn test_definitions_gate_trusted_only() {
        let servers = vec![server("pub", false), server("sec", true)];
        // untrusted: trusted_only の sec は出さない。
        let p = McpToolProvider::new(servers.clone(), false);
        let names: Vec<String> = p.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["mcp__pub__do"]);
        // trusted: 両方出す。
        let p = McpToolProvider::new(servers, true);
        let names: Vec<String> = p.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"mcp__pub__do".to_string()));
        assert!(names.contains(&"mcp__sec__do".to_string()));
    }

    #[tokio::test]
    async fn test_execute_rejects_untrusted_on_trusted_only() {
        let ctx = GatewayCallContext::for_agent("a1");
        let p = McpToolProvider::new(vec![server("sec", true)], false);
        // untrusted が trusted_only を名指し → 実行時も拒否（可視性と対称）。
        let r = p.execute("mcp__sec__do", &json!({}), &ctx).await;
        assert!(!r.success);
        assert!(r.error.unwrap().contains("rejected"));
        // trusted なら実行できる。
        let p = McpToolProvider::new(vec![server("sec", true)], true);
        let r = p.execute("mcp__sec__do", &json!({"x": 1}), &ctx).await;
        assert!(r.success);
        assert!(r.data.unwrap()["result"].as_str().unwrap().contains("do:"));
    }

    #[tokio::test]
    async fn test_execute_unknown_and_non_mcp() {
        let ctx = GatewayCallContext::for_agent("a1");
        let p = McpToolProvider::new(vec![server("pub", false)], false);
        // 存在しないサーバ。
        let r = p.execute("mcp__nope__do", &json!({}), &ctx).await;
        assert!(!r.success);
        // MCP 名でない。
        let r = p.execute("not_mcp", &json!({}), &ctx).await;
        assert!(!r.success);
    }
}
