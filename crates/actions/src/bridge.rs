use std::sync::Arc;

use async_trait::async_trait;
use opencrab_core::{ActionExecutor, ActionResult as CoreActionResult, FunctionDefinition};
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

/// 権限ポリシーによる拒否（実行に到達しなかった）を表す構造的マーカー。
///
/// gateway action 等が permission-check で拒否したときは、エラー文言の先頭へ
/// この安定コードを付ける（`crate::reject_marker` 経由）。分類器はこの構造的な
/// 接頭辞を第一の根拠にする。`"permission"` / `"denied"` / `"forbidden"` のような
/// 広い自然言語の部分一致は、実行されたが失敗した通常のエラー（例: OS の
/// "Permission denied"、shell の "Operation not permitted"）を rejected に誤分類
/// するため使わない。
pub const REJECTION_CODE_PREFIX: &str = "rejected: ";

/// Discord 送信系アクション: depth >= 1 の sub-engine からは**定義の非表示と実行の拒否の両方**で
/// ブロックする（定義から隠すだけでは、モデルが親コンテキストの記憶で名前を呼んだ場合に素通しになる）。
pub const DISCORD_ACTIONS: &[&str] = &[
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
    "request_peer_review",
];

/// spawn_subtask のネスト上限。
const MAX_DEPTH: u32 = 2;

/// owner のみが可視・実行できるアクション（#45）。
pub const OWNER_ONLY_ACTIONS: &[&str] = &["update_instructions", "update_heartbeat_instructions"];

/// owner / co_agent / trusted_user のみ（素の Agent は不可）のアクション（#45）。
/// `execute_skill` は現行の gateway に実装が無い防御的エントリ（将来追加時に
/// 最初からゲートされるように残している）。
pub const TRUSTED_ONLY_ACTIONS: &[&str] = &[
    "create_skill",
    "execute_skill",
    "read_heartbeat_instructions",
];

/// アクション名 → 権限/深度ポリシー（#45 の単一の表）。
///
/// 以前は可視性（`list_tools`）だけがこれらのリストを参照し、実行
/// （`dispatch_inner`）は depth 系しか強制していなかったため、「一覧から
/// 隠したツールをモデルが名前指定で実行できる」食い違いがあった。
/// 可視性と実行時強制は必ずこの関数を参照すること（discord 側ハンドラの
/// typed gate は多層防御としてそのまま残る）。
pub struct ToolPolicy {
    pub owner_only: bool,
    pub trusted_only: bool,
    /// depth >= 1 の sub-engine からブロック（Discord 送信系）。
    pub blocked_in_subengine: bool,
    /// depth >= MAX_DEPTH でブロック（ネスト上限）。
    pub depth_capped: bool,
}

pub fn tool_policy(name: &str) -> ToolPolicy {
    ToolPolicy {
        owner_only: OWNER_ONLY_ACTIONS.contains(&name),
        trusted_only: TRUSTED_ONLY_ACTIONS.contains(&name),
        blocked_in_subengine: DISCORD_ACTIONS.contains(&name),
        depth_capped: name == "spawn_subtask",
    }
}

