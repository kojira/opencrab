use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

// ================================================================================
// #157 S2 / #184: 停止（cancel_subtask）の移植テスト
//
// 旧 Discord 実装（`crates/discord` の `execute_cancel_subtask`）にあった 8 テストを
// そのまま持ってきたもの（1 件も落としていない）。停止の実装は
// `opencrab_actions::cancel_subtask` 1 箇所になったので、契約はこの合成層で固定する。
// ================================================================================

/// 停止対象を任意の label / tool_name で 1 件登録した registry を作る。
fn registry_with_labeled(
    subtask_id: &str,
    session_id: &str,
    parent_session_id: &str,
    label: &str,
    tool_name: &str,
) -> SubtaskRegistry {
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    registry.insert(
        subtask_id.to_string(),
        opencrab_actions::SpawnedSubtask {
            abort_handle: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            session_id: session_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            agent_id: "agent-x".to_string(),
            label: label.to_string(),
            tool_name: tool_name.to_string(),
            started_at: std::time::Instant::now(),
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Agent,
            lifecycle: opencrab_actions::SubtaskLifecycle::new(),
            steerable: false,
        },
    );
    registry
}

/// sub-session の行を作る（明示的な `spawn_subtask` 相当）。
fn insert_sub_session(state: &AppState, session_id: &str, theme: &str) {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::insert_session(
        &conn,
        &opencrab_db::queries::SessionRow {
            id: session_id.to_string(),
            mode: "subtask".to_string(),
            theme: theme.to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: json!(["agent-x"]).to_string(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

/// 停止ログ（`tool_cancelled`）を親セッションから 1 件だけ引く。
fn cancelled_log(state: &AppState, parent_session_id: &str) -> opencrab_db::queries::SessionLogRow {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_recent_session_logs(&conn, parent_session_id, 20)
        .unwrap()
        .into_iter()
        .find(|l| l.log_type == "tool_cancelled")
        .expect("tool_cancelled が親ログに残る")
}

fn cancelled_log_metadata(state: &AppState, parent_session_id: &str) -> Value {
    serde_json::from_str(
        cancelled_log(state, parent_session_id)
            .metadata_json
            .as_deref()
            .unwrap(),
    )
    .unwrap()
}

fn parent_ctx(parent_session_id: &str) -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id(parent_session_id)
}

async fn cancel(
    actions: &SystemGatewayActions,
    subtask_id: &str,
    ctx: &GatewayCallContext,
) -> GatewayActionResult {
    actions
        .execute("cancel_subtask", &json!({"subtask_id": subtask_id}), ctx)
        .await
}

/// 不在は**権限拒否ではない**プレーンなエラー（旧 Discord テストの移植）。
#[tokio::test]
async fn cancel_subtask_not_found_is_plain_error() {
    let state = crate::test_app_state();
    let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
    let actions = SystemGatewayActions::new(state, None, Some(registry), None);
    let r = cancel(&actions, "no-such", &parent_ctx("web-agent-x-c1")).await;
    assert!(!r.success);
    let err = r.error.unwrap();
    assert_eq!(err, "cancel_subtask: subtask 'no-such' not found");
    assert!(!err.starts_with(REJECTION_CODE_PREFIX));
}

/// 他セッションが親の subtask は拒否し、エントリも残す（abort しない）。
#[tokio::test]
async fn cancel_subtask_rejects_foreign_session() {
    let state = crate::test_app_state();
    let registry = registry_with("st-x", "subtask-x1", "web-other-c9");
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-x", &parent_ctx("web-agent-x-c1")).await;
    assert!(!r.success);
    assert_eq!(
            r.error.as_deref().unwrap(),
            format!("{REJECTION_CODE_PREFIX}cancel_subtask: subtask 'st-x' をこのセッションからキャンセルする権限がありません（親セッションまたは owner のみ）")
        );
    assert!(registry.contains_key("st-x"), "abort されていない");
}

/// 親セッションからの停止は成功し、registry から除去される。
#[tokio::test]
async fn cancel_subtask_allows_parent_session() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-mine", "subtask-m1", parent);
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-mine", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    // レスポンス JSON も旧実装と同一。
    assert_eq!(
        r.data.unwrap(),
        json!({"cancelled": true, "subtask_id": "st-mine"})
    );
    assert!(!registry.contains_key("st-mine"));
}

/// owner は無関係なセッション文脈からでも停止できる。
#[tokio::test]
async fn cancel_subtask_owner_bypasses_session_check() {
    let state = crate::test_app_state();
    let registry = registry_with("st-any", "subtask-a1", "web-other-c9");
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-any", &owner_ctx()).await;
    assert!(r.success, "{:?}", r.error);
    assert!(!registry.contains_key("st-any"));
}

/// セッション文脈の無い agent は他人の subtask を停止できない。
#[tokio::test]
async fn cancel_subtask_rejects_agent_without_session() {
    let state = crate::test_app_state();
    let registry = registry_with("st-ns", "subtask-n1", "web-other-c9");
    let actions = SystemGatewayActions::new(state, None, Some(registry.clone()), None);
    let r = cancel(&actions, "st-ns", &agent_ctx()).await;
    assert!(!r.success);
    assert!(r
        .error
        .as_deref()
        .unwrap()
        .starts_with(REJECTION_CODE_PREFIX));
    assert!(registry.contains_key("st-ns"));
}

/// #176: 自動 dispatch した subtask は sub-session の行を持たないため theme を引けず、
/// registry の label（ツール名を含む）へフォールバックする。
#[tokio::test]
async fn cancel_subtask_falls_back_to_label_without_sub_session() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    // sub-session は**作らない**（自動 dispatch の再現）。
    let registry = registry_with_labeled(
        "st-auto",
        "subtask-auto1",
        parent,
        "execute_shell(ls -la)",
        "execute_shell",
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let r = cancel(&actions, "st-auto", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);

    let log = cancelled_log(&state, parent);
    assert_ne!(
        log.content, "subtask '' was cancelled",
        "sub-session が無いとラベルが空になっている（#176 の退行）"
    );
    assert_eq!(log.content, "subtask 'execute_shell(ls -la)' was cancelled");
    let meta = cancelled_log_metadata(&state, parent);
    assert_eq!(meta["task"], "execute_shell(ls -la)");
    // #184: 種別名は固定値ではなく**実際に停止したツール名**。
    assert_eq!(meta["tool_name"], "execute_shell");
    assert_eq!(meta["tool_call_id"], "st-auto");
    assert_eq!(meta["label"], "execute_shell(ls -la)");
    assert_eq!(meta["completed_calls"], json!([]));
}

/// 明示的な `spawn_subtask`（sub-session あり）では theme を使い、`Subtask: ` prefix を
/// 除去する。
#[tokio::test]
async fn cancel_subtask_prefers_sub_session_theme() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    insert_sub_session(&state, "subtask-explicit1", "Subtask: ログを調査する");
    let registry = registry_with_labeled(
        "st-explicit",
        "subtask-explicit1",
        parent,
        "spawn_subtask(ログを調査する)",
        "spawn_subtask",
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let r = cancel(&actions, "st-explicit", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);

    assert_eq!(
        cancelled_log(&state, parent).content,
        "subtask 'ログを調査する' was cancelled"
    );
    let meta = cancelled_log_metadata(&state, parent);
    assert_eq!(meta["task"], "ログを調査する");
    assert_eq!(meta["tool_name"], "spawn_subtask");
}

/// sub-session はあるが theme が空のケースでも label へフォールバックする。
#[tokio::test]
async fn cancel_subtask_falls_back_on_empty_theme() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    insert_sub_session(&state, "subtask-empty1", "");
    let registry = registry_with_labeled(
        "st-empty",
        "subtask-empty1",
        parent,
        "nostr_generate_key(main)",
        "nostr_generate_key",
    );
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);
    let r = cancel(&actions, "st-empty", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        cancelled_log(&state, parent).content,
        "subtask 'nostr_generate_key(main)' was cancelled"
    );
}

