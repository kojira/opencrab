use std::sync::{Arc, Mutex};

use opencrab_actions::subtask::{SettleKind, SubtaskCompletionSink, SubtaskRegistry};
use opencrab_actions::subtask_notify::SubtaskRunInfo;
use opencrab_gateway::{GatewayActions as _, GatewayCallContext, GatewayCaller};
use serde_json::json;

use crate::system_actions::SystemGatewayActions;

use super::support::*;

/// **#175 S4 の主目的**: Discord を通さない経路（web / REST 相当 = inner gateway なし）
/// から `spawn_subtask` が動き、完了が親セッションログへ着地する。
///
/// 旧実装は Discord ゲートウェイにしか無く、REST は LLM クライアントとして `None` を
/// 渡していたため「no LLM client available」で必ず失敗していた。
///
/// #450: 「spawn 直後は登録簿に載っている」assert は、子が即完了して registry から
/// remove した後に親が assert する競合を塞げていなかった（`:233-` の開始ゲートは
/// 「親 insert より先に子が remove しない」順序しか保証しない）。ここでは
/// **完了を `gate` の合図まで遅延**させ、親が登録を確認し終えるまで子が決着できない
/// ようにして順序を固定する（`sleep` で隠さない）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_subtask_runs_and_settles_without_discord() {
    // 子の完了ゲート。親が登録を確認するまで `true` を送らない。
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let state = state_with_gated_llm("sub-engine done", gate_rx);
    let reg = registry();
    let sink = Arc::new(OrderCheckingSink::new(state.db.clone()));
    let actions = SystemGatewayActions::new(
        state.clone(),
        None, // inner gateway 無し（= web / REST / Nostr 経路）
        Some(reg.clone()),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let res = actions
        .execute(
            "spawn_subtask",
            &json!({ "task": "調べ物をする", "label": "job" }),
            &parent_ctx("web-parent-1"),
        )
        .await;
    assert!(res.success, "spawn_subtask: {:?}", res.error);
    let subtask_id = spawned_id(&res);

    // spawn は即座に返り、走行中エントリが共有登録簿に載っている。子はまだ `gate` の
    // 合図待ちで settle できないため、この確認は競合なく成立する（#450）。ここで登録が
    // 無ければ本物の退行（spawn したのに登録されない）＝赤になる。
    assert_eq!(res.data.as_ref().unwrap()["status"], "spawned");
    assert!(
        reg.contains_key(&subtask_id),
        "spawn 直後は登録簿に載っていなければならない"
    );
    // 親セッションログに subtask_spawned が残る。
    assert!(has_log_of_type(
        &state.db,
        "web-parent-1",
        "subtask_spawned"
    ));

    // 登録を確認したので、子の完了を許可する。
    gate_tx
        .send(true)
        .expect("gate receiver は sub-run が保持している");

    // 決着を待つ。
    assert!(
        wait_until(|| !sink.seen().is_empty()).await,
        "完了通知が届かない"
    );
    let seen = sink.seen();
    assert_eq!(seen.len(), 1, "決着通知はちょうど 1 本: {seen:?}");
    assert_eq!(seen[0].0, SettleKind::Completed);
    assert_eq!(seen[0].1, "completed", "sub-engine は正常終了する");
    // **順序契約**: sink が呼ばれた時点で完了本文は既に DB へ永続化されている。
    assert!(seen[0].2, "順序契約違反: DB 永続化より先に sink が発火した");

    assert_eq!(
        completed_result(&state.db, "web-parent-1").as_deref(),
        Some("sub-engine done"),
        "完了本文が親セッションログへ着地する"
    );
    // 決着後は登録簿からも随伴マップからも消える。
    assert!(!reg.contains_key(&subtask_id));
    assert!(!state.subtask_notifiers.contains_key(&subtask_id));
}

