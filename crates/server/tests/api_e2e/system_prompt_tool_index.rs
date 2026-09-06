// ==================== #923 静的 index（案B）受入: system prompt 境界 ====================
//
// 観測境界 = **LLM に渡る system prompt**（`run_agent_response` が組んだ最終 prompt を
// MockLlmProvider が capture する）。render 関数は呼ばない。案B は index を effective − 投影 から
// 導出し run_agent_response で後付けするので、ここで capture した prompt に現れる。
// system_prompt は literal を RunRequest へ渡す（build_agent_context を経由しない）ため、
// 実装前（5101f885）は index が一切後付けされず、nostr/設定カテゴリの assert が **赤**。

/// index 検証用 gateway: Nostr の write op（follow/unfollow/kind0/upload）と owner-only 設定
/// （configure_llm_provider）を任意で供給。会話 op reply（発話クラス）は常時集合＝投影される。
struct IdxProbeGateway {
    nostr_ops: bool,
    config_op: bool,
}

#[async_trait::async_trait]
impl opencrab_gateway::GatewayActions for IdxProbeGateway {
    fn definitions(&self) -> Vec<opencrab_gateway::GatewayActionDef> {
        let mk = |name: &str, dispatch, sharing| opencrab_gateway::GatewayActionDef {
            name: name.to_string(),
            description: format!("{name} op"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            class: opencrab_gateway::ToolClass {
                dispatch,
                sub_engine: opencrab_gateway::SubEngineAccess::NotExposed,
                sharing,
            },
        };
        let mut v = vec![mk(
            "reply",
            opencrab_gateway::DispatchMode::Utterance,
            opencrab_gateway::ToolSharing::ConversationBound,
        )];
        if self.nostr_ops {
            for n in ["follow", "unfollow", "kind0", "upload"] {
                v.push(mk(
                    n,
                    opencrab_gateway::DispatchMode::Inline,
                    opencrab_gateway::ToolSharing::AgentBound,
                ));
            }
        }
        if self.config_op {
            v.push(mk(
                "configure_llm_provider",
                opencrab_gateway::DispatchMode::Inline,
                opencrab_gateway::ToolSharing::AgentBound,
            ));
        }
        v
    }
    async fn execute(
        &self,
        name: &str,
        _a: &serde_json::Value,
        _c: &opencrab_gateway::GatewayCallContext,
    ) -> opencrab_gateway::GatewayActionResult {
        opencrab_gateway::GatewayActionResult {
            success: true,
            data: Some(serde_json::json!({"ok": name})),
            error: None,
        }
    }
}

/// caller・gateway でターンを 1 回回し、LLM に渡った system prompt（最初の呼び出し）を返す。
async fn captured_system_prompt(
    caller: opencrab_actions::CallerIdentity,
    gateway: IdxProbeGateway,
) -> String {
    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "IdxBot", "P").await;
    let session_id = format!("web-{agent_id}-idx");
    mock.push_text_response("done");
    let req = opencrab_actions::RunRequest::new(
        &agent_id,
        "IdxBot",
        &session_id,
        "SYSTEM_BASE_PROMPT",
        "user: hi",
        "nostr",
        caller,
    )
    .with_gateway_actions(std::sync::Arc::new(gateway));
    opencrab_server::process::run_agent_response(&state, req)
        .await
        .expect("run_agent_response failed");
    mock.system_prompts().first().cloned().unwrap_or_default()
}

/// Nostr レーン（Nostr の write op あり）: index に nostr カテゴリが出る。実装前は index 自体が
/// 後付けされないため **赤**。
#[tokio::test]
async fn nostr_turn_system_prompt_index_has_nostr_category() {
    let sp = captured_system_prompt(
        opencrab_actions::CallerIdentity::Agent,
        IdxProbeGateway {
            nostr_ops: true,
            config_op: false,
        },
    )
    .await;
    assert!(
        sp.contains("## More tools"),
        "index 節が system prompt に無い:\n{sp}"
    );
    assert!(
        sp.contains("- nostr:") && sp.contains("kind0"),
        "Nostr ターンの system prompt に nostr カテゴリ（follow/unfollow/kind0/upload）が無い:\n{sp}"
    );
}

/// Discord レーン（Nostr の write op 無し）: nostr カテゴリは出ない（負のコントロール）。
#[tokio::test]
async fn discord_turn_system_prompt_index_has_no_nostr_category() {
    let sp = captured_system_prompt(
        opencrab_actions::CallerIdentity::Agent,
        IdxProbeGateway {
            nostr_ops: false,
            config_op: false,
        },
    )
    .await;
    assert_eq!(
        sp.matches("- nostr:").count(),
        0,
        "Discord ターンの system prompt に nostr カテゴリが出た:\n{sp}"
    );
    assert_eq!(sp.matches("kind0").count(), 0, "{sp}");
}

/// owner ターン: owner-only 設定 op が index に出る。実装前は **赤**。
#[tokio::test]
async fn owner_turn_system_prompt_index_has_owner_only_setting() {
    let sp = captured_system_prompt(
        opencrab_actions::CallerIdentity::Owner,
        IdxProbeGateway {
            nostr_ops: false,
            config_op: true,
        },
    )
    .await;
    assert!(
        sp.contains("configure_llm_provider"),
        "owner ターンの system prompt index に configure_llm_provider が無い:\n{sp}"
    );
}

/// 非 owner ターン: owner-only 設定 op（configure_llm_provider）は policy で effective に出ない
/// ので index にも出ない（op 単位の owner ゲート・負のコントロール）。
#[tokio::test]
async fn nonowner_turn_system_prompt_index_omits_owner_only_setting() {
    let sp = captured_system_prompt(
        opencrab_actions::CallerIdentity::Agent,
        IdxProbeGateway {
            nostr_ops: false,
            config_op: true,
        },
    )
    .await;
    assert_eq!(
        sp.matches("configure_llm_provider").count(),
        0,
        "configure_llm_provider（owner-only）が非 owner の system prompt index に漏れた:\n{sp}"
    );
}
