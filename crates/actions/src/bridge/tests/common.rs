use super::super::*;
use crate::traits::CallerIdentity;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult, GatewayActions};
use serde_json::json;

/// テスト用GatewayActionsモック
pub(super) struct MockGatewayActions;

#[async_trait]
impl GatewayActions for MockGatewayActions {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "gw_action_a".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "Gateway action A".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            GatewayActionDef {
                name: "gw_action_b".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "Gateway action B".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        match name {
            "gw_action_a" => GatewayActionResult {
                success: true,
                data: Some(json!({"result": "from_gateway"})),
                error: None,
            },
            _ => GatewayActionResult {
                success: false,
                data: None,
                error: Some(format!("Unknown gateway action: {name}")),
            },
        }
    }
}

/// Discord 送信系アクションを含むモック（depth ゲートの検証用）。
pub(super) struct MockGatewayDiscord;

#[async_trait]
impl GatewayActions for MockGatewayDiscord {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "request_peer_review".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::Blocked,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "peer review".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            GatewayActionDef {
                name: "report_progress".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::Allowed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "progress".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        ]
    }

    async fn execute(
        &self,
        _name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: true,
            data: None,
            error: None,
        }
    }
}

/// update_heartbeat_instructions / read_heartbeat_instructions を含むモック。
pub(super) struct MockGatewayHeartbeat;

#[async_trait]
impl GatewayActions for MockGatewayHeartbeat {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![
            GatewayActionDef {
                name: "update_heartbeat_instructions".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Dispatchable,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "update".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            GatewayActionDef {
                name: "read_heartbeat_instructions".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "read".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
        ]
    }

    async fn execute(
        &self,
        _name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: true,
            data: None,
            error: None,
        }
    }
}

pub(super) fn test_context_with_caller(
    caller: CallerIdentity,
) -> (tempfile::TempDir, ActionContext) {
    let conn = opencrab_db::init_memory().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
    let ctx = ActionContext {
        caller,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: opencrab_db::Db::from_connection(conn),
        workspace: std::sync::Arc::new(ws),
        last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
        current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
        runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
    };
    (dir, ctx)
}

pub(super) fn test_context() -> (tempfile::TempDir, ActionContext) {
    let conn = opencrab_db::init_memory().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
    let ctx = ActionContext {
        caller: CallerIdentity::Owner,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: opencrab_db::Db::from_connection(conn),
        workspace: std::sync::Arc::new(ws),
        last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
        current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
        runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
    };
    (dir, ctx)
}

/// policy＋allowlist 済みの可視ツール名（§2.7 の depth0 narrowing より前の層）。
///
/// #923 で `list_tools()` は depth 0 で「常時集合（≤15）＋describe_tools」に絞る（投影の
/// 提示層）。owner-only 可視／agent gating／allowlist／gateway merge／depth ゲートといった
/// **可視性-policy の契約**は narrowing より前の層 `effective_tool_definitions()` が担うので、
/// これらの検証はこの helper（＝policy 層）に向ける。実行ゲート（dispatch_inner の policy）は
/// 無改変で、可視≠実行可否は別テストが pin する。narrowing 後の投影 ≤15 は
/// `crates/actions/tests/tool_hierarchy.rs` が等値で pin する。
pub(super) fn policy_visible_names(exec: &BridgedExecutor) -> Vec<String> {
    exec.effective_tool_definitions()
        .into_iter()
        .map(|t| t.definition.name)
        .collect()
}
