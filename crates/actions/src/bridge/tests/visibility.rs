use super::super::*;
use super::common::*;
use crate::traits::CallerIdentity;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult};
use serde_json::json;

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

/// (a) 実行時の select_llm 定義は RuntimeInfo の登録済みプロバイダだけを出す。
#[test]
fn select_llm_tool_schema_omits_unregistered_providers() {
    let (_dir, ctx) = test_context();
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    // #923: select_llm は常時集合外（index 行き）。schema の検証は policy 層で行う。
    let tool = executor
        .effective_tool_definitions()
        .into_iter()
        .find(|t| t.definition.name == "select_llm")
        .expect("select_llm")
        .definition;
    let desc = tool.description.unwrap_or_default();
    let params = tool.parameters.to_string();
    assert!(desc.contains("mock"), "{desc}");
    assert!(
        !desc.contains("openai"),
        "未登録 openai が説明に出ている: {desc}"
    );
    assert!(params.contains("mock"), "{params}");
    assert!(
        !params.contains("openai"),
        "未登録 openai がパラメータに出ている: {params}"
    );
}

#[test]
fn test_list_tools_merges_gateway_actions() {
    let (_dir, ctx) = test_context();
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayActions));

    // #923: gateway merge は可視性-policy の契約。narrowing 前の policy 層で検証する。
    let names = policy_visible_names(&executor);
    let has = |n: &str| names.iter().any(|x| x == n);

    // ゲートウェイアクションもマージされる
    assert!(has("gw_action_a"));
    assert!(has("gw_action_b"));
}

// ---- run 単位のツール許可リスト（#368）----

/// MCP スロット検証用: `mcp__` 名前空間の外部ツールを 1 つ定義するモック。
struct MockMcpSlot;

#[async_trait]
impl GatewayActions for MockMcpSlot {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        vec![GatewayActionDef {
            name: "mcp__ext__send".to_string(),
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
            description: "external send".to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        }]
    }
    async fn execute(
        &self,
        name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        GatewayActionResult {
            success: true,
            data: Some(json!({ "reached": name })),
            error: None,
        }
    }
}

