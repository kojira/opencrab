use super::super::*;
use super::common::*;
use crate::traits::CallerIdentity;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult};
use serde_json::json;
use std::sync::Mutex;

/// #330 で塞ぐローカル操作系ツール（policy 表の権威 = owner_only、trusted_only ではない）。
const LOCAL_OWNER_ONLY_TOOLS: &[&str] = &[
    "execute_shell",
    "ws_read",
    "ws_list",
    "ws_write",
    "ws_delete",
    "ws_edit",
    "ws_mkdir",
    "add_allowed_command",
    "remove_allowed_command",
];

/// #330: ローカルのシェル実行 / ファイル操作 / 実行許可リストの自己拡張は owner 限定。
/// ポリシー表（`tool_policy`）の権威を直接見る。`manage_allowed_commands` と同じ
/// owner_only（trusted_only ではない）に揃っていること。
#[test]
fn local_tools_are_owner_only_in_policy_table() {
    for name in LOCAL_OWNER_ONLY_TOOLS {
        let p = tool_policy(name);
        assert!(p.owner_only, "{name} must be owner_only (#330)");
        assert!(
            !p.trusted_only,
            "{name} は owner_only であるべき（CoAgent / TrustedUser にも開けない / #330）"
        );
    }
}

/// #330: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、上記
/// ローカル操作系ツールが **3 経路すべて**（`policy_allows` / `list_tools` /
/// `dispatch_inner`）で落ちること。`nostr_run_passes_the_gate_for_agent_caller` の逆。
///
/// 対照として caller=Owner（heartbeat tick / ダッシュボード / オーナー会話）では 3 経路
/// すべてで従来どおり使える（gateway まで到達する）ことも固定する。
#[tokio::test]
async fn local_tools_are_blocked_for_agent_caller() {
    /// 対象 9 ツールを定義するだけの fake gateway（実装は別 crate / config 駆動なので）。
    struct GwLocal;
    #[async_trait::async_trait]
    impl GatewayActions for GwLocal {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            LOCAL_OWNER_ONLY_TOOLS
                .iter()
                .map(|n| GatewayActionDef {
                    name: n.to_string(),
                    class: opencrab_gateway::ToolClass {
                        dispatch: opencrab_gateway::DispatchMode::Inline,
                        sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                        sharing: opencrab_gateway::ToolSharing::AgentBound,
                    },
                    description: "x".to_string(),
                    parameters: json!({"type": "object"}),
                })
                .collect()
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

    // caller=Agent: 3 経路すべてで落ちる。
    let (_d, actx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec =
        BridgedExecutor::new(ActionDispatcher::new(), actx).with_gateway_actions(Arc::new(GwLocal));
    // #923: owner-only の可視性は narrowing 前の policy 層で検証する。
    let agent_tools: Vec<String> = policy_visible_names(&agent_exec);
    for name in LOCAL_OWNER_ONLY_TOOLS {
        // 1. ポリシー述語。
        assert!(
            !agent_exec.policy_allows(name),
            "caller=Agent が {name} を policy_allows で通してしまう（#330）"
        );
        // 2. 可視性: モデルに見えない。
        assert!(
            !agent_tools.iter().any(|t| t == name),
            "caller=Agent の list_tools に {name} が出てはいけない（#330）"
        );
        // 3. 実行時強制: 名前指定の実行が owner ゲートで拒否される（gateway へ到達しない）。
        let r = agent_exec.execute(name, &json!({})).await;
        assert!(
            !r.success,
            "caller=Agent の {name} 実行は拒否されるべき（#330）"
        );
        assert!(
            r.error.unwrap_or_default().to_lowercase().contains("owner"),
            "{name} の拒否理由は owner ゲートであるべき（#330）"
        );
    }

    // 対照: caller=Owner（heartbeat 相当）では 3 経路すべてで従来どおり使える。
    let (_d2, octx) = test_context_with_caller(CallerIdentity::Owner);
    let owner_exec =
        BridgedExecutor::new(ActionDispatcher::new(), octx).with_gateway_actions(Arc::new(GwLocal));
    // #923: owner の可視性は narrowing 前の policy 層で検証する。
    let owner_tools: Vec<String> = policy_visible_names(&owner_exec);
    for name in LOCAL_OWNER_ONLY_TOOLS {
        assert!(
            owner_exec.policy_allows(name),
            "caller=Owner は {name} を使えるべき（heartbeat / 自律活動が死ぬ / #330）"
        );
        assert!(
            owner_tools.iter().any(|t| t == name),
            "caller=Owner の可視集合に {name} が出るべき（#330）"
        );
        // 実行時強制: owner ゲートで**止まらず**先の実装（dispatcher / gateway）へ
        // 到達する。`ws_*` は `ActionDispatcher::new()` に実在するため fake gateway では
        // なく本物の dispatcher が処理する（空引数で失敗しうるが、その失敗は owner
        // ゲート由来ではない）。よって「owner 拒否文言が出ないこと」で到達を判定する。
        let r = owner_exec.execute(name, &json!({})).await;
        assert!(
            !r.error
                .as_deref()
                .unwrap_or_default()
                .contains("requires owner"),
            "caller=Owner の {name} が owner ゲートで拒否された: {:?}（#330）",
            r.error
        );
    }

    // heartbeat 相当（caller=Owner）で `execute_shell` が gateway まで到達すること
    // （dispatcher に無い config 駆動ツールなので fake gateway が処理し、往復を確認できる）。
    let r = owner_exec.execute("execute_shell", &json!({})).await;
    assert!(
        r.success,
        "caller=Owner の execute_shell 実行が拒否された: {:?}（#330）",
        r.error
    );
    assert_eq!(
        r.data["reached_gateway"], true,
        "execute_shell が gateway へ到達しない（heartbeat 経路が死ぬ / #330）"
    );

    // #485: co_agent は owner 等価。owner と同じく 3 経路すべてで LOCAL_OWNER_ONLY_TOOLS を
    // 使え、execute_shell が gateway まで到達する（オーナーの「co_agent に execute_shell /
    // ファイル操作を開放して」を満たす）。is_owner_equivalent から CoAgent を外すと落ちる。
    let (_d3, cctx) = test_context_with_caller(CallerIdentity::CoAgent {
        agent_id: "peer".to_string(),
    });
    let co_exec =
        BridgedExecutor::new(ActionDispatcher::new(), cctx).with_gateway_actions(Arc::new(GwLocal));
    // #923: co_agent（owner 等価）の可視性は narrowing 前の policy 層で検証する。
    let co_tools: Vec<String> = policy_visible_names(&co_exec);
    for name in LOCAL_OWNER_ONLY_TOOLS {
        assert!(
            co_exec.policy_allows(name),
            "caller=CoAgent は {name} を使えるべき（#485: owner 等価）"
        );
        assert!(
            co_tools.iter().any(|t| t == name),
            "caller=CoAgent の可視集合に {name} が出るべき（#485）"
        );
        let r = co_exec.execute(name, &json!({})).await;
        assert!(
            !r.error
                .as_deref()
                .unwrap_or_default()
                .contains("requires owner"),
            "caller=CoAgent の {name} が owner ゲートで拒否された: {:?}（#485）",
            r.error
        );
    }
    let r = co_exec.execute("execute_shell", &json!({})).await;
    assert!(
        r.success,
        "caller=CoAgent の execute_shell 実行が拒否された: {:?}（#485）",
        r.error
    );
    assert_eq!(
        r.data["reached_gateway"], true,
        "execute_shell が gateway へ到達しない（#485: co_agent = owner 等価）"
    );
}

/// #330/#333: 判定軸は caller だけで、depth は増えても owner の可否を変えない。
///
/// #333 で sub-engine は親ターンの caller を継承するようになった
/// （`subtask_spawn.rs` が `spawn_subtask` の sub-run に親 caller を渡す）。したがって
/// **Owner ターンから起動したサブ（caller=Owner・depth>=1）は実在する構成**で、そこで
/// `execute_shell` / `ws_*` が使える必要がある（メインで直接やるのと同じ = 委譲都合の
/// 非対称を作らない）。逆に **Agent ターンから起動したサブ（caller=Agent・depth>=1）は
/// 塞がったまま**でなければならない（`spawn_subtask` を挟んだ迂回の封鎖）。
///
/// これらは sub-engine 遮断属性（`class.sub_engine == Blocked`）を持たないので、判定は
/// caller のみ。
#[test]
fn local_tools_gated_by_caller_only_regardless_of_depth() {
    // Owner: depth 0 でも depth>=1 でも使える（実在するサブ構成 = 親 Owner → サブ Owner）。
    let (_d, octx) = test_context_with_caller(CallerIdentity::Owner);
    let owner_depth0 = BridgedExecutor::new(ActionDispatcher::new(), octx);
    let (_d2, octx2) = test_context_with_caller(CallerIdentity::Owner);
    let owner_depth1 = BridgedExecutor::new(ActionDispatcher::new(), octx2).with_depth(1);
    // Agent: どの depth でも塞がる（親 Agent → サブ Agent の迂回封鎖）。
    let (_d3, actx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_depth0 = BridgedExecutor::new(ActionDispatcher::new(), actx);
    let (_d4, actx2) = test_context_with_caller(CallerIdentity::Agent);
    let agent_depth1 = BridgedExecutor::new(ActionDispatcher::new(), actx2).with_depth(1);
    for name in LOCAL_OWNER_ONLY_TOOLS {
        assert!(
            !owner_depth1.is_blocked_in_subengine(name),
            "{name} に depth ゲートを足していないこと（#330）"
        );
        assert_eq!(
            owner_depth0.policy_allows(name),
            owner_depth1.policy_allows(name),
            "{name} の owner 可否が depth 0 と depth>=1 で食い違ってはいけない（#330）"
        );
        assert!(
            owner_depth1.policy_allows(name),
            "caller=Owner のサブ（depth>=1）で {name} が使えないと実装作業が死ぬ（#333）"
        );
        assert!(
            !agent_depth1.policy_allows(name),
            "caller=Agent のサブ（depth>=1）で {name} が通ると spawn_subtask 迂回が開く（#333）"
        );
        assert_eq!(
            agent_depth0.policy_allows(name),
            agent_depth1.policy_allows(name),
            "{name} の agent 可否が depth で変わってはいけない（#333）"
        );
    }
}

/// 設定変更系（#116）は owner 限定であること（ポリシー表の権威）。
#[test]
fn test_settings_tools_are_owner_only() {
    for name in [
        "configure_llm_provider",
        "manage_allowed_commands",
        "configure_nostr",
        "configure_self",
        "configure_mcp_server",
    ] {
        let p = tool_policy(name);
        assert!(p.owner_only, "{name} must be owner_only");
        assert!(
            !p.trusted_only,
            "{name} should be gated by owner_only, not trusted_only"
        );
    }
}

#[test]
fn test_list_tools_owner_sees_update_heartbeat_instructions() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::Owner);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
    // #923: heartbeat/owner-only の可視性は narrowing 前の policy 層で検証する。
    let names: Vec<String> = policy_visible_names(&executor);
    assert!(names.iter().any(|n| n == "update_heartbeat_instructions"));
    assert!(names.iter().any(|n| n == "read_heartbeat_instructions"));
}

#[test]
fn test_list_tools_agent_cannot_see_heartbeat_actions() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::Agent);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
    // #923: heartbeat/owner-only の可視性は narrowing 前の policy 層で検証する。
    let names: Vec<String> = policy_visible_names(&executor);
    // Agent (non-owner, non-trusted) sees neither.
    assert!(!names.iter().any(|n| n == "update_heartbeat_instructions"));
    assert!(!names.iter().any(|n| n == "read_heartbeat_instructions"));
}

