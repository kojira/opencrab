use std::sync::Arc;

use async_trait::async_trait;
use opencrab_core::{
    ActionExecutor,
    ActionResult as CoreActionResult,
    ToolDefinition,
};
use opencrab_gateway::GatewayActions;

use crate::dispatcher::ActionDispatcher;
use crate::traits::{ActionContext, ActionResult as ActionsActionResult};

/// Bridges `ActionDispatcher` to the `ActionExecutor` trait so that
/// `SkillEngine` can drive real actions.
///
/// Holds both the dispatcher and a pre-configured `ActionContext`.
/// Optionally holds `GatewayActions` to merge gateway-specific tools.
pub struct BridgedExecutor {
    dispatcher: ActionDispatcher,
    context: ActionContext,
    gateway_actions: Option<Arc<dyn GatewayActions>>,
    depth: u32,
}

impl BridgedExecutor {
    pub fn new(dispatcher: ActionDispatcher, context: ActionContext) -> Self {
        Self {
            dispatcher,
            context,
            gateway_actions: None,
            depth: 0,
        }
    }

    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_gateway_actions(mut self, actions: Arc<dyn GatewayActions>) -> Self {
        self.gateway_actions = Some(actions);
        self
    }
}

#[async_trait]
impl ActionExecutor for BridgedExecutor {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
        // Try dispatcher first.
        let actions_result = self.dispatcher.execute(name, args, &self.context).await;
        if actions_result.error.as_deref() != Some(&format!("Unknown action: {name}")) {
            return actions_result.into();
        }

        // Fallback to gateway actions.
        if let Some(ref gw) = self.gateway_actions {
            // Inject caller identity so gateway actions can do permission checks.
            let mut enriched_args = args.clone();
            if let serde_json::Value::Object(ref mut map) = enriched_args {
                let caller_str = match &self.context.caller {
                    crate::traits::CallerIdentity::Owner => "owner",
                    crate::traits::CallerIdentity::Agent => "agent",
                    crate::traits::CallerIdentity::CoAgent { .. } => "co_agent",
                    crate::traits::CallerIdentity::TrustedUser => "trusted_user",
                };
                map.insert("__caller".to_string(), serde_json::json!(caller_str));
                if let Some(ref session_id) = self.context.session_id {
                    map.insert("__session_id".to_string(), serde_json::json!(session_id));
                }
                map.insert("__depth".to_string(), serde_json::json!(self.depth));
                map.insert("__agent_id".to_string(), serde_json::json!(&self.context.agent_id));
            }
            let gw_result = gw.execute(name, &enriched_args).await;
            return CoreActionResult {
                success: gw_result.success,
                data: gw_result.data.unwrap_or(serde_json::Value::Null),
                error: gw_result.error,
            };
        }

        actions_result.into()
    }

    fn list_tools(&self) -> Vec<ToolDefinition> {
        let is_owner = matches!(self.context.caller, crate::traits::CallerIdentity::Owner);
        const OWNER_ONLY_ACTIONS: &[&str] = &["update_instructions"];

        let mut tools: Vec<ToolDefinition> = self
            .dispatcher
            .get_definitions(&[])
            .into_iter()
            .filter(|d| {
                // owner_only_actions はOwnerのみに見せる
                if !is_owner && OWNER_ONLY_ACTIONS.contains(&d.name.as_str()) {
                    return false;
                }
                true
            })
            .map(|d| ToolDefinition {
                name: d.name,
                description: d.description,
                parameters: d.parameters,
            })
            .collect();

        // Discord-specific actions that are blocked at depth >= 1 (sub-engines cannot send to Discord directly)
        const DISCORD_ACTIONS: &[&str] = &[
            "discord_send", "discord_send_file", "discord_react",
            "discord_delete_message", "discord_edit_message",
            "discord_start_thread", "discord_list_channels", "discord_get_channel_info",
            "discord_list_guilds", "discord_set_channel_writable", "discord_whitelist_channel",
            "discord_add_reaction", "discord_remove_reaction", "discord_send_reply",
            "discord_send_with_embed", "discord_pin_message", "discord_unpin_message",
        ];
        const MAX_DEPTH: u32 = 2;

        // Merge gateway action definitions.
        if let Some(ref gw) = self.gateway_actions {
            // trusted-only gateway actions: excluded for non-trusted callers
            let trusted_only_actions = ["create_skill", "execute_skill"];
            let is_trusted = matches!(
                self.context.caller,
                crate::traits::CallerIdentity::Owner
                    | crate::traits::CallerIdentity::CoAgent { .. }
                    | crate::traits::CallerIdentity::TrustedUser
            );
            for def in gw.definitions() {
                // At depth >= 1: block Discord-specific actions (sub-engines cannot directly send to Discord)
                if self.depth >= 1 && DISCORD_ACTIONS.contains(&def.name.as_str()) {
                    continue;
                }
                // At depth >= MAX_DEPTH: block spawn_subtask (prevent infinite nesting)
                if self.depth >= MAX_DEPTH && def.name == "spawn_subtask" {
                    continue;
                }
                if !is_trusted && trusted_only_actions.contains(&def.name.as_str()) {
                    continue;
                }
                if !is_owner && OWNER_ONLY_ACTIONS.contains(&def.name.as_str()) {
                    continue;
                }
                tools.push(ToolDefinition {
                    name: def.name,
                    description: def.description,
                    parameters: def.parameters,
                });
            }
        }

        tools
    }
}

