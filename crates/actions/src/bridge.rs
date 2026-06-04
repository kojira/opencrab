use std::sync::Arc;

use async_trait::async_trait;
use opencrab_core::{ActionExecutor, ActionResult as CoreActionResult, ToolDefinition};
use opencrab_gateway::GatewayActions;

use crate::dispatcher::ActionDispatcher;
use crate::traits::{ActionContext, ActionResult as ActionsActionResult};

/// ツール 1 件の実行イベント種別。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolEventStatus {
    Started,
    Completed,
    Failed,
    Rejected,
}

/// 1 ツール実行イベントの観測データ（webhook 等の sink へ渡す）。
/// raw な args/result を保持し、redaction/整形は sink 側が配送直前に行う。
pub struct ToolEvent<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: &'a str,
    pub agent_id: &'a str,
    pub session_id: Option<&'a str>,
    pub depth: u32,
    pub status: ToolEventStatus,
    pub started_at: &'a str,
    pub duration_ms: Option<u64>,
    pub args: &'a serde_json::Value,
    pub result: Option<&'a serde_json::Value>,
    pub error: Option<&'a str>,
}

/// ツール実行イベントの sink。executor が start/terminal で呼ぶ。
pub trait ToolEventSink: Send + Sync {
    fn on_event(&self, event: &ToolEvent<'_>);
}

/// エラー文言から「権限拒否（実行されなかった）」を推定する。
/// rejected は failed（実行されたが失敗）と区別するためのヒューリスティック。
fn is_rejection(error: Option<&str>) -> bool {
    let Some(e) = error else {
        return false;
    };
    let lower = e.to_ascii_lowercase();
    [
        "owner-only",
        "requires owner",
        "forbidden",
        "permission",
        "not allowed",
        "not permitted",
        "redacted read requires",
        "denied",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

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
    tool_event_sink: Option<Arc<dyn ToolEventSink>>,
}

impl BridgedExecutor {
    pub fn new(dispatcher: ActionDispatcher, context: ActionContext) -> Self {
        Self {
            dispatcher,
            context,
            gateway_actions: None,
            depth: 0,
            tool_event_sink: None,
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

    pub fn with_tool_event_sink(mut self, sink: Arc<dyn ToolEventSink>) -> Self {
        self.tool_event_sink = Some(sink);
        self
    }

    /// 実際のディスパッチ本体（dispatcher → gateway fallback）。
    /// instrumentation は `ActionExecutor::execute` 側で wrap する。
    async fn dispatch_inner(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
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
                map.insert(
                    "__agent_id".to_string(),
                    serde_json::json!(&self.context.agent_id),
                );
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
}

#[async_trait]
impl ActionExecutor for BridgedExecutor {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
        let Some(sink) = self.tool_event_sink.clone() else {
            return self.dispatch_inner(name, args).await;
        };
        let call_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().to_rfc3339();
        let session_id = self.context.session_id.as_deref();
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: &call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status: ToolEventStatus::Started,
            started_at: &started_at,
            duration_ms: None,
            args,
            result: None,
            error: None,
        });
        let start = std::time::Instant::now();
        let result = self.dispatch_inner(name, args).await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let status = if result.success {
            ToolEventStatus::Completed
        } else if is_rejection(result.error.as_deref()) {
            ToolEventStatus::Rejected
        } else {
            ToolEventStatus::Failed
        };
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: &call_id,
            agent_id: &self.context.agent_id,
            session_id,
            depth: self.depth,
            status,
            started_at: &started_at,
            duration_ms: Some(duration_ms),
            args,
            result: Some(&result.data),
            error: result.error.as_deref(),
        });
        result
    }