/// 旧 Discord 実装の固有の後始末その 1: **中断を lifecycle 通知口へ伝え、随伴マップ
/// から外す**。落とすと lifecycle webhook の `aborted` が黙って消える。
#[tokio::test]
async fn cancel_subtask_notifies_the_run_notifier() {
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);
    impl opencrab_actions::subtask_notify::SubtaskRunNotifier for Recorder {
        fn on_cancelled(&self, _duration_ms: u64) {
            self.0.lock().unwrap().push("cancelled".to_string());
        }
    }

    let state = crate::test_app_state();
    let recorder = Arc::new(Recorder::default());
    state
        .subtask_notifiers
        .insert("st-1".to_string(), recorder.clone());
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-1", "subtask-st-1", parent);
    let actions = SystemGatewayActions::new(state.clone(), None, Some(registry), None);

    let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(recorder.0.lock().unwrap().clone(), vec!["cancelled"]);
    assert!(
        !state.subtask_notifiers.contains_key("st-1"),
        "通知口は registry と対で除去する"
    );
}

/// **停止も完了 sink（`on_subtask_cancelled`）へ通知する**（#184 / REST の永久 active
/// バグ）。委譲していた頃の Discord 経路はこれを落としていた。
#[tokio::test]
async fn cancel_subtask_notifies_the_completion_sink() {
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<String>>);
    impl SubtaskCompletionSink for Recorder {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, _ev: SubtaskSettled) {
            self.0.lock().unwrap().push("settled".to_string());
        }
        fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
            self.0
                .lock()
                .unwrap()
                .push(format!("cancelled:{}:{}", ev.subtask_id, ev.exit_reason));
        }
    }

    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-1", "subtask-st-1", parent);
    let sink = Arc::new(Recorder::default());
    let actions = SystemGatewayActions::new(
        state,
        None,
        Some(registry),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        sink.0.lock().unwrap().clone(),
        vec!["cancelled:st-1:cancelled"],
        "停止は on_subtask_cancelled だけを呼ぶ（resume する on_subtask_settled は呼ばない）"
    );
}