impl From<ActionsActionResult> for CoreActionResult {
    fn from(ar: ActionsActionResult) -> Self {
        CoreActionResult {
            success: ar.success,
            data: ar.data.unwrap_or(serde_json::Value::Null),
            error: ar.error,
        }
    }
}

// Static assertion: BridgedExecutor must be Send + Sync (required by ActionExecutor).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BridgedExecutor>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CallerIdentity;
    use opencrab_gateway::{GatewayActionDef, GatewayActionResult};
    use serde_json::json;

    /// テスト用GatewayActionsモック
    struct MockGatewayActions;

    #[async_trait]
    impl GatewayActions for MockGatewayActions {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "gw_action_a".to_string(),
                    description: "Gateway action A".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "gw_action_b".to_string(),
                    description: "Gateway action B".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ]
        }

        async fn execute(&self, name: &str, _args: &serde_json::Value) -> GatewayActionResult {
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

    fn test_context_with_caller(caller: CallerIdentity) -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            caller,
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: std::sync::Arc::new(std::sync::Mutex::new(conn)),
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

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            caller: CallerIdentity::Owner,
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: std::sync::Arc::new(std::sync::Mutex::new(conn)),
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

    // ---- list_tools ----

    #[test]
    fn test_list_tools_without_gateway_actions() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        // ディスパッチャーのアクションのみ
        assert!(!tools.is_empty());
        assert!(tools.iter().all(|t| t.name != "gw_action_a"));
    }

    #[test]
    fn test_list_tools_merges_gateway_actions() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        // ディスパッチャーのアクションが含まれる
        assert!(names.contains(&"send_speech"));
        // ゲートウェイアクションもマージされる
        assert!(names.contains(&"gw_action_a"));
        assert!(names.contains(&"gw_action_b"));
    }

    // ---- execute ----

    #[tokio::test]
    async fn test_execute_dispatcher_action() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        // ディスパッチャーに存在するアクションはディスパッチャーで処理される
        let result = executor
            .execute("send_speech", &json!({"content": "hello"}))
            .await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_execute_falls_back_to_gateway_actions() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        // ディスパッチャーに存在しないアクションはゲートウェイにフォールバック
        let result = executor.execute("gw_action_a", &json!({})).await;
        assert!(result.success);
        assert_eq!(result.data["result"], "from_gateway");
    }

    #[tokio::test]
    async fn test_execute_unknown_action_without_gateway() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        // ゲートウェイなし → ディスパッチャーのエラーがそのまま返る
        let result = executor.execute("nonexistent", &json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_execute_unknown_action_with_gateway() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActions));

        // ディスパッチャーにもゲートウェイにも無い → ゲートウェイのエラーが返る
        let result = executor.execute("totally_unknown", &json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown gateway action"));
    }

    /// create_skill / execute_skill を含むモック
    struct MockGatewayActionsWithSkills;

    #[async_trait]
    impl GatewayActions for MockGatewayActionsWithSkills {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "gw_action_a".to_string(),
                    description: "Gateway action A".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "create_skill".to_string(),
                    description: "Create a skill".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "execute_skill".to_string(),
                    description: "Execute a skill".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
            ]
        }

        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> GatewayActionResult {
            GatewayActionResult {
                success: true,
                data: None,
                error: None,
            }
        }
    }

    #[test]
    fn test_list_tools_trusted_user_sees_skill_actions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"create_skill"), "TrustedUser should see create_skill");
        assert!(names.contains(&"execute_skill"), "TrustedUser should see execute_skill");
        assert!(names.contains(&"gw_action_a"), "TrustedUser should see regular gateway actions");
    }

    #[test]
    fn test_list_tools_agent_cannot_see_skill_actions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(!names.contains(&"create_skill"), "Agent should NOT see create_skill");
        assert!(!names.contains(&"execute_skill"), "Agent should NOT see execute_skill");
        assert!(names.contains(&"gw_action_a"), "Agent should still see regular gateway actions");
    }

    // ---- owner_only_actions filtering ----

    #[test]
    fn test_list_tools_owner_sees_update_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"update_instructions"),
            "Owner should see update_instructions"
        );
    }

    #[test]
    fn test_list_tools_agent_cannot_see_update_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            !names.contains(&"update_instructions"),
            "Agent should NOT see update_instructions"
        );
    }

    #[test]
    fn test_list_tools_trusted_user_cannot_see_update_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            !names.contains(&"update_instructions"),
            "TrustedUser should NOT see update_instructions"
        );
    }
}
