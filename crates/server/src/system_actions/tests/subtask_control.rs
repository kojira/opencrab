use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

/// #647 gateway: 親セッションからの steer は success を返し、data.steered=true と note を載せる。
#[tokio::test]
async fn steer_subtask_gateway_accepted_maps_to_success() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-1", "message": "出力は JSON で" }),
            &ctx,
        )
        .await;
    assert!(r.success, "親からの steer は通る: {:?}", r.error);
    let data = r.data.expect("data");
    assert_eq!(data["steered"], json!(true));
    // sub-session の履歴に steer が 1 本落ちる。
    let conn = state.db.lock().unwrap();
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, "subtask-st-1").unwrap();
    assert_eq!(
        logs.iter()
            .filter(|l| l.log_type == opencrab_actions::STEER_LOG_TYPE)
            .count(),
        1
    );
}

/// #647 gateway: 空 message は fail-closed で弾く（registry を引くより前）。
#[tokio::test]
async fn steer_subtask_gateway_empty_message_is_rejected() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    // 空白のみ。
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-1", "message": "   " }),
            &ctx,
        )
        .await;
    assert!(!r.success, "空 message は弾く");
    assert!(r.error.unwrap().contains("message"));
    // message キーそのものが無い場合も弾く。
    let r2 = actions
        .execute("steer_subtask", &json!({ "subtask_id": "st-1" }), &ctx)
        .await;
    assert!(!r2.success, "message 欠落は弾く");
}

/// #647 gateway: registry 未配線（dispatch を追跡していない）は not found。
#[tokio::test]
async fn steer_subtask_gateway_no_registry_is_not_found() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-1", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("not found"));
}

/// #647 gateway: auto-dispatch（steerable=false）は NotSteerable をエラーで返す（黙って無視しない）。
#[tokio::test]
async fn steer_subtask_gateway_auto_dispatch_maps_to_error() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-ad",
        "subtask-st-ad",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    ); // steerable=false
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-ad", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("auto-dispatch"));
}

/// #647 gateway: 他セッションの Agent からは Unauthorized（拒否コード付き）。
#[tokio::test]
async fn steer_subtask_gateway_foreign_session_maps_to_unauthorized() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-f",
        "subtask-st-f",
        "web-other-c9",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "st-f", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().starts_with(REJECTION_CODE_PREFIX));
}

/// #647 gateway: registry にも DB にも無い id は not found（present registry + 別 id）。
#[tokio::test]
async fn steer_subtask_gateway_unknown_id_is_not_found() {
    let state = crate::test_app_state();
    let registry = registry_with_steerable(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Agent,
    );
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "steer_subtask",
            &json!({ "subtask_id": "does-not-exist", "message": "x" }),
            &ctx,
        )
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("not found"));
}

/// 親セッションログに記録された subtask_progress のメッセージ一覧。
fn progress_messages(state: &AppState, parent_session_id: &str) -> Vec<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_session_logs_by_session(&conn, parent_session_id)
        .unwrap()
        .into_iter()
        .filter_map(|row| {
            let v: Value = serde_json::from_str(&row.content).ok()?;
            if v.get("type").and_then(|t| t.as_str()) != Some("subtask_progress") {
                return None;
            }
            Some(v.get("message")?.as_str()?.to_string())
        })
        .collect()
}

/// **非 Discord（inner なし）で report_progress が動く**（#175 S1 の主目的）。
/// 親ログに本文が残り、デバウンス後に完了受け口へ `Progress` が届く。
#[tokio::test(start_paused = true)]
async fn report_progress_works_without_inner_gateway() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "halfway there" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(r.success, "error: {:?}", r.error);
    assert_eq!(r.data.as_ref().unwrap()["notified"], json!(true));

    // 本文は親セッションログへ永続化される（sink には運ばない / RFC §1.3）。
    assert_eq!(
        progress_messages(&state, "web-parent-1"),
        vec!["halfway there".to_string()]
    );

    // デバウンス満了後に Progress が 1 本届く。
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
    let settled = sink.settled();
    assert_eq!(settled.len(), 1, "デバウンス後に Progress が 1 本届く");
    assert_eq!(settled[0].kind, SettleKind::Progress);
    assert_eq!(settled[0].session_id, "web-parent-1");
    assert_eq!(settled[0].subtask_id, "st-1");
    assert_eq!(settled[0].exit_reason, "progress");
}

