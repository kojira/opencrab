use super::super::*;
use super::common::*;
use crate::traits::CallerIdentity;
use opencrab_gateway::{GatewayActionDef, GatewayActionResult};
use serde_json::json;

/// #923 実行ゲート不変の回帰ガード（可視≠実行可否）: ツール階層で常時集合の外に置いた
/// owner-only ツール（`configure_llm_provider`）を、非 owner が `describe_tools` で活性化
/// しようとしても — (1) describe_tools は policy 済みの effective 定義からしか schema を
/// 引かないので unknown を返し可視化されない、(2) 仮に名前を記憶で呼んでも dispatch_inner の
/// policy が従来どおり owner ゲートで拒否する。describe_tools が policy バイパスにならない
/// ことを最外層（list_tools 可視性＋execute 実行）で pin する。
#[tokio::test]
async fn describe_tools_does_not_bypass_owner_gate_for_nonowner() {
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

    let (_d, actx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), actx)
        .with_gateway_actions(Arc::new(GwConfig));

    // (1) 非 owner が describe_tools で活性化を試みる → unknown（loaded に入らない）。
    let d = agent_exec
        .execute(
            "describe_tools",
            &json!({"names": ["configure_llm_provider"]}),
        )
        .await;
    assert!(
        d.success,
        "describe_tools 自体は成功する（合成 query ツール）"
    );
    let unknown = d.data["unknown"]
        .as_array()
        .map(|a| a.iter().any(|v| v == "configure_llm_provider"))
        .unwrap_or(false);
    assert!(
        unknown,
        "非 owner には configure_llm_provider が unknown（可視化されない）: {:?}",
        d.data
    );
    let loaded_empty = d.data["loaded"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(false);
    assert!(
        loaded_empty,
        "owner-only ツールが loaded に入ってはならない"
    );

    // (2) 活性化後も list_tools（投影）に出ない（可視化バイパスなし）。
    let names: Vec<String> = agent_exec
        .list_tools()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(
        !names.iter().any(|n| n == "configure_llm_provider"),
        "describe_tools 後も owner-only ツールが投影に出てはならない: {names:?}"
    );

    // (3) 名前を記憶で直接呼んでも dispatch_inner の policy が owner ゲートで拒否する。
    let r = agent_exec
        .execute("configure_llm_provider", &json!({"provider": "acp"}))
        .await;
    assert!(!r.success, "非 owner の owner-only 実行は拒否されるべき");
    assert!(
        r.error.unwrap_or_default().to_lowercase().contains("owner"),
        "拒否理由は owner ゲート由来であるべき"
    );
    // gateway へ到達していない。
    assert!(
        r.data.get("reached_gateway").is_none(),
        "owner ゲートを越えて gateway へ到達してはならない"
    );
}

/// #264: `nostr_list_keys` は trusted 限定（owner 限定ではない）。未信頼の会話ターン
/// （caller=Agent）には出さず実行もしないが、owner/co_agent/trusted_user のターンでは
/// 使える（heartbeat / ダッシュボード / オーナー会話は全て trusted 相当の caller）。
#[test]
fn test_nostr_list_keys_is_trusted_only() {
    let p = tool_policy("nostr_list_keys");
    assert!(p.trusted_only, "nostr_list_keys must be trusted_only");
    assert!(
        !p.owner_only,
        "nostr_list_keys should be trusted_only, not owner_only（自分の鍵一覧は自分で見る）"
    );

    // caller=Agent は可視化されない（policy 表の権威を直接見る）。
    let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
    assert!(
        !agent_exec.policy_allows("nostr_list_keys"),
        "Agent（未信頼の外部会話ターン）は nostr_list_keys を使えない"
    );
    // caller=TrustedUser は使える。
    let (_d2, trusted_ctx) = test_context_with_caller(CallerIdentity::TrustedUser);
    let trusted_exec = BridgedExecutor::new(ActionDispatcher::new(), trusted_ctx);
    assert!(
        trusted_exec.policy_allows("nostr_list_keys"),
        "TrustedUser は nostr_list_keys を使える"
    );
}

