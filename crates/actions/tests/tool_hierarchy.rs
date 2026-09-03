//! 受け入れテスト（TDD・赤先行）: ツール階層（#923 / DESIGN-SYSTEM-PROMPT-V2 §2.6/§2.7）。
//!
//! 観測境界 = **SkillEngine が LLM へ渡す `ChatRequest.functions`**。engine は毎イテレーション
//! `executor.list_tools()` を取り直して functions を組む（§2.7）ので、ここで capture する
//! functions は「そのターンでモデルに投影された関数集合」そのもの。モックの内部コールバック
//! ではなく、engine → provider の実リクエストを見る。
//!
//! 期待挙動（env ゲート無し・既定）:
//! - depth 0 の会話ターンでは投影関数を**常時集合（≤15）＋describe_tools**に絞る。
//! - 常時集合の外のツール（例 set_my_heartbeat / 記憶管理・内省など）は投影に出ない（余分 0）。
//! - `describe_tools([...])` を呼ぶと、**同一ターンの次の LLM 呼び出し**の functions に
//!   その名前が加わる（呼ばないターンでは加わらない）。
//!
//! 現 tip（2fe4c1e0）では list_tools が全ツールを返すため、count / 名前集合 / describe_tools
//! 存在の各 assert が**赤**。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencrab_actions::bridge::BridgedExecutor;
use opencrab_actions::dispatcher::ActionDispatcher;
use opencrab_actions::traits::{ActionContext, CallerIdentity};
use opencrab_core::{ChatRequest, ChatResponse, LlmClient, SkillEngine, ToolCall};
use opencrab_gateway::{
    DispatchMode, GatewayActionDef, GatewayActionResult, GatewayActions, GatewayCallContext,
    SubEngineAccess, ToolClass, ToolSharing,
};
use opencrab_llm_types::{Choice, FunctionCall, Message, MessageContent, Role, Usage};

/// 常時集合（DIRECTION-LOG 510・実測 14）。レーン/owner 依存で「見える部分集合」になる。
const ALWAYS: &[&str] = &[
    "reply",
    "reaction",
    "resolve",
    "execute_shell",
    "spawn_subtask",
    "cancel_subtask",
    "steer_subtask",
    "retrieve_memory_nodes",
    "search_memory_index",
    "browse_memory_index",
    "open_task",
    "record_task_progress",
    "close_task",
    "read_skill",
];

/// 1 回の LLM 呼び出しで投影された functions の観測（名前集合と serialized バイト数）。
#[derive(Clone, Debug, Default)]
struct CallCapture {
    names: Vec<String>,
    bytes: usize,
}

/// pre-queued 応答を順に返しつつ、各リクエストの `functions` を記録する MockLlm。
struct CapturingMockLlm {
    responses: Mutex<Vec<ChatResponse>>,
    captures: Arc<Mutex<Vec<CallCapture>>>,
}

impl CapturingMockLlm {
    fn new(responses: Vec<ChatResponse>) -> (Self, Arc<Mutex<Vec<CallCapture>>>) {
        let captures = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                responses: Mutex::new(responses),
                captures: captures.clone(),
            },
            captures,
        )
    }
}

#[async_trait]
impl LlmClient for CapturingMockLlm {
    async fn chat(&self, req: ChatRequest) -> anyhow::Result<ChatResponse> {
        let names = req
            .functions
            .as_ref()
            .map(|fs| fs.iter().map(|f| f.name.clone()).collect())
            .unwrap_or_default();
        let bytes = serde_json::to_string(&req.functions)
            .map(|s| s.len())
            .unwrap_or(0);
        self.captures
            .lock()
            .unwrap()
            .push(CallCapture { names, bytes });

        let mut rs = self.responses.lock().unwrap();
        if rs.is_empty() {
            anyhow::bail!("CapturingMockLlm: no more responses");
        }
        Ok(rs.remove(0))
    }
}

fn tc(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: name.to_string(),
            arguments: serde_json::to_string(&args).unwrap(),
        },
    }
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse {
        id: String::new(),
        model: String::new(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: Some(MessageContent::Text(text.to_string())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
            finish_reason: None,
        }],
        usage: Usage::default(),
        created: 0,
    }
}

fn tool_call_response(calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        id: String::new(),
        model: String::new(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: None,
                name: None,
                function_call: None,
                tool_calls: Some(calls),
                tool_call_id: None,
            },
            finish_reason: None,
        }],
        usage: Usage::default(),
        created: 0,
    }
}

/// 会話ターンで露出する gateway op を供給するモック。
/// - `reply` / `reaction` / `resolve`: 常時集合の会話 op（Discord/Nostr レーン）。
/// - `set_my_heartbeat`: 常時集合の**外**（設定カテゴリ）。describe_tools の活性化検証に使う。
struct ConversationGateway;