/// **Discord（inner あり）では inner へ委譲される**（S1 で Discord 経路は挙動不変）。
/// own 実装は走らない＝親ログを書かない。
#[tokio::test]
async fn report_progress_delegates_to_inner_when_inner_defines_it() {
    let state = crate::test_app_state();
    let inner = Arc::new(RecordingInner::new(&["report_progress", "spawn_subtask"]));
    let registry = registry_with("st-1", "subtask-st-1", "discord-parent-1");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        Some(inner.clone() as Arc<dyn GatewayActions>),
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "from discord" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap()["reached_inner"],
        json!("report_progress"),
        "inner（Discord 実装）へ委譲されなければならない"
    );
    assert_eq!(inner.calls(), vec!["report_progress".to_string()]);
    // own 実装は走っていない（親ログも sink も触っていない）。
    assert!(progress_messages(&state, "discord-parent-1").is_empty());
    assert!(sink.settled().is_empty());
}

/// 所有権ゲート: 他人の subtask（自分の session でも親でもない）は拒否する。
#[tokio::test]
async fn report_progress_rejects_foreign_subtask() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "parent-of-someone-else");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink as Arc<dyn SubtaskCompletionSink>),
    );

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "sneaky", "subtask_id": "st-1" }),
            &sub_ctx("some-other-session"),
        )
        .await;
    assert!(!r.success);
    let e = r.error.unwrap();
    assert!(
        e.starts_with(REJECTION_CODE_PREFIX),
        "権限拒否は構造的マーカー付き: {e}"
    );
    // 他セッションの親ログを汚さない。
    assert!(progress_messages(&state, "parent-of-someone-else").is_empty());
}

/// 親セッションからの代理報告は許す（所有権ゲートの片方の分岐）。
///
/// 所有権ゲートは「自分の subtask」か「自分が親である subtask」のどちらかなら通す。
/// 親側の分岐を落としても他のテストは全て通ってしまう（変異実験で確認済み）ため、
/// ここで固定する。Discord 側にも同趣旨のテストがある。
#[tokio::test]
async fn report_progress_allows_parent_reporting_child() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "parent-session");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    // 呼び出し元は subtask 本人ではなく「親セッション」。
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "親からの代理報告", "subtask_id": "st-1" }),
            &sub_ctx("parent-session"),
        )
        .await;
    assert!(
        r.success,
        "親セッションからの代理報告は許される: {:?}",
        r.error
    );
    assert!(
        progress_messages(&state, "parent-session")
            .iter()
            .any(|m| m.contains("親からの代理報告")),
        "親セッションのログへ記録される"
    );
}

/// #331: セッションを 1 本にした（#323）結果、親経路（`parent_session_id` 一致）だけでは
/// 見知らぬ相手（caller=Agent）のターンから Owner 由来の subtask へ進捗を差し込め、親会話の
/// resume（メインエンジン再呼び出し）を誘発できてしまう。caller ゲートでこれを塞ぐ。
#[tokio::test]
async fn report_progress_non_owner_cannot_report_owner_spawned_via_parent() {
    let state = crate::test_app_state();
    // オーナー発のターンが spawn した subtask。親は 1本化セッション（呼び出し元と一致）。
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Owner,
    );
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    // 見知らぬ相手（caller=Agent）のターン。session は親と一致している（1本化）。
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "sneaky", "subtask_id": "st-1" }),
            &ctx,
        )
        .await;
    assert!(!r.success, "非オーナーは Owner 由来へ進捗を差し込めない");
    assert!(
        r.error.unwrap().starts_with(REJECTION_CODE_PREFIX),
        "権限拒否は構造的マーカー付き"
    );
    // 親ログを汚さない & resume も起こさない。
    assert!(progress_messages(&state, "nostr-agent-a").is_empty());
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
    assert!(sink.settled().is_empty(), "resume を誘発しない");
}

