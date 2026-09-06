use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

// ================================================================================
// #157 S3: ハートビート指示ツールの移植テスト
//
// 旧 Discord 実装（`crates/discord` の `heartbeat_instructions.rs`）にあった 4 テストを
// そのまま持ってきたもの（1 件も落としていない）＋ 移設の本題（非 Discord 構成でも
// 定義に現れる）とレスポンス JSON / エラー文言のリテラル固定。
// ================================================================================

/// エージェント行を用意する（`scope="agent"` の patch 対象）。
fn insert_agent(state: &AppState, heartbeat_instructions: &str) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: "agent-x".to_string(),
            name: "N".to_string(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "P".to_string(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: heartbeat_instructions.to_string(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

fn audit_rows(state: &AppState) -> Vec<opencrab_db::queries::HeartbeatInstructionsAuditRow> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_heartbeat_instructions_audit(&conn, "agent-x", 10).unwrap()
}

/// **#157 S3 の本題**: 2 ツールが own 定義（= transport の有無に依存せず全ターンで
/// 露出する）。own から消えると Discord 専用に逆戻りする。
#[test]
fn heartbeat_instruction_tools_are_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    for name in [
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ] {
        assert_eq!(
            defs.iter().filter(|d| d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要（#157 S3）"
        );
    }
    let update = defs
        .iter()
        .find(|d| d.name == "update_heartbeat_instructions")
        .unwrap();
    let required = update.parameters["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "scope"));
    assert!(required.iter().any(|v| v == "instructions"));
    let props = update.parameters["properties"].as_object().unwrap();
    for key in ["scope", "channel_id", "guild_id", "instructions", "reason"] {
        assert!(props.contains_key(key), "missing property: {key}");
    }
    let read = defs
        .iter()
        .find(|d| d.name == "read_heartbeat_instructions")
        .unwrap();
    assert!(read.parameters["required"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "scope"));
}

/// **Discord 無効の構成でも定義に現れる**（#157 の本題）。inner=None は
/// web / Nostr / REST / heartbeat 経路そのもの。
#[test]
fn heartbeat_instruction_tools_are_visible_without_discord() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    assert!(names.contains(&"update_heartbeat_instructions".to_string()));
    assert!(names.contains(&"read_heartbeat_instructions".to_string()));
    // 停止も同様（#157 S2）。
    assert!(names.contains(&"cancel_subtask".to_string()));
}

/// owner 以外は拒否し、監査ログも残さない（旧 Discord テストの移植）。
#[tokio::test]
async fn update_heartbeat_instructions_rejected_for_non_owner() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent", "instructions": "話題があるときだけ話す"}),
            &trusted_ctx(),
        )
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションはオーナーのみ実行できます")
    );
    assert!(audit_rows(&state).is_empty(), "監査ログを残してはならない");
}

/// owner は成功し、DB へ反映され、監査ログに old/new/reason が残る（旧テストの移植）。
/// レスポンス JSON もリテラルで固定する。
#[tokio::test]
async fn update_heartbeat_instructions_owner_success_and_audit() {
    let state = crate::test_app_state();
    insert_agent(&state, "OLD");
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({
                "scope": "agent",
                "instructions": "NEW指示",
                "reason": "オーナー依頼",
            }),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "success": true,
            "scope": "agent",
            "channel_id": Value::Null,
            "length": 5,
            "preview": "NEW指示",
        })
    );

    {
        let conn = state.db.lock().unwrap();
        let got = opencrab_db::queries::get_agent(&conn, "agent-x")
            .unwrap()
            .unwrap();
        assert_eq!(got.heartbeat_instructions, "NEW指示");
    }
    let rows = audit_rows(&state);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].scope, "agent");
    assert_eq!(rows[0].old_value.as_deref(), Some("OLD"));
    assert_eq!(rows[0].new_value.as_deref(), Some("NEW指示"));
    assert_eq!(rows[0].reason.as_deref(), Some("オーナー依頼"));
    assert_eq!(rows[0].caller_identity, GatewayCaller::Owner.label());
}

/// エージェント行が無ければ移設前と同じ文言で失敗する。
#[tokio::test]
async fn update_heartbeat_instructions_missing_agent_and_bad_args() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(r.error.as_deref(), Some("エージェントが見つかりません"));

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(r.error.as_deref(), Some("instructionsパラメータが必要です"));

    let too_long = "あ".repeat(opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN + 1);
    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "agent", "instructions": too_long}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some(
            format!(
                "instructionsが長すぎます（最大{}文字）",
                opencrab_db::queries::MAX_HEARTBEAT_INSTRUCTIONS_LEN
            )
            .as_str()
        )
    );

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "channel", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("scope=channelのときはchannel_idが必要です")
    );

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "channel", "channel_id": "ch1", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("新規チャンネル設定の作成にはguild_idが必要です")
    );

    let r = actions
        .execute(
            "update_heartbeat_instructions",
            &json!({"scope": "nope", "instructions": "x"}),
            &owner_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("不明なscope: nope（agent または channel）")
    );
}