#[async_trait]
impl GatewayActions for ConversationGateway {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        let conv = |name: &str, dispatch: DispatchMode| GatewayActionDef {
            name: name.to_string(),
            description: format!("{name} op"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            class: ToolClass {
                dispatch,
                sub_engine: SubEngineAccess::NotExposed,
                sharing: ToolSharing::ConversationBound,
            },
        };
        vec![
            conv("reply", DispatchMode::Utterance),
            conv("reaction", DispatchMode::Utterance),
            conv("resolve", DispatchMode::Inline),
            // 常時集合の外（設定カテゴリ・index 経由でのみ describe_tools 対象）。
            GatewayActionDef {
                name: "set_my_heartbeat".to_string(),
                description: "set heartbeat interval".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"minutes": {"type": "integer"}}
                }),
                class: ToolClass {
                    dispatch: DispatchMode::Inline,
                    sub_engine: SubEngineAccess::NotExposed,
                    sharing: ToolSharing::AgentBound,
                },
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        _args: &serde_json::Value,
        _ctx: &GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: true,
            data: Some(serde_json::json!({"ok": name})),
            error: None,
        }
    }
}

fn ctx_for(caller: CallerIdentity) -> (tempfile::TempDir, ActionContext) {
    let conn = opencrab_db::init_memory().unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
    let ctx = ActionContext {
        caller,
        agent_id: "agent-1".to_string(),
        agent_name: "Test Agent".to_string(),
        session_id: Some("session-1".to_string()),
        db: opencrab_db::Db::from_connection(conn),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(Mutex::new(None)),
        model_override: Arc::new(Mutex::new(None)),
        current_purpose: Arc::new(Mutex::new("conversation".to_string())),
        runtime_info: Arc::new(Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test-model".to_string(),
            active_model: None,
            available_providers: vec!["mock".to_string()],
            gateway: "test".to_string(),
        })),
    };
    (dir, ctx)
}

/// executor（depth 0・会話 gateway 注入）を組む。
fn executor_with_conv_gateway(caller: CallerIdentity) -> (tempfile::TempDir, BridgedExecutor) {
    let (dir, ctx) = ctx_for(caller);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(ConversationGateway));
    (dir, executor)
}