/// #264: `nostr_switch_identity`（採用＝接続）は trusted 限定。外部ユーザー由来の
/// 会話ターン（caller=Agent）には出さず実行もしない（乗っ取り防止）。owner/trusted の
/// ターン（heartbeat / ダッシュボード / オーナー会話）でだけ自分の意思で採用できる。
#[test]
fn test_nostr_switch_identity_is_trusted_only() {
    let p = tool_policy("nostr_switch_identity");
    assert!(p.trusted_only, "nostr_switch_identity must be trusted_only");

    let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
    assert!(
        !agent_exec.policy_allows("nostr_switch_identity"),
        "Agent（未信頼の外部会話ターン）は nostr_switch_identity を使えない（乗っ取り防止）"
    );
    let (_d2, owner_ctx) = test_context_with_caller(CallerIdentity::Owner);
    let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), owner_ctx);
    assert!(
        owner_exec.policy_allows("nostr_switch_identity"),
        "Owner は nostr_switch_identity を使える"
    );
}

/// #303: `nostr_run` は caller=Agent のターンで**実際にゲートを通る**。
///
/// caller=Agent が指すのは **Nostr 受信ターン**（`crates/nostr/src/sink.rs`）と
/// 非オーナー相手の会話ターン。ここで塞がると「Nostr 上で自律的に活動する」という
/// 目的そのものが成立しない。
///
/// リスト（`TRUSTED_ONLY_ACTIONS` に無いこと）だけでは、**別の場所に新しいゲートが
/// 足された**場合を捕まえられない。そこで `policy_allows` / `list_tools` /
/// `dispatch_inner`（= `execute`）の 3 経路を実際に通す。
#[tokio::test]
async fn nostr_run_passes_the_gate_for_agent_caller() {
    /// `nostr_run` を定義するだけの fake gateway（server 側の実装は別 crate なので）。
    struct GwNostrRun;
    #[async_trait::async_trait]
    impl GatewayActions for GwNostrRun {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            vec![GatewayActionDef {
                name: "nostr_run".to_string(),
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

    let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx)
        .with_gateway_actions(Arc::new(GwNostrRun));

    // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
    assert!(
        agent_exec.policy_allows("nostr_run"),
        "caller=Agent（Nostr 受信ターン）で nostr_run が policy_allows を通らない \
         — どこかに caller ゲートが足された"
    );
    // 2. 可視性: モデルに見えていること（#923: 会話 op 以外の可視性は policy 層で）。
    assert!(
        policy_visible_names(&agent_exec)
            .iter()
            .any(|n| n == "nostr_run"),
        "caller=Agent の可視集合に nostr_run が出ない"
    );
    // 3. 実行時強制: 名前指定の実行が gateway まで到達すること。
    let r = agent_exec
        .execute("nostr_run", &json!({"subcommand": "post"}))
        .await;
    assert!(
        r.success,
        "caller=Agent の nostr_run 実行が拒否された: {:?}",
        r.error
    );
    assert_eq!(r.data["reached_gateway"], true);

    // 対照: owner ターンでも当然通る（Agent 側だけ通す非対称にしていない）。
    let (_d2, owner_ctx) = test_context_with_caller(CallerIdentity::Owner);
    let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), owner_ctx);
    assert!(
        owner_exec.policy_allows("nostr_run"),
        "Owner ターンでも nostr_run は通る"
    );
}