#[test]
fn test_list_tools_trusted_user_heartbeat_read_only() {
    let (_dir, ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(Arc::new(MockGatewayHeartbeat));
    // #923: heartbeat/owner-only の可視性は narrowing 前の policy 層で検証する。
    let names: Vec<String> = policy_visible_names(&executor);
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
            class: opencrab_gateway::ToolClass {
                dispatch: opencrab_gateway::DispatchMode::Inline,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing: opencrab_gateway::ToolSharing::AgentBound,
            },
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

/// RFC #152 S2: gateway の `execute` に渡る ctx に、合成 gateway 自身への
/// ハンドル（`root_gateway`）が注入されること。sub-engine を構築する
/// `spawn_subtask` がこれを辿って合成 gateway を wrap できる（自己参照 Arc 不要
/// = Arc は本 executor が所有し、ctx は clone を短命に運ぶだけ）。
#[tokio::test]
async fn test_gateway_ctx_carries_root_gateway_handle() {
    let (_dir, ctx) = test_context();
    let gw = Arc::new(CtxRecordingGateway {
        last_ctx: Mutex::new(None),
        last_args: Mutex::new(None),
    });
    let executor =
        BridgedExecutor::new(ActionDispatcher::new(), ctx).with_gateway_actions(gw.clone());
    let r = executor.execute("ctx_probe", &json!({})).await;
    assert!(r.success);
    let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
    assert!(
        seen.root_gateway.is_some(),
        "root_gateway handle must be injected so a sub-engine can wrap the composite gateway"
    );
}

/// #158 S1: gateway の `execute` に渡る ctx が、この run を起こした inbound の
/// 返信先（gateway 不透明 token）を運ぶこと。宛先引数を省略したツール呼び出しの
/// フォールバック源になる。
#[tokio::test]
async fn test_gateway_ctx_carries_reply_target() {
    let (_dir, ctx) = test_context();
    let gw = Arc::new(CtxRecordingGateway {
        last_ctx: Mutex::new(None),
        last_args: Mutex::new(None),
    });
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx)
        .with_gateway_actions(gw.clone())
        .with_reply_target(Some("note1abcdef".to_string()));
    let r = executor.execute("ctx_probe", &json!({})).await;
    assert!(r.success);
    let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
    assert_eq!(seen.reply_target.as_deref(), Some("note1abcdef"));
}

/// #158 S1 非退行: 返信先を注入しない executor は ctx.reply_target が None のまま
/// （後方互換 = 宛先を明示する呼び出しの挙動は変わらない）。
#[tokio::test]
async fn test_gateway_ctx_reply_target_none_by_default() {
    let (_dir, ctx) = test_context();
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    assert!(executor.gateway_call_context(None).reply_target.is_none());
}

/// #158 S1: Nostr 経路（`RunRequest.reply_target` を載せる gateway）で、`process.rs`
/// と同じ配線を通すとツール実行の文脈に返信先が**非 None** で届くこと。Nostr は既に
/// `RunRequest` に返信先を入れている（#167）ので、この段だけで効く。
#[tokio::test]
async fn test_nostr_run_request_reply_target_reaches_gateway_ctx() {
    use crate::run_request::RunRequest;

    let (_dir, action_ctx) = test_context();
    // Nostr gateway が inbound の返信先（イベント id）を RunRequest に載せる。
    let req = RunRequest::new(
        "agent-a",
        "A",
        "nostr-agent-a-npub1sender",
        "sys",
        "conv",
        "nostr",
        CallerIdentity::Agent,
    )
    .with_reply_target("note1abcdef");

    let gw = Arc::new(CtxRecordingGateway {
        last_ctx: Mutex::new(None),
        last_args: Mutex::new(None),
    });
    // process.rs の executor 構築と同じ配線（RunRequest → BridgedExecutor）。
    let executor = BridgedExecutor::new(ActionDispatcher::new(), action_ctx)
        .with_gateway_actions(gw.clone())
        .with_reply_target(req.reply_target.clone());

    assert!(executor.execute("ctx_probe", &json!({})).await.success);
    let seen = gw.last_ctx.lock().unwrap().clone().unwrap();
    assert!(
        seen.reply_target.is_some(),
        "Nostr 経路ではツール文脈の返信先が非 None になる"
    );
    assert_eq!(seen.reply_target.as_deref(), Some("note1abcdef"));
}

/// root_gateway 未注入（gateway_actions 無し）の executor は、ctx.root_gateway が
/// None のまま（後方互換 = 非破壊）。
#[tokio::test]
async fn test_gateway_ctx_root_gateway_none_without_gateway_actions() {
    // gateway_actions を付けない executor では、そもそも gateway.execute へ
    // 到達しないため、ここでは gateway_call_context() の生成結果を直接確認する。
    let (_dir, ctx) = test_context();
    let executor = BridgedExecutor::new(ActionDispatcher::new(), ctx);
    let call_ctx = executor.gateway_call_context(None);
    assert!(
        call_ctx.root_gateway.is_none(),
        "no gateway_actions => root_gateway must stay None (backward compatible)"
    );
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
    // `create_skill`（#157 S6）と `update_heartbeat_instructions` /
    // `read_heartbeat_instructions`（#157 S3）は server 側の合成 gateway が実装する
    // （実在性は server crate のテストで検証）。execute_skill は防御的エントリ
    // （実装なし）であることをここで明文化する。
    assert!(!names.iter().any(|n| n == "execute_skill"));
}
