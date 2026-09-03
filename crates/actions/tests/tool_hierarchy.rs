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

    // 等値 pin（テストレビュー指摘・強化）: 非 owner 会話ターンの投影集合を、この caller で
    // 「可視な常時集合」＝会話 op 3（gateway）＋memory 参照 3＋ledger 3＋read_skill（dispatcher）
    // ＋describe_tools と**完全一致**で pin する。存在 assert だけだと search/browse_memory_index
    // や record_task_progress/close_task を落とす実装でも緑になるため、欠落も余分も落とす等値で。
    // execute_shell / subtask 3 は非 owner では不可視（policy・owner-only or 本モック未供給）なので
    // ここには入らない。常時集合 14 全ての等値は owner 版
    // （`owner_turn_projects_exactly_the_always_set_plus_describe_tools`）が受け持つ。
    assert_projected_set_eq(
        &c.names,
        &[
            "reply",
            "reaction",
            "resolve",
            "retrieve_memory_nodes",
            "search_memory_index",
            "browse_memory_index",
            "open_task",
            "record_task_progress",
            "close_task",
            "read_skill",
            "describe_tools",
        ],
        "非 owner 会話ターンの投影集合が常時集合（可視部分）＋describe_tools と一致しない",
    );

    // 個数 ≤15（等値 pin に含意されるが冗長に残す）。
    assert!(
        c.names.len() <= 15,
        "投影 functions が 15 を超えた: {:?}",
        c.names
    );

    // 【粗い sanity のみ】mock 境界のバイト数上限。本モックの gateway op は短い description
    // なので、これは「常時集合に絞れているか」の粗い sanity であって、実 description 込みの
    // 実バイト（実測 9,187B 相当）の受入判定ではない。実バイトの受入は PR 本文の隔離実測表で。
    assert!(
        c.bytes <= 10_000,
        "投影 functions の serialized バイト数が sanity 上限超過: {} > 10000",
        c.bytes
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

/// 常時集合 14 個すべて（会話 op 3＋execute_shell＋subtask 3）を供給する gateway。
/// ＋常時集合外の set_my_heartbeat（narrowing で落ちる/index 行き）。owner 等値 pin で
/// 「常時集合の 14 個が 1 つも欠けない」ことを検証するのに使う。
struct FullAlwaysGateway;

#[async_trait]
impl GatewayActions for FullAlwaysGateway {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        let mk = |name: &str, dispatch: DispatchMode, sharing: ToolSharing| GatewayActionDef {
            name: name.to_string(),
            description: format!("{name} op"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            class: ToolClass {
                dispatch,
                sub_engine: SubEngineAccess::NotExposed,
                sharing,
            },
        };
        vec![
            mk(
                "reply",
                DispatchMode::Utterance,
                ToolSharing::ConversationBound,
            ),
            mk(
                "reaction",
                DispatchMode::Utterance,
                ToolSharing::ConversationBound,
            ),
            mk(
                "resolve",
                DispatchMode::Inline,
                ToolSharing::ConversationBound,
            ),
            mk(
                "execute_shell",
                DispatchMode::Inline,
                ToolSharing::AgentBound,
            ),
            mk(
                "spawn_subtask",
                DispatchMode::Inline,
                ToolSharing::AgentBound,
            ),
            mk(
                "cancel_subtask",
                DispatchMode::Inline,
                ToolSharing::AgentBound,
            ),
            mk(
                "steer_subtask",
                DispatchMode::Inline,
                ToolSharing::AgentBound,
            ),
            // 常時集合の外（設定カテゴリ）。narrowing で投影から落ちる。
            mk(
                "set_my_heartbeat",
                DispatchMode::Inline,
                ToolSharing::AgentBound,
            ),
        ]
    }
    async fn execute(
        &self,
        name: &str,
        _a: &serde_json::Value,
        _c: &GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: true,
            data: Some(serde_json::json!({"ok": name})),
            error: None,
        }
    }
}

/// 投影名集合が期待集合と**完全一致**することを確認する（余分も欠落も落とす）。
fn assert_projected_set_eq(actual: &[String], expected: &[&str], ctx: &str) {
    use std::collections::BTreeSet;
    let a: BTreeSet<&str> = actual.iter().map(|s| s.as_str()).collect();
    let e: BTreeSet<&str> = expected.iter().copied().collect();
    let missing: Vec<&&str> = e.iter().filter(|x| !a.contains(*x)).collect();
    let extra: Vec<&&str> = a.iter().filter(|x| !e.contains(*x)).collect();
    assert_eq!(
        a, e,
        "{ctx}\n  欠落（narrowing しすぎ）: {missing:?}\n  余分（narrowing 漏れ）: {extra:?}\n  実集合: {a:?}"
    );
}

// ---------------------------------------------------------------------------
// §1-1 強化: owner 会話ターンで投影集合が「常時集合 14 ＋ describe_tools」と**等値**。
// 存在 assert だけだと execute_shell / subtask 3 / memory / ledger を落とす実装でも緑に
// なる（テストレビュー非ブロック指摘）。ここは可視な常時集合 14 個すべてを gateway/dispatcher
// で用意し、投影が正確に 15 個（14＋describe_tools）であることを等値で pin する。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn owner_turn_projects_exactly_the_always_set_plus_describe_tools() {
    let (_dir, ctx) = ctx_for(CallerIdentity::Owner);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(std::sync::Arc::new(FullAlwaysGateway));
    let (llm, captures) = CapturingMockLlm::new(vec![text_response("done")]);

    let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
    engine
        .run("You are the owner's agent", "hi", "mock-model")
        .await
        .unwrap();

    let caps = captures.lock().unwrap();
    assert_eq!(caps.len(), 1);
    // 常時集合 14（会話 op 3＋execute_shell＋subtask 3＋memory 3＋ledger 3＋read_skill）＋
    // describe_tools。set_my_heartbeat は常時集合外なので投影されない（narrowing 漏れ検出）。
    assert_projected_set_eq(
        &caps[0].names,
        &[
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
            "describe_tools",
        ],
        "owner 会話ターンの投影集合が常時集合 14＋describe_tools と一致しない",
    );
}