/// #319: Nostr 受信ターンの呼び出し元が `Owner` なら、設定変更系（OWNER_ONLY 7 個）
/// と自分の設定を触る TRUSTED_ONLY が**実際に通る**。
///
/// 発言者からの解決（`NostrAgentRunner::resolve_nostr_caller`）で `Owner` に
/// なったターンが、ポリシー層でどう扱われるかをここに固定する。以前は Nostr の
/// 呼び出し元が `Agent` 固定だったため、この一覧が丸ごと消えていた（issue 本文の表）。
#[test]
fn test_owner_caller_unlocks_the_tools_missing_from_nostr_turns() {
    // issue #319 で「消えている」と記録されたツール。
    const OWNER_ONLY_FROM_ISSUE: [&str; 7] = [
        "configure_self",
        "configure_nostr",
        "configure_llm_provider",
        "configure_mcp_server",
        "update_instructions",
        "update_heartbeat_instructions",
        "manage_allowed_commands",
    ];
    const TRUSTED_ONLY_FROM_ISSUE: [&str; 4] = [
        "set_my_heartbeat",
        "get_my_heartbeat",
        "nostr_list_keys",
        "nostr_switch_identity",
    ];

    let (_d, owner_ctx) = test_context_with_caller(CallerIdentity::Owner);
    let owner_exec = BridgedExecutor::new(ActionDispatcher::new(), owner_ctx);
    let (_d2, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);

    for name in OWNER_ONLY_FROM_ISSUE.iter().chain(&TRUSTED_ONLY_FROM_ISSUE) {
        assert!(
            owner_exec.policy_allows(name),
            "オーナー発のターンで {name} が通らない"
        );
        assert!(
            !agent_exec.policy_allows(name),
            "他人発のターン（最小権限）で {name} が通ってしまう"
        );
    }
}