/// #331: 同じ状況でも Owner のターンからは従来どおり進捗を代理報告できる。
#[tokio::test]
async fn report_progress_owner_can_report_owner_spawned_via_parent() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Owner,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    let ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "owner 代理報告", "subtask_id": "st-1" }),
            &ctx,
        )
        .await;
    assert!(r.success, "Owner のターンからは通る: {:?}", r.error);
    assert!(progress_messages(&state, "nostr-agent-a")
        .iter()
        .any(|m| m.contains("owner 代理報告")));
}

/// #331: サブエージェント自身（depth>=1・自セッション）の進捗報告は、subtask が Owner 由来
/// でも通る。self 経路には caller ゲートを掛けない（掛けると進捗報告が死ぬ）。自セッションは
/// 本人しか名乗れないので攻撃経路にはならない。
#[tokio::test]
async fn report_progress_subagent_self_report_survives_for_owner_spawned() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "nostr-agent-a",
        opencrab_actions::CallerIdentity::Owner,
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    // subtask 本人（sub-engine = caller Agent, depth 1, 自セッション）。
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "作業中です" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(
        r.success,
        "サブエージェント自身の進捗報告は Owner 由来でも通る: {:?}",
        r.error
    );
    assert!(progress_messages(&state, "nostr-agent-a")
        .iter()
        .any(|m| m == "作業中です"));
}

/// #331: Agent 由来の subtask は従来どおり Agent のターンから親経由で代理報告できる
/// （正常系を壊さない）。cancel 側の `cancel_subtask_agent_can_cancel_agent_spawned` に
/// 対応する report_progress 版。caller=Agent / spawner=Agent なので caller ゲートを通る。
#[tokio::test]
async fn report_progress_agent_can_report_agent_spawned_via_parent() {
    let state = crate::test_app_state();
    // 既定の caller=Agent。親は 1本化セッション（呼び出し元と一致）、subtask 本体は別セッション。
    let registry = registry_with("st-1", "subtask-st-1", "nostr-agent-a");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    // Agent のターン。session は親と一致（is_parent 経路）だが subtask 本体とは別。
    let ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("nostr-agent-a");
    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "agent 代理報告", "subtask_id": "st-1" }),
            &ctx,
        )
        .await;
    assert!(
        r.success,
        "Agent 由来の subtask は Agent のターンから親経由で代理報告できる: {:?}",
        r.error
    );
    assert!(progress_messages(&state, "nostr-agent-a")
        .iter()
        .any(|m| m.contains("agent 代理報告")));
}

/// セッション必須ガード（fail-closed）: session_id が無い文脈では実行できない。
#[tokio::test]
async fn report_progress_requires_session_context() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
    let r = actions
        .execute("report_progress", &json!({ "message": "x" }), &ctx)
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("session_id"));
}

/// message は必須。
#[tokio::test]
async fn report_progress_requires_message() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("report_progress", &json!({}), &sub_ctx("subtask-st-1"))
        .await;
    assert!(!r.success);
    assert!(r.error.unwrap().contains("'message' is required"));
}

/// 完了受け口が未配線なら、記録だけして通知はしない（デバウンスタスクも起動しない）。
/// 「黙って消える」のを避けるため、結果に `notified: false` を載せる。
#[tokio::test(start_paused = true)]
async fn report_progress_records_but_does_not_notify_without_sink() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "rest-parent-1");
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    let r = actions
        .execute(
            "report_progress",
            &json!({ "message": "no sink here" }),
            &sub_ctx("subtask-st-1"),
        )
        .await;
    assert!(r.success);
    assert_eq!(r.data.unwrap()["notified"], json!(false));
    // 記録は残る。
    assert_eq!(
        progress_messages(&state, "rest-parent-1"),
        vec!["no sink here".to_string()]
    );
    // デバウンスタスクを起動していない＝世代カウンタも進んでいない。
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;
    assert!(
        !state.progress_debounce.claim_latest("rest-parent-1", 1),
        "受け口未配線ではデバウンス世代を消費しない"
    );
}