/// `scope="effective"` が解決結果（source + instructions）を返す（旧テストの移植）。
#[tokio::test]
async fn read_heartbeat_instructions_effective() {
    let state = crate::test_app_state();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_channel_config(
            &conn,
            &opencrab_db::queries::ChannelConfigRow {
                channel_id: "ch1".to_string(),
                agent_id: "agent-x".to_string(),
                guild_id: "g1".to_string(),
                channel_name: String::new(),
                readable: true,
                writable: true,
                whitelisted: false,
                heartbeat_enabled: true,
                heartbeat_interval_secs: None,
                heartbeat_instructions: "業務連絡のみ".to_string(),
            },
        )
        .unwrap();
    }
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "effective", "channel_id": "ch1"}),
            &trusted_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(data["scope"], "effective");
    assert_eq!(data["channel_id"], "ch1");
    assert_eq!(data["source"], "channel");
    assert_eq!(data["instructions"], "業務連絡のみ");
}

/// 素の agent は拒否、co_agent は許可（旧テストの移植）。移設後も権限のゲートが効く。
#[tokio::test]
async fn read_heartbeat_instructions_rejected_for_plain_agent() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "agent"}),
            &agent_ctx(),
        )
        .await;
    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションは信頼済みの呼び出し元のみ実行できます")
    );

    let allowed = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "agent"}),
            &GatewayCallContext::new(
                GatewayCaller::CoAgent {
                    agent_id: "co-agent-1".to_string(),
                },
                "agent-x",
            ),
        )
        .await;
    assert!(allowed.success, "{:?}", allowed.error);
    assert_eq!(
        allowed.data.unwrap(),
        json!({"scope": "agent", "instructions": ""})
    );
}

/// **チャンネル単位設定の非対称（#157 S3）**: 非 Discord 経路には通常チャンネル設定の
/// 行が無いので、`scope="channel"` は空文字列を返し、`scope="effective"` は
/// エージェント/既定へフォールバックする。エラーにはならない（露出はする）。
#[tokio::test]
async fn read_heartbeat_instructions_channel_scope_is_empty_without_a_channel_row() {
    let state = crate::test_app_state();
    insert_agent(&state, "エージェント既定の指示");
    let actions = SystemGatewayActions::new(state, None, None, None);

    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "channel", "channel_id": "no-such-channel"}),
            &trusted_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "scope": "channel",
            "channel_id": "no-such-channel",
            "instructions": "",
        })
    );

    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "effective", "channel_id": "no-such-channel"}),
            &trusted_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(data["instructions"], "エージェント既定の指示");
    assert_eq!(data["source"], "agent");
}

/// 読み出しの引数エラー文言も移設前と同一。
#[tokio::test]
async fn read_heartbeat_instructions_bad_args() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "channel"}),
            &trusted_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("scope=channelのときはchannel_idが必要です")
    );

    let r = actions
        .execute(
            "read_heartbeat_instructions",
            &json!({"scope": "nope"}),
            &trusted_ctx(),
        )
        .await;
    assert_eq!(
        r.error.as_deref(),
        Some("不明なscope: nope（agent / channel / effective）")
    );
}

/// **negative assert（#157 S3）**: Discord がハートビート指示ツールを再定義しても own が
/// 処理する（委譲パターンにしない）。
#[tokio::test]
async fn heartbeat_instruction_tools_are_not_delegated_to_inner() {
    let state = crate::test_app_state();
    insert_agent(&state, "OLD");
    let inner = Arc::new(RecordingInner::new(&[
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ]));
    let actions = SystemGatewayActions::new(
        state,
        Some(inner.clone() as Arc<dyn GatewayActions>),
        None,
        None,
    );

    for (name, args) in [
        (
            "update_heartbeat_instructions",
            json!({"scope": "agent", "instructions": "NEW"}),
        ),
        ("read_heartbeat_instructions", json!({"scope": "agent"})),
    ] {
        let r = actions.execute(name, &args, &owner_ctx()).await;
        assert!(r.success, "{name}: {:?}", r.error);
        assert!(
            r.data.as_ref().unwrap().get("reached_inner").is_none(),
            "{name} が inner へ委譲されている（own が処理すべき）"
        );
    }
    assert!(
        inner.calls().is_empty(),
        "inner へ到達してはならない: {:?}",
        inner.calls()
    );

    // merge 後も 1 件（own 優先で dedup）。
    let inner2: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&[
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ]));
    let merged = SystemGatewayActions::merge_definitions(
        SystemGatewayActions::own_definitions(),
        Some(&inner2),
    );
    for name in [
        "update_heartbeat_instructions",
        "read_heartbeat_instructions",
    ] {
        assert_eq!(merged.iter().filter(|d| d.name == name).count(), 1);
    }
}