/// エラー文言から「権限拒否（実行されなかった）」を判定する。
///
/// 優先: 構造的マーカー（`REJECTION_CODE_PREFIX`）。
/// 後方互換: まだマーカー化されていない経路向けに、曖昧さの少ない明示ドメイン
/// マーカーのみを許可する（広い NL 部分一致は誤検知になるため不可）。
fn is_rejection(error: Option<&str>) -> bool {
    let Some(e) = error else {
        return false;
    };
    // 構造的シグナル（権威）。
    if e.starts_with(REJECTION_CODE_PREFIX) {
        return true;
    }
    // 後方互換の明示ドメインマーカー（未マーカー化の owner-only gateway action 等）。
    // いずれも通常の OS/ツール失敗には現れない十分に固有なトークンに限定する。
    let lower = e.to_ascii_lowercase();
    [
        "owner-only",
        "requires owner",
        "forbidden_scope",
        "redacted read requires",
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

    /// dispatcher の CallerIdentity を gateway 境界の型付き caller に写像する。
    /// CoAgent の agent_id は保存する（旧 `__caller` 文字列注入では落ちていた）。
    fn gateway_call_context(&self) -> opencrab_gateway::GatewayCallContext {
        let caller = match &self.context.caller {
            crate::traits::CallerIdentity::Owner => opencrab_gateway::GatewayCaller::Owner,
            crate::traits::CallerIdentity::Agent => opencrab_gateway::GatewayCaller::Agent,
            crate::traits::CallerIdentity::CoAgent { agent_id } => {
                opencrab_gateway::GatewayCaller::CoAgent {
                    agent_id: agent_id.clone(),
                }
            }
            crate::traits::CallerIdentity::TrustedUser => {
                opencrab_gateway::GatewayCaller::TrustedUser
            }
        };
        opencrab_gateway::GatewayCallContext {
            caller,
            session_id: self.context.session_id.clone(),
            depth: self.depth,
            agent_id: self.context.agent_id.clone(),
        }
    }

    fn caller_is_owner(&self) -> bool {
        matches!(self.context.caller, crate::traits::CallerIdentity::Owner)
    }

    fn caller_is_trusted(&self) -> bool {
        matches!(
            self.context.caller,
            crate::traits::CallerIdentity::Owner
                | crate::traits::CallerIdentity::CoAgent { .. }
                | crate::traits::CallerIdentity::TrustedUser
        )
    }

    /// このコンテキスト（caller/depth）で name が可視・実行可能か（#45）。
    /// list_tools と dispatch_inner が同一のポリシー判定を共有するための述語。
    fn policy_allows(&self, name: &str) -> bool {
        let policy = tool_policy(name);
        if policy.owner_only && !self.caller_is_owner() {
            return false;
        }
        if policy.trusted_only && !self.caller_is_trusted() {
            return false;
        }
        if self.depth >= 1 && policy.blocked_in_subengine {
            return false;
        }
        if self.depth >= MAX_DEPTH && policy.depth_capped {
            return false;
        }
        true
    }

    /// 実際のディスパッチ本体（dispatcher → gateway fallback）。
    /// instrumentation は `ActionExecutor::execute` 側で wrap する。
    async fn dispatch_inner(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
        // 可視性（list_tools）と同じポリシー表を実行時にも強制する（#45）。
        // 一覧から隠しただけでは、モデルが名前を記憶で呼んだ場合に素通しになる。
        let reject = |msg: String| CoreActionResult {
            success: false,
            data: serde_json::Value::Null,
            error: Some(format!("{REJECTION_CODE_PREFIX}{msg}")),
        };
        let policy = tool_policy(name);
        if policy.owner_only && !self.caller_is_owner() {
            return reject(format!("action '{name}' requires owner"));
        }
        if policy.trusted_only && !self.caller_is_trusted() {
            return reject(format!(
                "action '{name}' requires a trusted caller (owner/co_agent/trusted_user)"
            ));
        }
        if self.depth >= 1 && policy.blocked_in_subengine {
            return reject(format!(
                "action '{name}' is not available in sub-engines (depth {})",
                self.depth
            ));
        }
        if self.depth >= MAX_DEPTH && policy.depth_capped {
            return reject(format!(
                "{name} is not available at depth {} (max nesting: {MAX_DEPTH})",
                self.depth
            ));
        }

        // Try dispatcher first. フォールバック判定は登録有無で行う
        // （"Unknown action" エラー文言の文字列比較は、実アクションが同文を
        // 返した場合に gateway へ誤ルートするため廃止 — #36）。
        if self.dispatcher.has_action(name) {
            return self
                .dispatcher
                .execute(name, args, &self.context)
                .await
                .into();
        }

        // Fallback to gateway actions.
        if let Some(ref gw) = self.gateway_actions {
            // 実行コンテキストは型付きで渡す。LLM 由来の args には混ぜない（#36）。
            let ctx = self.gateway_call_context();
            let gw_result = gw.execute(name, args, &ctx).await;
            return CoreActionResult {
                success: gw_result.success,
                data: gw_result.data.unwrap_or(serde_json::Value::Null),
                error: gw_result.error,
            };
        }

        // dispatcher にも gateway にも無い。
        CoreActionResult {
            success: false,
            data: serde_json::Value::Null,
            error: Some(format!("Unknown action: {name}")),
        }
    }
}

impl BridgedExecutor {
    /// instrumentation 付き実行本体。
    ///
    /// `tool_call_id` は LLM 由来の元 ID を伝播するための相関キー。`Some(id)` なら
    /// その ID を webhook/トレースの相関に使う（skill engine の tool_call.id と一致）。
    /// `None`（id を持たない直接呼び出し）のときのみ合成 UUID を生成し、ペイロード上は
    /// `correlation = "synthetic"` として区別できるようにする。
    async fn execute_instrumented(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: Option<&str>,
    ) -> CoreActionResult {
        let Some(sink) = self.tool_event_sink.clone() else {
            return self.dispatch_inner(name, args).await;
        };
        // 相関 ID: LLM 由来 ID があれば伝播、無ければ合成（同 start/terminal で一致）。
        let synthetic;
        let call_id: &str = match tool_call_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                synthetic = uuid::Uuid::new_v4().to_string();
                &synthetic
            }
        };
        let started_at = chrono::Utc::now().to_rfc3339();
        let session_id = self.context.session_id.as_deref();
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
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
            // permission-policy 拒否を観測可能にする（raw URL/token は載せない）。
            tracing::debug!(
                tool = %name,
                tool_call_id = %call_id,
                depth = self.depth,
                "tool call classified as rejected (policy)"
            );
            ToolEventStatus::Rejected
        } else {
            ToolEventStatus::Failed
        };
        sink.on_event(&ToolEvent {
            tool_name: name,
            tool_call_id: call_id,
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
}

#[async_trait]
impl ActionExecutor for BridgedExecutor {
    async fn execute(&self, name: &str, args: &serde_json::Value) -> CoreActionResult {
        self.execute_instrumented(name, args, None).await
    }

    async fn execute_with_id(
        &self,
        name: &str,
        args: &serde_json::Value,
        tool_call_id: &str,
    ) -> CoreActionResult {
        self.execute_instrumented(name, args, Some(tool_call_id))
            .await
    }

    fn list_tools(&self) -> Vec<FunctionDefinition> {
        // 空 description は None にする（旧 to_function_def の挙動を踏襲）。
        let opt_desc = |d: String| if d.is_empty() { None } else { Some(d) };

        let mut tools: Vec<FunctionDefinition> = self
            .dispatcher
            .get_definitions(&[])
            .into_iter()
            .filter(|d| self.policy_allows(&d.name))
            .map(|d| FunctionDefinition {
                name: d.name,
                description: opt_desc(d.description),
                parameters: d.parameters,
                cache_control: None,
            })
            .collect();

        // Merge gateway action definitions（同じポリシー述語でフィルタ）。
        if let Some(ref gw) = self.gateway_actions {
            for def in gw.definitions() {
                if !self.policy_allows(&def.name) {
                    continue;
                }
                tools.push(FunctionDefinition {
                    name: def.name,
                    description: opt_desc(def.description),
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
    struct MockGatewayDiscord;

    #[async_trait]
    impl GatewayActions for MockGatewayDiscord {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![
                GatewayActionDef {
                    name: "request_peer_review".to_string(),
                    description: "peer review".to_string(),
                    parameters: json!({"type": "object", "properties": {}}),
                },
                GatewayActionDef {
                    name: "report_progress".to_string(),
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

    fn test_context_with_caller(caller: CallerIdentity) -> (tempfile::TempDir, ActionContext) {
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

    fn test_context() -> (tempfile::TempDir, ActionContext) {
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

    #[test]
    fn test_peer_review_visible_at_depth0_hidden_in_subengine() {
        let (_dir, ctx) = test_context();
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayDiscord));
        let names: Vec<String> = executor
            .list_tools()
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert!(names.contains(&"request_peer_review".to_string()));
        assert!(names.contains(&"report_progress".to_string()));

        // depth >= 1 の sub-engine からはピアレビュー依頼が見えない
        let (_dir2, sub_ctx) = test_context();
        let sub = BridgedExecutor::new(ActionDispatcher::new(), sub_ctx)
            .with_gateway_actions(Arc::new(MockGatewayDiscord))
            .with_depth(1);
        let names: Vec<String> = sub.list_tools().iter().map(|t| t.name.clone()).collect();
        assert!(!names.contains(&"request_peer_review".to_string()));
        assert!(names.contains(&"report_progress".to_string()));
    }

    /// 定義から隠すだけでなく、名前指定の実行も depth ゲートで拒否されること
    /// （モデルは親コンテキストの記憶でツール名を呼ぶことがある）。
    #[tokio::test]
    async fn test_peer_review_execute_rejected_in_subengine() {
        let (_dir, ctx) = test_context();
        let sub = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayDiscord))
            .with_depth(1);
        let result = sub.execute("request_peer_review", &json!({})).await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.starts_with(REJECTION_CODE_PREFIX));
        assert!(err.contains("not available in sub-engines"));

        // ブロック対象外の gateway action は depth 1 でも実行できる
        let result = sub.execute("report_progress", &json!({})).await;
        assert!(result.success);
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

    // ---- #36: typed GatewayCallContext ----

    /// gateway に渡った ctx / args を記録するモック。
    struct CtxRecordingGateway {
        last_ctx: Mutex<Option<opencrab_gateway::GatewayCallContext>>,
        last_args: Mutex<Option<serde_json::Value>>,
    }

    #[async_trait]
    impl GatewayActions for CtxRecordingGateway {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "ctx_probe".to_string(),
                description: "probe".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            args: &serde_json::Value,
            ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            *self.last_ctx.lock().unwrap() = Some(ctx.clone());
            *self.last_args.lock().unwrap() = Some(args.clone());
            GatewayActionResult {
                success: true,
                data: None,
                error: None,
            }
        }
    }

    /// CoAgent の agent_id が境界を越えて保存されること（旧 `__caller` 文字列注入では
    /// "co_agent" に落ちていた）と、LLM 由来 args に実行コンテキストが混ざらないこと。
    #[tokio::test]
    async fn test_gateway_receives_typed_context_preserving_coagent_id() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::CoAgent {
            agent_id: "co-agent-42".to_string(),
        });
        let gw = Arc::new(CtxRecordingGateway {
            last_ctx: Mutex::new(None),
            last_args: Mutex::new(None),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(gw.clone())
            .with_depth(1);

        let result = executor.execute("ctx_probe", &json!({"x": 1})).await;
        assert!(result.success);

        let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
        assert_eq!(
            seen.caller,
            opencrab_gateway::GatewayCaller::CoAgent {
                agent_id: "co-agent-42".to_string()
            }
        );
        assert_eq!(seen.session_id.as_deref(), Some("session-1"));
        assert_eq!(seen.depth, 1);
        assert_eq!(seen.agent_id, "agent-1");

        // args は LLM 由来のものがそのまま渡り、__* キーは注入されない。
        let args = gw.last_args.lock().unwrap().clone().unwrap();
        assert_eq!(args, json!({"x": 1}));
    }

    /// "Unknown action: {name}" と同文のエラーを返す実アクションが gateway に
    /// 誤ルートされないこと（旧実装はエラー文言の文字列比較で判定していた）。
    struct UnknownEchoAction;
    #[async_trait]
    impl crate::traits::Action for UnknownEchoAction {
        fn name(&self) -> &str {
            "unknown_echo"
        }
        fn description(&self) -> &str {
            "returns an error that mimics the dispatcher's unknown-action message"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
            _ctx: &crate::traits::ActionContext,
        ) -> crate::traits::ActionResult {
            crate::traits::ActionResult::error("Unknown action: unknown_echo")
        }
    }

    #[tokio::test]
    async fn test_registered_action_with_unknown_action_error_not_misrouted() {
        let (_dir, ctx) = test_context();
        let mut dispatcher = ActionDispatcher::new();
        dispatcher.register(Arc::new(UnknownEchoAction));
        let gw = Arc::new(CtxRecordingGateway {
            last_ctx: Mutex::new(None),
            last_args: Mutex::new(None),
        });
        let executor = BridgedExecutor::new(dispatcher, ctx).with_gateway_actions(gw.clone());

        let result = executor.execute("unknown_echo", &json!({})).await;
        // dispatcher の結果がそのまま返り、gateway へはフォールバックしない。
        assert!(!result.success);
        assert_eq!(
            result.error.as_deref(),
            Some("Unknown action: unknown_echo")
        );
        assert!(gw.last_ctx.lock().unwrap().is_none());
    }

    // ---- #45: 実行時ポリシー強制（可視性と対称） ----

    /// owner-only の dispatcher アクションは、一覧から隠れるだけでなく
    /// 名前指定の実行も bridge で拒否されること。
    #[tokio::test]
    async fn test_owner_only_dispatcher_action_rejected_at_execute_for_agent() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        let result = executor
            .execute("update_instructions", &json!({"instructions": "x"}))
            .await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.starts_with(REJECTION_CODE_PREFIX));
        assert!(err.contains("requires owner"));
    }

    #[tokio::test]
    async fn test_owner_only_action_executes_for_owner() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        // owner はポリシーを通過して dispatcher 本体に到達する（結果の成否は本体次第）。
        let result = executor
            .execute("update_instructions", &json!({"instructions": "x"}))
            .await;
        if let Some(err) = &result.error {
            assert!(
                !err.starts_with(REJECTION_CODE_PREFIX),
                "owner must not be policy-rejected: {err}"
            );
        }
    }

    /// trusted-only の gateway アクションは、素の Agent からの名前指定実行が
    /// gateway に到達する前に bridge で拒否されること（旧実装はモックまで素通し）。
    #[tokio::test]
    async fn test_trusted_only_gateway_action_rejected_at_execute_for_agent() {
        let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
        let result = executor.execute("create_skill", &json!({})).await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.starts_with(REJECTION_CODE_PREFIX));
        assert!(err.contains("trusted"));

        // trusted_user は通過してモック（success）に到達する
        let (_dir2, ctx2) = test_context_with_caller(CallerIdentity::TrustedUser);
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx2)
            .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
        let result = executor.execute("create_skill", &json!({})).await;
        assert!(result.success);
    }

    /// ポリシー表のドリフト検出: dispatcher 側の owner-only 名は実在する
    /// アクションであること（表が死に名を指したまま実アクションが野放しになる事故の防止）。
    #[test]
    fn test_policy_owner_only_dispatcher_names_are_live() {
        let dispatcher = ActionDispatcher::new();
        let names = dispatcher.action_names();
        assert!(
            names.iter().any(|n| n == "update_instructions"),
            "update_instructions must exist in dispatcher"
        );
        // update_heartbeat_instructions / create_skill / read_heartbeat_instructions は
        // gateway 側（discord crate のテストで実在性を検証）。execute_skill は防御的
        // エントリ（実装なし）であることをここで明文化する。
        assert!(!names.iter().any(|n| n == "execute_skill"));
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
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
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
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
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
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        let _ = executor.execute("nonexistent_tool", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[1].1, "failed");
    }

    #[tokio::test]
    async fn test_tool_event_sink_rejected_on_permission_error() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayRejecting))
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("rej_action", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs[1].1, "rejected");
    }

    // ---- M1: structured rejection classification ----

    /// 構造マーカー接頭辞付きのエラーを返す gateway モック（構造的 rejected 判定用）。
    struct MockGatewayStructuredReject;
    #[async_trait]
    impl GatewayActions for MockGatewayStructuredReject {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "sr_action".to_string(),
                description: "sr".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                // reject() ヘルパが付ける構造マーカーを模す。
                error: Some(format!("{REJECTION_CODE_PREFIX}forbidden_scope: nope")),
            }
        }
    }

    /// "permission denied" を含む通常の実行失敗を返す gateway モック。
    /// これは実行されたが失敗したケースで、rejected に誤分類されてはならない。
    struct MockGatewayOrdinaryPermFailure;
    #[async_trait]
    impl GatewayActions for MockGatewayOrdinaryPermFailure {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "perm_fail".to_string(),
                description: "pf".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            }]
        }
        async fn execute(
            &self,
            _name: &str,
            _args: &serde_json::Value,
            _ctx: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: false,
                data: None,
                // OS 由来の通常失敗。広い NL 一致なら誤って rejected になる。
                error: Some("write failed: Permission denied (os error 13)".to_string()),
            }
        }
    }

    #[test]
    fn test_is_rejection_structured_marker() {
        assert!(is_rejection(Some(&format!(
            "{REJECTION_CODE_PREFIX}anything at all"
        ))));
    }

    #[test]
    fn test_is_rejection_ignores_ordinary_permission_failures() {
        // 実行されたが失敗した通常エラーは rejected ではない。
        assert!(!is_rejection(Some("Permission denied (os error 13)")));
        assert!(!is_rejection(Some("operation not permitted")));
        assert!(!is_rejection(Some("forbidden by remote host")));
        assert!(!is_rejection(Some("access denied to file")));
    }

    #[test]
    fn test_is_rejection_legacy_domain_markers() {
        // マーカー未付与の owner-only gateway action 等は後方互換で検知する。
        assert!(is_rejection(Some("this action is owner-only")));
        assert!(is_rejection(Some("forbidden_scope: ...")));
        assert!(is_rejection(Some("redacted read requires owner")));
    }

    #[tokio::test]
    async fn test_tool_event_sink_rejected_on_structured_marker() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayStructuredReject))
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("sr_action", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs[1].1, "rejected");
    }

    #[tokio::test]
    async fn test_tool_event_sink_ordinary_permission_failure_is_failed_not_rejected() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayOrdinaryPermFailure))
            .with_tool_event_sink(sink.clone());
        let _ = executor.execute("perm_fail", &json!({})).await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs[1].1, "failed", "ordinary failure must not be rejected");
    }

    // ---- M2: tool_call_id propagation ----

    #[tokio::test]
    async fn test_execute_with_id_propagates_tool_call_id() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        let r = executor
            .execute_with_id(
                "generate_inner_voice",
                &json!({"thought": "hi"}),
                "llm-call-42",
            )
            .await;
        assert!(r.success);
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        // start/terminal の両方が LLM 由来 ID を伝播する。
        assert_eq!(evs[0].0, "llm-call-42");
        assert_eq!(evs[1].0, "llm-call-42");
    }

    #[tokio::test]
    async fn test_execute_without_id_synthesizes_stable_pair() {
        let (_dir, ctx) = test_context();
        let sink = Arc::new(RecordingSink {
            events: Mutex::new(Vec::new()),
        });
        let executor =
            BridgedExecutor::new(ActionDispatcher::new(), ctx).with_tool_event_sink(sink.clone());
        // id 無し: 合成 UUID だが start/terminal で一致する。
        let _ = executor
            .execute("generate_inner_voice", &json!({"thought": "hi"}))
            .await;
        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 2);
        assert!(!evs[0].0.is_empty());
        assert_eq!(evs[0].0, evs[1].0);
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