/// #431: **明示 `spawn_subtask` も**親ターンの subtask 起動カウンタを進める。
///
/// auto-dispatch 経路（`SubtaskToolDispatcher`）だけを数えていると、この経路で
/// 掘削を始めたターンに「発言終わり」🏁 が付き、『調べますね🏁』の数分後に完了
/// resume の続きが届く逆情報になる。両経路が**同じカウンタ**へ載ることを固定する。
///
/// 起動に**失敗**したターンは数えない（resume が来ない＝そのターンが最後の発話
/// なので 🏁 は付くのが正しい）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_subtask_counts_the_start_for_the_parent_turn() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let state = state_with_stub_llm("never", true);
    let reg = registry();
    let starts = Arc::new(AtomicUsize::new(0));
    let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None)
        .with_subtask_starts(Some(starts.clone()));

    let res = actions
        .execute(
            "spawn_subtask",
            &json!({ "task": "調べ物をする", "label": "job" }),
            &parent_ctx("web-parent-count"),
        )
        .await;
    assert!(res.success, "spawn_subtask: {:?}", res.error);
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "起動が成立したら親ターンのカウンタが進む"
    );

    // 起動に失敗するターン（`task` 引数なし）は数えない。
    let failed = actions
        .execute("spawn_subtask", &json!({}), &parent_ctx("web-parent-count"))
        .await;
    assert!(!failed.success, "task 引数なしは失敗する");
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "起動に失敗したターンは数えない（resume が来ないので 🏁 は付いてよい）"
    );
}

/// **停止の到達性**: spawn した subtask は、同じ `SystemGatewayActions` の
/// `cancel_subtask` が引く**同一の登録簿**に入る。別の登録簿へ入れると not found。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_subtask_is_cancellable_through_the_shared_registry() {
    let state = state_with_stub_llm("never", true);
    let reg = registry();
    let sink = Arc::new(OrderCheckingSink::new(state.db.clone()));
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(reg.clone()),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );

    let res = actions
        .execute(
            "spawn_subtask",
            &json!({ "task": "終わらない仕事", "label": "endless" }),
            &parent_ctx("web-parent-1"),
        )
        .await;
    assert!(res.success, "spawn_subtask: {:?}", res.error);
    let subtask_id = spawned_id(&res);

    // 親セッションから停止できる。
    let cancelled = actions
        .execute(
            "cancel_subtask",
            &json!({ "subtask_id": subtask_id }),
            &parent_ctx("web-parent-1"),
        )
        .await;
    assert!(
        cancelled.success,
        "spawn した subtask は同一登録簿から停止できなければならない: {:?}",
        cancelled.error
    );
    assert!(!reg.contains_key(&subtask_id), "停止後は登録簿から消える");
    assert!(has_log_of_type(
        &state.db,
        "web-parent-1",
        "subtask_spawned"
    ));

    // 停止は `on_subtask_cancelled`（resume しない別メソッド）で通知される。
    let seen = sink.seen();
    assert_eq!(seen.len(), 1, "停止通知は 1 本: {seen:?}");
    assert_eq!(seen[0].0, SettleKind::Cancelled);

    // 二重決着しない: 完了ログは着地しない（止めたのに返信が届くのを防ぐ）。
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !has_log_of_type(&state.db, "web-parent-1", "subtask_completed"),
        "停止した subtask の完了ログが着地してはならない"
    );
}

/// **開始ゲート**: 走行が終わる時点では必ず登録簿へ登録済み。登録より先に決着すると
/// 「running のまま」のエントリがリークする。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subtask_is_registered_before_the_run_can_finish() {
    /// 終了時点の登録簿の状態を記録する通知口。
    struct RegistryWatcher {
        registry: SubtaskRegistry,
        registered_at_finish: Mutex<Option<bool>>,
    }
    impl opencrab_actions::subtask_notify::SubtaskRunNotifier for RegistryWatcher {
        fn on_finished(&self, _exit: &str, _ms: u64, _text: &str) {
            *self.registered_at_finish.lock().unwrap() = Some(!self.registry.is_empty());
        }
    }
    struct WatcherFactory(Arc<RegistryWatcher>);
    impl opencrab_actions::subtask_notify::SubtaskLifecycleNotifier for WatcherFactory {
        fn begin_run(
            &self,
            _run: &SubtaskRunInfo<'_>,
        ) -> Result<
            opencrab_actions::subtask_notify::SubtaskNotifySession,
            opencrab_actions::subtask_notify::NotifyTargetError,
        > {
            Ok(opencrab_actions::subtask_notify::SubtaskNotifySession {
                notifier: self.0.clone(),
                target: opencrab_actions::subtask_notify::NotifyTarget::none(),
            })
        }
    }

    let state = state_with_stub_llm("fast", false);
    let reg = registry();
    let watcher = Arc::new(RegistryWatcher {
        registry: reg.clone(),
        registered_at_finish: Mutex::new(None),
    });
    *state.subtask_lifecycle_notifier.lock().unwrap() =
        Some(Arc::new(WatcherFactory(watcher.clone())));

    let sink = Arc::new(OrderCheckingSink::new(state.db.clone()));
    let actions = SystemGatewayActions::new(
        state.clone(),
        None,
        Some(reg.clone()),
        Some(sink.clone() as Arc<dyn SubtaskCompletionSink>),
    );
    let res = actions
        .execute(
            "spawn_subtask",
            &json!({ "task": "すぐ終わる" }),
            &parent_ctx("web-parent-1"),
        )
        .await;
    assert!(res.success, "{:?}", res.error);

    assert!(
        wait_until(|| watcher.registered_at_finish.lock().unwrap().is_some()).await,
        "終了通知が届かない"
    );
    assert_eq!(
        *watcher.registered_at_finish.lock().unwrap(),
        Some(true),
        "開始ゲートが無いと、登録より先に決着してエントリがリークする"
    );
    // 決着後は空（リークしていない）。
    assert!(
        wait_until(|| reg.is_empty()).await,
        "登録簿にエントリが残った"
    );
}