    fn list_tools(&self) -> Vec<ToolDefinition> {
        let is_owner = matches!(self.context.caller, crate::traits::CallerIdentity::Owner);
        const OWNER_ONLY_ACTIONS: &[&str] =
            &["update_instructions", "update_heartbeat_instructions"];

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
                cache_control: None,
            })
            .collect();

        // Discord-specific actions that are blocked at depth >= 1 (sub-engines cannot send to Discord directly)
        const DISCORD_ACTIONS: &[&str] = &[
            "discord_send",
            "discord_send_file",
            "discord_react",
            "discord_delete_message",
            "discord_edit_message",
            "discord_start_thread",
            "discord_list_channels",
            "discord_get_channel_info",
            "discord_list_guilds",
            "discord_set_channel_writable",
            "discord_whitelist_channel",
            "discord_add_reaction",
            "discord_remove_reaction",
            "discord_send_reply",
            "discord_send_with_embed",
            "discord_pin_message",
            "discord_unpin_message",
        ];
        const MAX_DEPTH: u32 = 2;

        // Merge gateway action definitions.
        if let Some(ref gw) = self.gateway_actions {
            // trusted-only gateway actions: excluded for non-trusted callers
            let trusted_only_actions = [
                "create_skill",
                "execute_skill",
                "read_heartbeat_instructions",
            ];
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
                    cache_control: None,
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
    use std::sync::Mutex;

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

    /// update_heartbeat_instructions / read_heartbeat_instructions を含むモック。
    struct MockGatewayHeartbeat;

    #[async_trait]
    impl GatewayActions for MockGatewayHeartbeat {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "update_heartbeat_instructions".to_string(),
                    description: "update".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "read_heartbeat_instructions".to_string(),
                    description: "read".to_string(),
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
            .execute("generate_inner_voice", &json!({"thought": "hello"}))
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

        assert!(
            names.contains(&"create_skill"),
            "TrustedUser should see create_skill"
        );
        assert!(
            names.contains(&"execute_skill"),
            "TrustedUser should see execute_skill"
        );
        assert!(
            names.contains(&"gw_action_a"),
            "TrustedUser should see regular gateway actions"
        );
    }

    #[test]
    fn test_list_tools_agent_cannot_see_skill_actions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));

        let tools = executor.list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(
            !names.contains(&"create_skill"),
            "Agent should NOT see create_skill"
        );
        assert!(
            !names.contains(&"execute_skill"),
            "Agent should NOT see execute_skill"
        );
        assert!(
            names.contains(&"gw_action_a"),
            "Agent should still see regular gateway actions"
        );
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
    fn test_list_tools_owner_sees_update_heartbeat_instructions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
        let names: Vec<String> = executor.list_tools().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "update_heartbeat_instructions"));
        assert!(names.iter().any(|n| n == "read_heartbeat_instructions"));
    }

    #[test]
    fn test_list_tools_agent_cannot_see_heartbeat_actions() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
        let names: Vec<String> = executor.list_tools().into_iter().map(|t| t.name).collect();
        // Agent (non-owner, non-trusted) sees neither.
        assert!(!names.iter().any(|n| n == "update_heartbeat_instructions"));
        assert!(!names.iter().any(|n| n == "read_heartbeat_instructions"));
    }

    #[test]
    fn test_list_tools_trusted_user_heartbeat_read_only() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
        let names: Vec<String> = executor.list_tools().into_iter().map(|t| t.name).collect();
        // TrustedUser can read but not write (write is owner-only).
        assert!(names.iter().any(|n| n == "read_heartbeat_instructions"));
        assert!(!names.iter().any(|n| n == "update_heartbeat_instructions"));
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

    // ---- ToolEventSink ----

    struct RecordingSink {
        events: Mutex<Vec<(String, String)>>, // (tool_call_id, status)
    }
    impl ToolEventSink for RecordingSink {
        fn on_event(&self, ev: &ToolEvent<'_>) {
            let status = match ev.status {
                ToolEventStatus::Started => "started",
                ToolEventStatus::Completed => "completed",
                ToolEventStatus::Failed => "failed",
                ToolEventStatus::Rejected => "rejected",
            };
            self.events
                .lock()
                .unwrap()
                .push((ev.tool_call_id.to_string(), status.to_string()));
        }
    }

    /// owner-only エラーを返す gateway モック（rejected 判定の確認用）。
    struct MockGatewayRejecting;
    #[async_trait]
    impl GatewayActions for MockGatewayRejecting {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "rej_action".to_string(),
                description: "rej".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                error: Some("this action is owner-only".to_string()),
            }
        }
    }

    #[tokio::test]
    async fn test_tool_event_sink_started_then_completed() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink { events: Mutex::new(Vec::new()) });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_tool_event_sink(sink.clone());
        let r = executor
            .execute("generate_inner_voice", &json!({"thought": "hi"}))
            .await;
        assert!(r.success);
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].1, "started");
        assert_eq!(evs[1].1, "completed");
        // same correlation id for the pair
        assert_eq!(evs[0].0, evs[1].0);
    }

    #[tokio::test]
    async fn test_tool_event_sink_failed_on_unknown() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink { events: Mutex::new(Vec::new()) });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("nonexistent_tool", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].1, "failed");
    }

    #[tokio::test]
    async fn test_tool_event_sink_rejected_on_permission_error() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink { events: Mutex::new(Vec::new()) });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayRejecting))
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("rej_action", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs[1].1, "rejected");
    }

    #[tokio::test]
    async fn test_no_sink_is_noop() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        let r = executor
            .execute("generate_inner_voice", &json!({"thought": "hi"}))
            .await;
        assert!(r.success);
    }
}