/// 許可リストは **3 スロット全て**（dispatcher core / gateway own / MCP）を、
/// **可視性（list_tools）と実行（dispatch）の両方**で絞る。許可リスト無し（None）なら
/// 従来どおり全部見える（＝対話ターン・heartbeat・subtask の不変性の裏付け）。
#[tokio::test]
async fn tool_allowlist_gates_all_slots_visibility_and_execution() {
    // 許可リスト: 読み取り 1 + タグ 1 + 終了宣言 1（整理ランの最小形）。
    let allow = vec![
        "browse_memory_index".to_string(),
        "tag_topic".to_string(),
        "declare_done".to_string(),
    ];

    // --- 許可リスト無し（None）: 全スロットのツールが見える（不変性の対照） ---
    let (_dir0, ctx0) = test_context();
    let unrestricted = BridgedExecutor::new(ActionDispatcher::new(), ctx0)
        .with_gateway_actions(Arc::new(MockGatewayActions)) // gw_action_a/b（gateway own 相当）
        .with_mcp_actions(Arc::new(MockMcpSlot)); // mcp__ext__send（MCP スロット）
                                                  // #923: allowlist の可視性は narrowing 前の policy＋allowlist 層で検証する
                                                  // （list_tools は depth0 で常時集合に絞るため、allowlist 契約は effective 層で見る）。
    let base: Vec<String> = policy_visible_names(&unrestricted);
    // dispatcher core / gateway own / MCP がどれも見える。
    assert!(
        base.contains(&"ws_delete".to_string()),
        "core が見える: {base:?}"
    );
    assert!(
        base.contains(&"gw_action_a".to_string()),
        "gateway own が見える"
    );
    assert!(base.contains(&"mcp__ext__send".to_string()), "MCP が見える");

    // --- 許可リスト有り（Some）: 許可外は全スロットで消える ---
    let (_dir1, ctx1) = test_context();
    let restricted = BridgedExecutor::new(ActionDispatcher::new(), ctx1)
        .with_gateway_actions(Arc::new(MockGatewayActions))
        .with_mcp_actions(Arc::new(MockMcpSlot))
        .with_tool_allowlist(Some(allow.clone()));
    let visible: Vec<String> = policy_visible_names(&restricted);
    // 経路2（policy＋allowlist 可視性）: 許可されたものだけ見える。
    assert!(visible.contains(&"browse_memory_index".to_string()));
    assert!(visible.contains(&"tag_topic".to_string()));
    assert!(visible.contains(&"declare_done".to_string()));
    // 3 スロットの許可外ツールがどれも消える。
    for forbidden in ["ws_delete", "gw_action_a", "mcp__ext__send"] {
        assert!(
            !visible.contains(&forbidden.to_string()),
            "許可外 {forbidden} が list_tools に残っている: {visible:?}"
        );
    }

    // 経路3（実行）: 許可外は dispatch で拒否（rejected: マーカー）。
    for (forbidden, slot) in [
        ("ws_delete", "dispatcher core"),
        ("gw_action_a", "gateway own"),
        ("mcp__ext__send", "MCP"),
    ] {
        let r = restricted.execute(forbidden, &json!({})).await;
        assert!(!r.success, "{slot} の {forbidden} は拒否されるべき");
        let err = r.error.unwrap_or_default();
        assert!(
            err.starts_with(REJECTION_CODE_PREFIX),
            "{slot} の {forbidden} は構造的拒否であるべき: {err}"
        );
        // gateway/MCP には届いていない（実行痕跡 reached が無い）。
        assert!(
            r.data.get("reached").is_none(),
            "{slot} の {forbidden} は実装へ届いてはならない"
        );
    }

    // 許可されたツールは実行が拒否されない（tag_topic は書き込み・DB 依存だが、
    // 少なくとも許可リストでの拒否は受けない）。
    let ok = restricted.execute("browse_memory_index", &json!({})).await;
    assert!(
        !is_rejection(ok.error.as_deref()),
        "許可ツールが許可リストで拒否されてはならない: {:?}",
        ok.error
    );
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
    // #923: depth0 で request_peer_review は投影に出さない（#921・設計 §2.7 L168）が、
    // policy 層（＝depth ゲートを持つ層）には出る。depth ゲートの契約はこの層で検証する。
    let names: Vec<String> = policy_visible_names(&executor);
    assert!(names.contains(&"request_peer_review".to_string()));
    assert!(names.contains(&"report_progress".to_string()));

    // depth >= 1 の sub-engine からはピアレビュー依頼が見えない
    let (_dir2, sub_ctx) = test_context();
    let sub = BridgedExecutor::new(ActionDispatcher::new(), sub_ctx)
        .with_gateway_actions(Arc::new(MockGatewayDiscord))
        .with_depth(1);
    // depth>0 では list_tools は narrowing しない（常時集合の絞りは depth0 のみ）ので、
    // sub-engine の depth ゲート（sub_engine=Blocked）はそのまま list_tools で観測できる。
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
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "Gateway action A".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            GatewayActionDef {
                name: "create_skill".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Dispatchable,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "Create a skill".to_string(),
                parameters: json!({"type": "object", "properties": {}}),
            },
            GatewayActionDef {
                name: "execute_skill".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
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

    // #923: owner/trusted 限定の可視性は narrowing 前の policy 層で検証する。
    let names = policy_visible_names(&executor);
    let has = |n: &str| names.iter().any(|x| x == n);

    assert!(has("create_skill"), "TrustedUser should see create_skill");
    assert!(has("execute_skill"), "TrustedUser should see execute_skill");
    assert!(
        has("gw_action_a"),
        "TrustedUser should see regular gateway actions"
    );
}

#[test]
fn test_list_tools_agent_cannot_see_skill_actions() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));

    // #923: agent gating の可視性は narrowing 前の policy 層で検証する。
    let names = policy_visible_names(&executor);
    let has = |n: &str| names.iter().any(|x| x == n);

    assert!(!has("create_skill"), "Agent should NOT see create_skill");
    assert!(!has("execute_skill"), "Agent should NOT see execute_skill");
    assert!(
        has("gw_action_a"),
        "Agent should still see regular gateway actions"
    );
}