/// #298/#333: spawn した subtask は**親ターンの呼び出し元**を登録簿に持つ。
///
/// 決着時に `settle_completed` がこれを読んで sink へ渡し、resume が元の権限で走る。
/// #333 以降は sub-engine の実行 caller も同じ親 caller（`parent_caller`）なので、
/// 登録簿の caller は「実行時に見えていた権限」と一致する（resume でズレない）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_subtask_records_the_parent_caller() {
    let state = state_with_stub_llm("never", true);
    let reg = registry();
    let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

    let ctx = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
        .with_session_id("discord-agent-x-1-2");
    let res = actions
        .execute("spawn_subtask", &json!({ "task": "長い仕事" }), &ctx)
        .await;
    assert!(res.success, "spawn_subtask: {:?}", res.error);

    let subtask_id = spawned_id(&res);
    let entry = reg.get(&subtask_id).expect("登録簿に載る");
    assert_eq!(
        entry.caller,
        opencrab_actions::CallerIdentity::Owner,
        "親ターンの呼び出し元が登録簿に保持されていない（resume で降格する）"
    );
    entry.abort_handle.abort();
}

/// 昇格経路にはしない: 親が `Agent` なら登録簿の caller も `Agent`。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_subtask_does_not_escalate_agent_callers() {
    let state = state_with_stub_llm("never", true);
    let reg = registry();
    let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

    let res = actions
        .execute(
            "spawn_subtask",
            &json!({ "task": "長い仕事" }),
            &parent_ctx("web-parent-1"),
        )
        .await;
    assert!(res.success, "{:?}", res.error);
    let subtask_id = spawned_id(&res);
    let entry = reg.get(&subtask_id).expect("登録簿に載る");
    assert_eq!(entry.caller, opencrab_actions::CallerIdentity::Agent);
    entry.abort_handle.abort();
}

/// #333 の本丸: sub-engine の**実行 caller** が親ターンの caller を継承すること、
/// および `spawn_subtask` 経由の迂回が閉じること。
///
/// sub-run が LLM へ提示するツール一覧を観測する。`execute_shell` / `ws_read` は
/// #330 で owner_only なので、提示されていれば sub-run の実行 caller は Owner、
/// 提示されていなければ Agent。
/// - **親 Owner → サブ Owner**: `execute_shell` / `ws_read` が見える（実装作業が死なない）。
/// - **親 Agent（外部由来ターン相当）→ サブ Agent**: どちらも消える
///   （`spawn_subtask` を挟んでローカル操作へ昇格する迂回路の封鎖）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sub_engine_inherits_parent_caller_and_closes_spawn_bypass() {
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let state = state_with_shell_and_capture(seen.clone());
    let reg = registry();
    let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

    // --- 親 Owner ---
    let owner_ctx =
        GatewayCallContext::new(GatewayCaller::Owner, "agent-x").with_session_id("sub-owner-1");
    let res = actions
        .execute("spawn_subtask", &json!({ "task": "t" }), &owner_ctx)
        .await;
    assert!(res.success, "spawn(owner): {:?}", res.error);
    assert!(
        wait_until(|| !seen.lock().unwrap().is_empty()).await,
        "親 Owner の sub-run が LLM を呼ばない"
    );
    let owner_tools = seen.lock().unwrap().last().unwrap().clone();
    assert!(
        owner_tools.iter().any(|t| t == "execute_shell"),
        "親 Owner のサブ run に execute_shell が出ない（継承されていない / #333）: {owner_tools:?}"
    );
    assert!(
        owner_tools.iter().any(|t| t == "ws_read"),
        "親 Owner のサブ run に ws_read が出ない（#333）: {owner_tools:?}"
    );

    // 観測を混ぜないよう、次の spawn の前に走行中サブを止めて登録簿を空にする。
    let owner_calls = seen.lock().unwrap().len();
    for id in reg.iter().map(|e| e.key().clone()).collect::<Vec<_>>() {
        if let Some(e) = reg.get(&id) {
            e.abort_handle.abort();
        }
    }
    reg.clear();

    // --- 親 Agent（外部由来ターン相当）---
    let agent_ctx =
        GatewayCallContext::new(GatewayCaller::Agent, "agent-x").with_session_id("sub-agent-1");
    let res2 = actions
        .execute("spawn_subtask", &json!({ "task": "t" }), &agent_ctx)
        .await;
    assert!(res2.success, "spawn(agent): {:?}", res2.error);
    assert!(
        wait_until(|| seen.lock().unwrap().len() > owner_calls).await,
        "親 Agent の sub-run が LLM を呼ばない"
    );
    let agent_tools = seen.lock().unwrap().last().unwrap().clone();
    assert!(
            !agent_tools.iter().any(|t| t == "execute_shell"),
            "外部 Agent 親のサブ run に execute_shell が出た = spawn_subtask 迂回が開いている（#333）: {agent_tools:?}"
        );
    assert!(
        !agent_tools.iter().any(|t| t == "ws_read"),
        "外部 Agent 親のサブ run に ws_read が出た（#333）: {agent_tools:?}"
    );

    for id in reg.iter().map(|e| e.key().clone()).collect::<Vec<_>>() {
        if let Some(e) = reg.get(&id) {
            e.abort_handle.abort();
        }
    }
}