/// **negative assert（#157 S2）**: Discord が `cancel_subtask` を再定義しても own が
/// 処理する。委譲パターンに戻すと own の後始末（通知・部分結果ログ・sink）が黙って
/// バイパスされるので、その経路を作らせない。
#[tokio::test]
async fn cancel_subtask_is_not_delegated_to_inner() {
    let state = crate::test_app_state();
    let parent = "web-agent-x-c1";
    let registry = registry_with("st-1", "subtask-st-1", parent);
    let inner = Arc::new(RecordingInner::new(&["cancel_subtask"]));
    let actions = SystemGatewayActions::new(
        state,
        Some(inner.clone() as Arc<dyn GatewayActions>),
        Some(registry.clone()),
        None,
    );

    let r = cancel(&actions, "st-1", &parent_ctx(parent)).await;
    assert!(r.success, "{:?}", r.error);
    assert!(
        r.data.as_ref().unwrap().get("reached_inner").is_none(),
        "cancel_subtask が inner へ委譲されている（own が処理すべき）"
    );
    assert!(
        inner.calls().is_empty(),
        "inner へ到達してはならない: {:?}",
        inner.calls()
    );
    assert!(!registry.contains_key("st-1"), "own が実際に停止している");
}

/// merge 後も `cancel_subtask` は 1 件（own 優先で dedup）。
#[test]
fn merge_definitions_still_dedups_cancel_subtask() {
    let inner: Arc<dyn GatewayActions> = Arc::new(RecordingInner::new(&["cancel_subtask"]));
    let merged = SystemGatewayActions::merge_definitions(
        SystemGatewayActions::own_definitions(),
        Some(&inner),
    );
    assert_eq!(
        merged.iter().filter(|d| d.name == "cancel_subtask").count(),
        1
    );
}