// ---------------------------------------------------------------------------
// §1-1: Discord 会話ターン（非 owner）— 投影 functions ≤15 / ≤10,000B / 名前集合一致（余分 0）
// ---------------------------------------------------------------------------
#[tokio::test]
async fn discord_nonowner_turn_projects_only_always_set_plus_describe_tools() {
    let (_dir, executor) = executor_with_conv_gateway(CallerIdentity::Agent);
    let (llm, captures) = CapturingMockLlm::new(vec![text_response("done")]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    engine
        .run("You are an agent", "hi", "mock-model")
        .await
        .unwrap();

    let caps = captures.lock().unwrap();
    assert_eq!(caps.len(), 1, "1 ターン = 1 LLM 呼び出し");
    let c = &caps[0];

    // 個数 ≤15。
    assert!(
        c.names.len() <= 15,
        "投影 functions が 15 を超えた: {} 個 {:?}",
        c.names.len(),
        c.names
    );
    // serialized バイト数 ≤10,000（実測 9,187 を基にした上限）。
    assert!(
        c.bytes <= 10_000,
        "投影 functions の serialized バイト数が上限超過: {} > 10000",
        c.bytes
    );
    // describe_tools が常時投影される。
    assert!(
        c.names.iter().any(|n| n == "describe_tools"),
        "describe_tools が投影に無い: {:?}",
        c.names
    );
    // 余分 0: 投影は常時集合 ∪ {describe_tools} の部分集合。
    for n in &c.names {
        assert!(
            n == "describe_tools" || ALWAYS.contains(&n.as_str()),
            "常時集合外のツールが投影された（余分）: {n}\n{:?}",
            c.names
        );
    }
    // 会話 op（レーン依存の常時集合）が見える。
    for expect in ["reply", "reaction", "resolve"] {
        assert!(
            c.names.iter().any(|n| n == expect),
            "会話 op {expect} が投影に無い: {:?}",
            c.names
        );
    }
    // 記憶参照・ledger・read_skill（非 owner でも見える常時集合）。
    for expect in ["retrieve_memory_nodes", "open_task", "read_skill"] {
        assert!(
            c.names.iter().any(|n| n == expect),
            "常時集合 {expect} が投影に無い: {:?}",
            c.names
        );
    }
    // 否定側（narrowing が効いている証拠）: 常時集合外は投影されない。
    assert!(
        !c.names.iter().any(|n| n == "set_my_heartbeat"),
        "設定カテゴリの set_my_heartbeat が会話ターンに漏れた: {:?}",
        c.names
    );
    assert!(
        !c.names.iter().any(|n| n == "search_my_history"),
        "常時集合外の dispatcher ツール search_my_history が漏れた: {:?}",
        c.names
    );
}

// ---------------------------------------------------------------------------
// §1-3: describe_tools 活性化 — 同一ターンの次の LLM 呼び出しの functions に加わる
// ---------------------------------------------------------------------------
#[tokio::test]
async fn describe_tools_activates_named_tool_for_next_llm_call() {
    let (_dir, executor) = executor_with_conv_gateway(CallerIdentity::Owner);
    // 1 回目: describe_tools(["set_my_heartbeat"]) を呼ぶ。2 回目: text で終端。
    let (llm, captures) = CapturingMockLlm::new(vec![
        tool_call_response(vec![tc(
            "call-1",
            "describe_tools",
            serde_json::json!({"names": ["set_my_heartbeat"]}),
        )]),
        text_response("done"),
    ]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    engine
        .run(
            "You are the owner's agent",
            "heartbeat を 30 分に",
            "mock-model",
        )
        .await
        .unwrap();

    let caps = captures.lock().unwrap();
    assert_eq!(caps.len(), 2, "describe_tools→終端で 2 回の LLM 呼び出し");

    // 1 回目: 常時集合のみ（set_my_heartbeat は未活性・count ≤15・describe_tools 在り）。
    let first = &caps[0];
    assert!(
        first.names.len() <= 15,
        "1 回目の投影が 15 超: {} {:?}",
        first.names.len(),
        first.names
    );
    assert!(
        first.names.iter().any(|n| n == "describe_tools"),
        "1 回目に describe_tools が無い: {:?}",
        first.names
    );
    assert!(
        !first.names.iter().any(|n| n == "set_my_heartbeat"),
        "呼ぶ前から set_my_heartbeat が投影されている（活性化前提が崩れる）: {:?}",
        first.names
    );

    // 2 回目: describe_tools で活性化した set_my_heartbeat が加わる（count は依然 ≤15）。
    let second = &caps[1];
    assert!(
        second.names.len() <= 15,
        "2 回目の投影が 15 超: {} {:?}",
        second.names.len(),
        second.names
    );
    assert!(
        second.names.iter().any(|n| n == "set_my_heartbeat"),
        "describe_tools 後も set_my_heartbeat が投影に加わらない: {:?}",
        second.names
    );
}

// ---------------------------------------------------------------------------
// §1-3 否定側: describe_tools を呼ばないターンでは活性化しない
// ---------------------------------------------------------------------------
#[tokio::test]
async fn without_describe_tools_the_named_tool_stays_hidden() {
    let (_dir, executor) = executor_with_conv_gateway(CallerIdentity::Owner);
    let (llm, captures) = CapturingMockLlm::new(vec![text_response("done")]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    engine
        .run("You are the owner's agent", "hi", "mock-model")
        .await
        .unwrap();

    let caps = captures.lock().unwrap();
    assert_eq!(caps.len(), 1);
    assert!(
        !caps[0].names.iter().any(|n| n == "set_my_heartbeat"),
        "describe_tools 未呼び出しなのに set_my_heartbeat が投影された: {:?}",
        caps[0].names
    );
    assert!(
        caps[0].names.len() <= 15,
        "投影が 15 超: {:?}",
        caps[0].names
    );
}

// ---------------------------------------------------------------------------
// §1-5: REST レーン（会話 gateway 無し）でも投影 functions ≤15
// ---------------------------------------------------------------------------
#[tokio::test]
async fn rest_lane_turn_projects_at_most_15_functions() {
    // REST は reply/reaction gateway を持たない（応答は HTTP body）。
    let (_dir, ctx) = ctx_for(CallerIdentity::Agent);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    let (llm, captures) = CapturingMockLlm::new(vec![text_response("done")]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    engine
        .run("You are an agent", "hi", "mock-model")
        .await
        .unwrap();

    let caps = captures.lock().unwrap();
    assert_eq!(caps.len(), 1);
    assert!(
        caps[0].names.len() <= 15,
        "REST レーンの投影が 15 超: {} {:?}",
        caps[0].names.len(),
        caps[0].names
    );
    assert!(
        caps[0].names.iter().any(|n| n == "describe_tools"),
        "REST レーンにも describe_tools が投影されるべき: {:?}",
        caps[0].names
    );
    // 会話 op は REST レーンには無い。
    assert!(
        !caps[0].names.iter().any(|n| n == "reply"),
        "REST レーンに reply が漏れた: {:?}",
        caps[0].names
    );
}