/// **デバウンス状態が `AppState` 側にあることを固定する回帰テスト（#175 S1 の最重要点）**。
///
/// `SystemGatewayActions` は run ごとに作り直される。デバウンス世代カウンタを
/// この構造体のフィールドに置くと、2 回目の呼び出しで世代が 0 から張り直され、
/// **両方の呼び出しが発火する**（＝バーストで LLM を無駄に呼ぶ）。ここでは
/// 別インスタンスから 2 回報告し、届く `Progress` が 1 本だけであることを固定する。
#[tokio::test(start_paused = true)]
async fn progress_debounce_survives_gateway_recreation() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
    let sink = Arc::new(RecordingSink::default());

    // 1 回目: この run 用の gateway インスタンス。
    let first = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry.clone()),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );
    assert!(
        first
            .execute(
                "report_progress",
                &json!({ "message": "step 1" }),
                &sub_ctx("subtask-st-1")
            )
            .await
            .success
    );
    drop(first);

    // 2 回目: 別の run（＝別インスタンス）。同じ AppState を共有する。
    let second = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );
    assert!(
        second
            .execute(
                "report_progress",
                &json!({ "message": "step 2" }),
                &sub_ctx("subtask-st-1")
            )
            .await
            .success
    );

    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

    // 本文は 2 件とも親ログへ残る（間引くのは通知だけ）。
    assert_eq!(
        progress_messages(&state, "web-parent-1"),
        vec!["step 1".to_string(), "step 2".to_string()]
    );
    // 通知は最後の 1 本だけ。デバウンス状態をインスタンスのフィールドに移すと 2 本届く。
    let settled = sink.settled();
    assert_eq!(
            settled.len(),
            1,
            "デバウンスは gateway の作り直しを跨いで効かなければならない（AppState 側に置く）。届いた: {settled:?}"
        );
    assert_eq!(settled[0].kind, SettleKind::Progress);
}

/// **#298 の直接のトリガ**: `report_progress` のデバウンス発火は親会話を resume
/// するので、通知には**親ターンの呼び出し元**を載せる。
///
/// `ctx.caller`（= sub-engine 自身 = `Agent`）を載せると、進捗を報告した瞬間に
/// 親ターンが最小権限へ降格し、owner/trusted のツールが丸ごと消える。
#[tokio::test(start_paused = true)]
async fn report_progress_carries_the_parent_caller_to_the_sink() {
    let state = crate::test_app_state();
    let registry = registry_with_caller(
        "st-1",
        "subtask-st-1",
        "web-parent-1",
        opencrab_actions::CallerIdentity::Owner,
    );
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    assert!(
        actions
            .execute(
                "report_progress",
                &json!({ "message": "掘っています" }),
                // 呼ぶのは sub-engine（最小権限）。ここの caller を使ってはならない。
                &sub_ctx("subtask-st-1"),
            )
            .await
            .success
    );
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

    let settled = sink.settled();
    assert_eq!(settled.len(), 1, "進捗通知は 1 本: {settled:?}");
    assert_eq!(settled[0].kind, SettleKind::Progress);
    assert_eq!(
        settled[0].caller,
        opencrab_actions::CallerIdentity::Owner,
        "進捗を報告すると親ターンの権限が落ちる（#298 の自爆的な挙動）"
    );
}

/// 昇格経路にはしない: 親が `Agent` なら進捗通知の caller も `Agent`。
#[tokio::test(start_paused = true)]
async fn report_progress_does_not_escalate_agent_callers() {
    let state = crate::test_app_state();
    let registry = registry_with("st-1", "subtask-st-1", "web-parent-1");
    let sink = Arc::new(RecordingSink::default());
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    assert!(
        actions
            .execute(
                "report_progress",
                &json!({ "message": "掘っています" }),
                &sub_ctx("subtask-st-1"),
            )
            .await
            .success
    );
    tokio::time::sleep(PROGRESS_DEBOUNCE_DELAY + Duration::from_secs(1)).await;

    let settled = sink.settled();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].caller, opencrab_actions::CallerIdentity::Agent);
}