/// #298: subtask 決着で resume したターンでも owner/trusted のツールが残る。
///
/// `policy_allows` がこのバグの実体（`trusted_only && !caller_is_trusted()` で
/// **list_tools からも dispatch からも**落ちる）なので、ここで固定するのは
/// 「決着通知が運ぶ呼び出し元でツール一覧を組めば元ターンと同じ集合になる」こと。
/// 通知の caller は `settle_completed` が registry のエントリから読む実物を使う。
#[tokio::test]
async fn resumed_turn_keeps_owner_and_trusted_tools() {
    use crate::subtask::{
        settle_completed, SettleContext, SpawnedSubtask, SubtaskCompletionSink, SubtaskLifecycle,
        SubtaskRegistry, SubtaskSettled,
    };

    /// 決着通知を 1 件だけ捕まえる sink。
    #[derive(Default)]
    struct CaptureSink(std::sync::Mutex<Option<SubtaskSettled>>);
    impl SubtaskCompletionSink for CaptureSink {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, ev: SubtaskSettled) {
            *self.0.lock().unwrap() = Some(ev);
        }
    }

    // owner 発のターンが subtask を spawn した状態を作る。
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let registry: SubtaskRegistry = std::sync::Arc::new(dashmap::DashMap::new());
    registry.insert(
        "st-1".to_string(),
        SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: "subtask-st-1".to_string(),
            parent_session_id: "discord-agent-1-1-2".to_string(),
            agent_id: "agent-1".to_string(),
            label: "job".to_string(),
            tool_name: "spawn_subtask".to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: CallerIdentity::Owner,
            lifecycle: SubtaskLifecycle::new(),
            steerable: false,
        },
    );

    let sink = CaptureSink::default();
    settle_completed(
        &registry,
        &db,
        &sink,
        SettleContext {
            parent_session_id: "discord-agent-1-1-2".to_string(),
            agent_id: "agent-1".to_string(),
            subtask_id: "st-1".to_string(),
            sub_session_id: "subtask-st-1".to_string(),
            exit_reason: "completed".to_string(),
            lifecycle: SubtaskLifecycle::new(),
        },
        "done",
    );
    let ev = sink.0.lock().unwrap().take().expect("sink が発火する");

    // resume 側は決着通知の caller で実行文脈を組む。
    let (_dir, ctx) = test_context_with_caller(ev.caller);
    let resumed = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
    // #923: owner/trusted の可視性維持は narrowing 前の policy 層で検証する。
    let names: Vec<String> = policy_visible_names(&resumed);
    assert!(
        names.iter().any(|n| n == "create_skill"),
        "resume 後に trusted_only のツールが消えている: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "update_instructions"),
        "resume 後に owner_only のツールが消えている: {names:?}"
    );

    // 対照: 最小権限へ降格すると同じツールが丸ごと消える（= このバグの実害）。
    let (_dir2, ctx2) = test_context_with_caller(CallerIdentity::Agent);
    let demoted = BridgedExecutor::new(ActionDispatcher::new(), ctx2)
        .with_gateway_actions(Arc::new(MockGatewayActionsWithSkills));
    let demoted_names: Vec<String> = policy_visible_names(&demoted);
    assert!(!demoted_names.iter().any(|n| n == "create_skill"));
    assert!(!demoted_names.iter().any(|n| n == "update_instructions"));
}

// ---- owner_only_actions filtering ----

#[test]
fn test_list_tools_owner_sees_update_instructions() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);

    // #923: owner-only 可視性は narrowing 前の policy 層で検証する。
    let names = policy_visible_names(&executor);
    assert!(
        names.iter().any(|n| n == "update_instructions"),
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

/// `configure_llm_provider`（#118）は owner 限定。gateway が定義を出しても
/// 非 owner には可視化されず、名前指定の実行も dispatch で拒否されること。
#[tokio::test]
async fn test_configure_llm_provider_is_owner_only() {
    struct GwConfig;
    #[async_trait::async_trait]
    impl GatewayActions for GwConfig {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "configure_llm_provider".to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: "x".to_string(),
                parameters: json!({"type": "object"}),
            }]
        }
        async fn execute(
            &self,
            _n: &str,
            _a: &serde_json::Value,
            _c: &opencrab_gateway::GatewayCallContext,
        ) -> GatewayActionResult {
            GatewayActionResult {
                success: true,
                data: Some(json!({"reached_gateway": true})),
                error: None,
            }
        }
    }

    // Agent: 一覧に出ず、名前指定の実行も owner ゲートで拒否される。
    let (_d, actx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), actx)
        .with_gateway_actions(Arc::new(GwConfig));
    // #923: owner-only 可視性は narrowing 前の policy 層で検証する。
    assert!(
        !policy_visible_names(&agent_exec)
            .iter()
            .any(|n| n == "configure_llm_provider"),
        "Agent must NOT see configure_llm_provider"
    );
    let r = agent_exec
        .execute("configure_llm_provider", &json!({"provider": "acp"}))
        .await;
    assert!(!r.success, "Agent execution must be rejected");
    assert!(r.error.unwrap().to_lowercase().contains("owner"));

    // Owner: 可視化され、実行は gateway に到達する。
    let (_d2, octx) = test_context_with_caller(CallerIdentity::Owner);
    let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), octx)
        .with_gateway_actions(Arc::new(GwConfig));
    assert!(
        policy_visible_names(&owner_exec)
            .iter()
            .any(|n| n == "configure_llm_provider"),
        "Owner should see configure_llm_provider"
    );
    let r2 = owner_exec
        .execute("configure_llm_provider", &json!({"provider": "acp"}))
        .await;
    assert!(r2.success, "Owner execution should reach the gateway");
    assert_eq!(r2.data["reached_gateway"], true);
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