/// セッション必須ガード（fail-closed）: session_id が無い文脈では起動できない。
#[tokio::test]
async fn spawn_subtask_requires_session_context() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, Some(registry()), None);
    let ctx = GatewayCallContext::new(GatewayCaller::Agent, "agent-x");
    let res = actions
        .execute("spawn_subtask", &json!({ "task": "t" }), &ctx)
        .await;
    assert!(!res.success);
    assert!(res.error.unwrap().contains("セッション"));
}

/// `task` は必須。
#[tokio::test]
async fn spawn_subtask_requires_task() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, Some(registry()), None);
    let res = actions
        .execute("spawn_subtask", &json!({}), &parent_ctx("web-parent-1"))
        .await;
    assert!(!res.success);
    assert!(res.error.unwrap().contains("'task' argument is required"));
}

/// **停止の到達性（fail-closed）**: 登録簿が未配線なら起動しない。走らせてしまうと
/// `cancel_subtask` から到達できない「見えない走行」になる。
#[tokio::test]
async fn spawn_subtask_refuses_without_a_registry() {
    let state = state_with_stub_llm("x", false);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let res = actions
        .execute(
            "spawn_subtask",
            &json!({ "task": "t" }),
            &parent_ctx("web-parent-1"),
        )
        .await;
    assert!(!res.success, "登録簿が無ければ起動してはならない");
    assert!(res.error.unwrap().contains("登録簿"));
    assert!(!has_log_of_type(
        &state.db,
        "web-parent-1",
        "subtask_spawned"
    ));
}

/// 通知先の解決に失敗したら spawn しない（fail-closed）。親ログも汚さない。
#[tokio::test]
async fn spawn_subtask_is_not_started_when_notify_target_fails() {
    struct FailingFactory;
    impl opencrab_actions::subtask_notify::SubtaskLifecycleNotifier for FailingFactory {
        fn begin_run(
            &self,
            _run: &SubtaskRunInfo<'_>,
        ) -> Result<
            opencrab_actions::subtask_notify::SubtaskNotifySession,
            opencrab_actions::subtask_notify::NotifyTargetError,
        > {
            Err(opencrab_actions::subtask_notify::NotifyTargetError {
                code: "invalid_webhook_url".to_string(),
                message: "url must start with https://".to_string(),
                source: "explicit",
            })
        }
    }

    let state = state_with_stub_llm("x", false);
    *state.subtask_lifecycle_notifier.lock().unwrap() = Some(Arc::new(FailingFactory));
    let reg = registry();
    let actions = SystemGatewayActions::new(state.clone(), None, Some(reg.clone()), None);

    let res = actions
        .execute(
            "spawn_subtask",
            &json!({ "task": "t" }),
            &parent_ctx("web-parent-1"),
        )
        .await;
    assert!(!res.success);
    assert!(res.error.unwrap().contains("invalid_webhook_url"));
    assert!(reg.is_empty(), "起動していないので登録簿は空");
    assert!(!has_log_of_type(
        &state.db,
        "web-parent-1",
        "subtask_spawned"
    ));
}