/// #306: `nostr_zap` は caller=Agent のターンで**実際にゲートを通る**。
///
/// 以前は `nostr_dm` / `nostr_zap` が `TRUSTED_ONLY_ACTIONS` に入っていたが、`nostr_run`
/// を開けた（#303）時点で `nostr_run dm` / `nostr_run zap` が同じターンから通るため、
/// inner ツール名を隠すだけのゲートになっていた。一貫性を**制約を減らす方向**で取ると
/// いうオーナーの決定（#306）に従い外した。ここはその決定を実測で固定する。
///
/// **#514 で `nostr_dm` は撤去した**（DM 送信禁止・定義から削除＋`nostr_run dm` も deny）
/// ので、#306 の対象から外れ、ここでの検証は `nostr_zap` に絞る。DM のブロックは bridge の
/// caller ゲート層ではなく定義層と passthrough 層で行う（`crates/nostr`）。
///
/// `nostr_run` 側（`nostr_run_passes_the_gate_for_agent_caller`）と同じく、リストに
/// 無いことだけを見ても**別の場所に新しいゲートが足された**場合を捕まえられないので、
/// `policy_allows` / `list_tools` / `dispatch_inner`（= `execute`）の 3 経路を通す。
#[tokio::test]
async fn nostr_messaging_passes_the_gate_for_agent_caller() {
    /// `nostr_zap` を定義するだけの fake gateway
    /// （本体は `crates/nostr` にあり、この crate からは参照できない）。
    struct GwNostrMessaging;
    #[async_trait::async_trait]
    impl GatewayActions for GwNostrMessaging {
        fn definitions(&self) -> Vec<GatewayActionDef> {
            ["nostr_zap"]
                .into_iter()
                .map(|name| GatewayActionDef {
                    name: name.to_string(),
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

    let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx)
        .with_gateway_actions(Arc::new(GwNostrMessaging));
    // #923: 会話 op 以外の可視性は narrowing 前の policy 層で検証する。
    let listed: Vec<String> = policy_visible_names(&agent_exec);

    for name in ["nostr_zap"] {
        // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
        assert!(
            agent_exec.policy_allows(name),
            "caller=Agent（Nostr 受信ターン）で {name} が policy_allows を通らない \
             — TRUSTED_ONLY_ACTIONS へ戻されたか、別の場所に caller ゲートが足された"
        );
        // 2. 可視性: モデルに見えていること。
        assert!(
            listed.iter().any(|n| n == name),
            "caller=Agent の list_tools に {name} が出ない"
        );
        // 3. 実行時強制: 名前指定の実行が gateway まで到達すること。
        let r = agent_exec.execute(name, &json!({})).await;
        assert!(
            r.success,
            "caller=Agent の {name} 実行が拒否された: {:?}",
            r.error
        );
        assert_eq!(r.data["reached_gateway"], true);
    }

    // 対照: 他ツールの trusted ゲートは維持されている（一律に開けたのではない）。
    for name in ["create_skill", "nostr_switch_identity", "nostr_list_keys"] {
        assert!(
            !agent_exec.policy_allows(name),
            "{name} の trusted ゲートは維持されるべき（#306 は nostr_zap のみ・nostr_dm は #514 で撤去）"
        );
    }
}

/// #351 で trusted ゲートへ載せるスキル生成 / 自律学習系（core dispatcher アクション）。
const SKILL_LEARNING_TRUSTED_ONLY: &[&str] = &[
    "create_my_skill",
    "learn_from_experience",
    "learn_from_peer",
    "reflect_and_learn",
];

/// #351: スキル生成（`create_my_skill`）と自律学習（`learn_from_experience` /
/// `learn_from_peer` / `reflect_and_learn`）は trusted_only（owner_only ではない）。
/// gateway 版 `create_skill` と同じ棚に揃っていることをポリシー表の権威で固定する。
#[test]
fn skill_and_learning_actions_are_trusted_only_in_policy_table() {
    for name in SKILL_LEARNING_TRUSTED_ONLY {
        let p = tool_policy(name);
        assert!(p.trusted_only, "{name} must be trusted_only (#351)");
        assert!(
            !p.owner_only,
            "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
             TrustedUser も塞がれる / #351）"
        );
    }
}

/// #351: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、スキル
/// 生成 / 自律学習系が **3 経路すべて**（`policy_allows` / `list_tools` /
/// `dispatch_inner`）で落ちること。オーナー要望「スキルを作るのもなし」を実測で固定
/// する。名前がリストから消えただけでは、別の場所に caller ゲートが足された場合を
/// 捕まえられないので `nostr_run_passes_the_gate_for_agent_caller` と同じ 3 経路を通す。
///
/// 対照: caller=Owner / CoAgent / TrustedUser（heartbeat tick / ダッシュボード /
/// オーナー会話 / 信頼済みユーザー会話）では従来どおり 3 経路すべてで通る。
#[tokio::test]
async fn skill_and_learning_actions_gated_from_agent_caller() {
    // caller=Agent: 3 経路すべてで落ちる。
    let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
    // #923: agent gating の可視性は narrowing 前の policy 層で検証する。
    let agent_listed: Vec<String> = policy_visible_names(&agent_exec);

    for name in SKILL_LEARNING_TRUSTED_ONLY {
        // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
        assert!(
            !agent_exec.policy_allows(name),
            "caller=Agent が {name} を policy_allows で通してしまう（#351）"
        );
        // 2. 可視性: モデルに見えていないこと。
        assert!(
            !agent_listed.iter().any(|n| n == name),
            "caller=Agent の list_tools に {name} が出てしまう（#351）"
        );
        // 3. 実行時強制: 名前指定の実行が拒否されること（記憶で名前を呼んでも素通し
        //    しない）。
        let r = agent_exec.execute(name, &json!({})).await;
        assert!(
            !r.success,
            "caller=Agent の {name} 実行が拒否されない（#351）"
        );
    }

    // 対照: Owner / CoAgent / TrustedUser では 3 経路すべてで通る。
    for caller in [
        CallerIdentity::Owner,
        CallerIdentity::CoAgent {
            agent_id: "peer".to_string(),
        },
        CallerIdentity::TrustedUser,
    ] {
        let (_d, ctx) = test_context_with_caller(caller.clone());
        let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        // #923: owner/trusted 可視性は narrowing 前の policy 層で検証する。
        let listed: Vec<String> = policy_visible_names(&exec);
        for name in SKILL_LEARNING_TRUSTED_ONLY {
            assert!(
                exec.policy_allows(name),
                "caller={caller:?} で {name} が policy_allows を通らない（#351 は \
                 trusted を塞がない）"
            );
            assert!(
                listed.iter().any(|n| n == name),
                "caller={caller:?} の list_tools に {name} が出ない（#351）"
            );
        }
    }
}

/// #356 で trusted ゲートへ載せる、caller=Agent に素通しだった 9 個。
const PASSTHROUGH_9_TRUSTED_ONLY: &[&str] = &[
    "set_default_webhook",
    "set_default_subtask_webhook",
    "get_default_webhook",
    "get_default_subtask_webhook",
    "list_webhooks",
    "list_subtask_webhooks",
    "update_memory_index_config",
    "get_system_info",
    "list_allowed_commands",
];

/// #356 の 9 個のうち、`SystemGatewayActions`（server 側 own ツール）で定義される 8 個。
/// 残る 1 個 `get_system_info` は core dispatcher（`ActionDispatcher::new`）側。
const PASSTHROUGH_9_SERVER_SLOT: &[&str] = &[
    "set_default_webhook",
    "set_default_subtask_webhook",
    "get_default_webhook",
    "get_default_subtask_webhook",
    "list_webhooks",
    "list_subtask_webhooks",
    "update_memory_index_config",
    "list_allowed_commands",
];

/// #356: 素通しだった 9 個はいずれも trusted_only（owner_only ではない）。owner_only に
/// すると CoAgent / TrustedUser も塞がれてしまう（オーナー決定は「9 個すべて
/// trusted_only」）。ポリシー表の権威で固定する。
#[test]
fn passthrough_actions_are_trusted_only_in_policy_table() {
    assert_eq!(
        PASSTHROUGH_9_TRUSTED_ONLY.len(),
        9,
        "#356 の対象は 9 個（棚卸しで見つかった素通し分）"
    );
    for name in PASSTHROUGH_9_TRUSTED_ONLY {
        let p = tool_policy(name);
        assert!(p.trusted_only, "{name} must be trusted_only (#356)");
        assert!(
            !p.owner_only,
            "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
             TrustedUser も塞がれる / #356）"
        );
    }
}

/// #356: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、素通し
/// だった 9 個が **3 経路すべて**（`policy_allows` / `list_tools` / `dispatch_inner`）で
/// 落ちること。名前がリストから消えただけでは別の場所に caller ゲートが足された場合を
/// 捕まえられないので `skill_and_learning_actions_gated_from_agent_caller`（#351）と同じ
/// 3 経路を通す。
///
/// 実行時強制の確認は「拒否された（`!success`）」だけでなく **policy 由来の拒否である
/// こと**（`is_rejection` = REJECTION_CODE_PREFIX 付き）まで見る。8 個は
/// `SystemGatewayActions` 由来で、`BridgedExecutor::new` は gateway を注入しないため、
/// もし policy が拒否しなければ「Unknown action」で `!success` になってしまい、ゲートの
/// 有無を区別できない。policy 拒否まで確認して初めて「trusted ゲートが効いている」と
/// 言える（dispatch_inner の policy 判定はルーティングより前 / #45）。
///
/// 対照: caller=Owner / CoAgent / TrustedUser（heartbeat tick / ダッシュボード /
/// オーナー会話 / 信頼済みユーザー会話）では従来どおり通る（`policy_allows` true、かつ
/// gateway を注入した list_tools に出る）。
#[tokio::test]
async fn passthrough_actions_gated_from_agent_caller() {
    // gateway 源（8 個の server ツール）を注入した executor で list_tools を実測する。
    // get_system_info は dispatcher 側にあるので gateway には入れない。
    let build_exec = |caller: CallerIdentity| {
        let (dir, ctx) = test_context_with_caller(caller);
        let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx)
            .with_gateway_actions(Arc::new(MockGatewayServerSlot8));
        (dir, exec)
    };

    // caller=Agent: 3 経路すべてで落ちる。
    let (_d, agent_exec) = build_exec(CallerIdentity::Agent);
    // #923: agent gating の可視性は narrowing 前の policy 層で検証する。
    let agent_listed: Vec<String> = policy_visible_names(&agent_exec);
    for name in PASSTHROUGH_9_TRUSTED_ONLY {
        // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
        assert!(
            !agent_exec.policy_allows(name),
            "caller=Agent が {name} を policy_allows で通してしまう（#356）"
        );
        // 2. 可視性: モデルに見えていないこと（gateway を注入しても policy で除外される）。
        assert!(
            !agent_listed.iter().any(|n| n == name),
            "caller=Agent の list_tools に {name} が出てしまう（#356）"
        );
        // 3. 実行時強制: 名前指定の実行が policy で拒否されること（記憶で名前を呼んでも
        //    素通ししない）。Unknown action ではなく policy 拒否であることまで見る。
        let r = agent_exec.execute(name, &json!({})).await;
        assert!(
            !r.success,
            "caller=Agent の {name} 実行が拒否されない（#356）"
        );
        assert!(
            is_rejection(r.error.as_deref()),
            "caller=Agent の {name} 実行が policy 拒否になっていない \
             （error={:?} / #356）",
            r.error
        );
    }

    // 対照: Owner / CoAgent / TrustedUser では policy_allows を通り list_tools に出る。
    for caller in [
        CallerIdentity::Owner,
        CallerIdentity::CoAgent {
            agent_id: "peer".to_string(),
        },
        CallerIdentity::TrustedUser,
    ] {
        let (_d, exec) = build_exec(caller.clone());
        // #923: owner/trusted 可視性は narrowing 前の policy 層で検証する。
        let listed: Vec<String> = policy_visible_names(&exec);
        for name in PASSTHROUGH_9_TRUSTED_ONLY {
            assert!(
                exec.policy_allows(name),
                "caller={caller:?} で {name} が policy_allows を通らない（#356 は \
                 trusted を塞がない）"
            );
            assert!(
                listed.iter().any(|n| n == name),
                "caller={caller:?} の list_tools に {name} が出ない（#356）"
            );
        }
    }
}

/// #359 で trusted ゲートへ載せるタグ操作 3 個（core inline dispatcher アクション）。
const TAG_ACTIONS_TRUSTED_ONLY: &[&str] = &["tag_topic", "untag_topic", "merge_tags"];

/// #359: タグ操作 3 個は trusted_only（owner_only ではない）。owner_only にすると
/// CoAgent / TrustedUser も塞がれてしまう（オーナー決定は「TRUSTED_ONLY / OWNER_ONLY
/// ではない」）。ポリシー表の権威で固定する。
#[test]
fn tag_actions_are_trusted_only_in_policy_table() {
    for name in TAG_ACTIONS_TRUSTED_ONLY {
        let p = tool_policy(name);
        assert!(p.trusted_only, "{name} must be trusted_only (#359)");
        assert!(
            !p.owner_only,
            "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
             TrustedUser も塞がれる / #359）"
        );
    }
}

/// #359: caller=Agent（Nostr 受信ターン / 非オーナー相手の会話ターン）からは、タグ操作
/// 3 個が **3 経路すべて**（`policy_allows` / `list_tools` / `dispatch_inner`）で落ちる
/// こと。名前がリストから消えただけでは別の場所に caller ゲートが足された場合を捕まえ
/// られないので `passthrough_actions_gated_from_agent_caller`（#356）と同じ 3 経路を通す。
///
/// これらは core dispatcher に**実在**するアクション（`ActionDispatcher::new` に登録済み）
/// なので、もし policy が拒否しなければ execute が実際に走ってしまう（引数不足で
/// `!success` にはなるが policy 拒否ではない）。よって実行時強制は「拒否された
/// （`!success`）」だけでなく **policy 由来の拒否であること**（`is_rejection` =
/// REJECTION_CODE_PREFIX 付き）まで見て、ゲートが効いていることを区別する（#357 に倣う）。
///
/// 対照: caller=Owner / CoAgent / TrustedUser（heartbeat tick / ダッシュボード /
/// オーナー会話 / 信頼済みユーザー会話）では従来どおり 3 経路すべてで通る。
#[tokio::test]
async fn tag_actions_gated_from_agent_caller() {
    // caller=Agent: 3 経路すべてで落ちる。
    let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
    // #923: agent gating の可視性は narrowing 前の policy 層で検証する。
    let agent_listed: Vec<String> = policy_visible_names(&agent_exec);

    for name in TAG_ACTIONS_TRUSTED_ONLY {
        // 1. ポリシー述語（list_tools と dispatch_inner が共有する単一の判定）。
        assert!(
            !agent_exec.policy_allows(name),
            "caller=Agent が {name} を policy_allows で通してしまう（#359）"
        );
        // 2. 可視性: モデルに見えていないこと。
        assert!(
            !agent_listed.iter().any(|n| n == name),
            "caller=Agent の list_tools に {name} が出てしまう（#359）"
        );
        // 3. 実行時強制: 名前指定の実行が policy で拒否されること（記憶で名前を呼んでも
        //    素通ししない）。実在アクションなので「引数不足エラー」ではなく policy 拒否で
        //    あることまで見る。
        let r = agent_exec.execute(name, &json!({})).await;
        assert!(
            !r.success,
            "caller=Agent の {name} 実行が拒否されない（#359）"
        );
        assert!(
            is_rejection(r.error.as_deref()),
            "caller=Agent の {name} 実行が policy 拒否になっていない（error={:?} / #359）",
            r.error
        );
    }

    // 対照: Owner / CoAgent / TrustedUser では policy_allows を通り list_tools に出る。
    for caller in [
        CallerIdentity::Owner,
        CallerIdentity::CoAgent {
            agent_id: "peer".to_string(),
        },
        CallerIdentity::TrustedUser,
    ] {
        let (_d, ctx) = test_context_with_caller(caller.clone());
        let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        // #923: owner/trusted 可視性は narrowing 前の policy 層で検証する。
        let listed: Vec<String> = policy_visible_names(&exec);
        for name in TAG_ACTIONS_TRUSTED_ONLY {
            assert!(
                exec.policy_allows(name),
                "caller={caller:?} で {name} が policy_allows を通らない（#359 は \
                 trusted を塞がない）"
            );
            assert!(
                listed.iter().any(|n| n == name),
                "caller={caller:?} の list_tools に {name} が出ない（#359）"
            );
        }
    }
}

/// #379: 記憶の単位（宣言）道具は trusted_only（owner_only ではない）。
/// #394 で窓を決める `plan_next_memory_window` を同じ扱いで足した。
const MEMORY_UNIT_ACTIONS_TRUSTED_ONLY: &[&str] = &[
    "survey_my_history",
    "read_my_history",
    "record_memory_unit",
    "retract_memory_unit",
    "plan_next_memory_window",
];

#[test]
fn memory_unit_actions_are_trusted_only_in_policy_table() {
    for name in MEMORY_UNIT_ACTIONS_TRUSTED_ONLY {
        let p = tool_policy(name);
        assert!(p.trusted_only, "{name} must be trusted_only (#379)");
        assert!(
            !p.owner_only,
            "{name} は trusted_only であるべき（owner_only にすると CoAgent / \
             TrustedUser も塞がれる / #379）"
        );
    }
}

/// #379: caller=Agent からは記憶の単位道具 4 個が **3 経路すべて**（`policy_allows` /
/// `list_tools` / `dispatch_inner`）で落ちる。対照で Owner / CoAgent / TrustedUser は通る。
/// タグ道具の `tag_actions_gated_from_agent_caller`（#359）と同型。
#[tokio::test]
async fn memory_unit_actions_gated_from_agent_caller() {
    let (_d, agent_ctx) = test_context_with_caller(CallerIdentity::Agent);
    let agent_exec = BridgedExecutor::new(ActionDispatcher::new(), agent_ctx);
    // #923: agent gating の可視性は narrowing 前の policy 層で検証する。
    let agent_listed: Vec<String> = policy_visible_names(&agent_exec);

    for name in MEMORY_UNIT_ACTIONS_TRUSTED_ONLY {
        // 1. ポリシー述語（list_tools と dispatch_inner が共有）。
        assert!(
            !agent_exec.policy_allows(name),
            "caller=Agent が {name} を policy_allows で通してしまう（#379）"
        );
        // 2. 可視性: モデルに見えていない。
        assert!(
            !agent_listed.iter().any(|n| n == name),
            "caller=Agent の list_tools に {name} が出てしまう（#379）"
        );
        // 3. 実行時強制: 名前指定の実行が policy 拒否になる（実在アクションなので
        //    「引数不足」ではなく policy 拒否であることまで見る）。
        let r = agent_exec.execute(name, &json!({})).await;
        assert!(
            !r.success,
            "caller=Agent の {name} 実行が拒否されない（#379）"
        );
        assert!(
            is_rejection(r.error.as_deref()),
            "caller=Agent の {name} 実行が policy 拒否になっていない（error={:?} / #379）",
            r.error
        );
    }

    for caller in [
        CallerIdentity::Owner,
        CallerIdentity::CoAgent {
            agent_id: "peer".to_string(),
        },
        CallerIdentity::TrustedUser,
    ] {
        let (_d, ctx) = test_context_with_caller(caller.clone());
        let exec = BridgedExecutor::new(ActionDispatcher::new(), ctx);
        // #923: owner/trusted 可視性は narrowing 前の policy 層で検証する。
        let listed: Vec<String> = policy_visible_names(&exec);
        for name in MEMORY_UNIT_ACTIONS_TRUSTED_ONLY {
            assert!(
                exec.policy_allows(name),
                "caller={caller:?} で {name} が policy_allows を通らない（#379）"
            );
            assert!(
                listed.iter().any(|n| n == name),
                "caller={caller:?} の list_tools に {name} が出ない（#379）"
            );
        }
    }
}

/// #356: server 側 own ツール（webhook 6 個 + `update_memory_index_config` +
/// `list_allowed_commands`）を露出するモック。これらは本番では `SystemGatewayActions`
/// が定義するが、`BridgedExecutor::new` は `gateway_actions: None` なので、list_tools の
/// 可視性フィルタ（`policy_allows` による gateway merge の絞り込み）を実測するには
/// gateway 源を注入する必要がある。`get_system_info` は core dispatcher 側にあるので
/// ここには入れない（二重登録を避ける）。
struct MockGatewayServerSlot8;

#[async_trait]
impl GatewayActions for MockGatewayServerSlot8 {
    fn definitions(&self) -> Vec<GatewayActionDef> {
        PASSTHROUGH_9_SERVER_SLOT
            .iter()
            .map(|name| GatewayActionDef {
                name: name.to_string(),
                class: opencrab_gateway::ToolClass {
                    dispatch: opencrab_gateway::DispatchMode::Inline,
                    sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                    sharing: opencrab_gateway::ToolSharing::AgentBound,
                },
                description: format!("server slot {name}"),
                parameters: json!({"type": "object", "properties": {}}),
            })
            .collect()
    }

    async fn execute(
        &self,
        name: &str,
        _args: &serde_json::Value,
        _ctx: &opencrab_gateway::GatewayCallContext,
    ) -> GatewayActionResult {
        // caller ゲートを通過した owner/trusted のときだけここへ来る（本番では実処理）。
        GatewayActionResult {
            success: true,
            data: Some(json!({"ok": name})),
            error: None,
        }
    }
}
