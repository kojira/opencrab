// ==================== Subtask dispatch integration ====================

/// **#298**: 非ブロック dispatch した subtask は、**その run の呼び出し元**を決着通知まで
/// 運ぶ。resume する sink（Discord / web）はこの値で `RunRequest` を組むので、ここで落ちると
/// オーナー発のターンが決着の瞬間に最小権限へ降格し、`policy_allows` が owner/trusted の
/// ツールを list_tools からも dispatch からも落とす。
///
/// このテストが必要な理由: 配線点は `process.rs` の 1 箇所
/// （`SubtaskToolDispatcher::with_caller`）にしかなく、そこを外しても `crates/actions` 側の
/// ユニットテストは**自前で dispatcher を組む**ので落ちない（配線の写しでしかない）。
#[tokio::test]
async fn test_dispatched_subtask_carries_the_run_caller_to_settlement() {
    /// 決着通知を溜めるだけの sink。
    #[derive(Default)]
    struct CaptureSink(Mutex<Vec<opencrab_actions::SubtaskSettled>>);
    impl opencrab_actions::SubtaskCompletionSink for CaptureSink {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, ev: opencrab_actions::SubtaskSettled) {
            self.0.lock().unwrap().push(ev);
        }
    }

    // 昇格経路は作らない（元が `Agent` なら `Agent` のまま）ので両方を見る。
    for caller in [
        opencrab_actions::CallerIdentity::Owner,
        opencrab_actions::CallerIdentity::Agent,
    ] {
        let (app, _db, mock, state) = create_test_app_with_state();
        let (agent_id, _app) = create_test_agent_named(app, "DispatchCaller", "TestPersona").await;
        let session_id = format!("web-{agent_id}-u298");

        mock.push_tool_call_response(vec![ToolCall {
            id: "tc-298".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "learn_from_experience".to_string(),
                arguments: serde_json::json!({
                    "skill_name": "background_work",
                    "description": "d",
                    "situation_pattern": "s",
                    "guidance": "g"
                })
                .to_string(),
            },
        }]);
        mock.push_text_response("バックグラウンドで実行を開始しました");

        let capture = Arc::new(CaptureSink::default());
        let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = capture.clone();
        let run_req = opencrab_actions::RunRequest::new(
            &agent_id,
            "DispatchCaller",
            &session_id,
            "system",
            "user: スキルを覚えて",
            "web",
            caller.clone(),
        )
        .with_dispatch(
            Some(state.subtask_registries.registry_for(&session_id)),
            sink,
        );
        opencrab_server::process::run_agent_response(&state, run_req)
            .await
            .expect("dispatch する run が失敗した");

        // 非ブロック dispatch なので決着は別タスク。CI 負荷時に取りこぼさないよう
        // 上限は 5 秒（成功時は最初の観測で即抜けるので通常はほぼ待たない）。
        let mut observed = None;
        for _ in 0..250 {
            observed = capture
                .0
                .lock()
                .unwrap()
                .first()
                .map(|ev| ev.caller.clone());
            if observed.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            observed.expect("dispatch した subtask が決着していない（前提が崩れている）"),
            caller,
            "dispatch した subtask が run の呼び出し元を運んでいない（resume が降格する）"
        );
    }
}

/// #431: `RunRequest::subtask_starts` が **両方の起動経路**から加算される。
///
/// Discord の「発言終わり」🏁 はこの数が `0` かどうかだけで「次の行動を選ばずに終わった
/// ターンか」を判定する。数え漏らすと、掘削を始めたターンに 🏁 が付き『調べますね🏁』の
/// 数分後に完了 resume の続きが届く**逆の情報**になる。
///
/// このテストが必要な理由: 配線点は `process.rs` の 2 箇所
/// （`SubtaskToolDispatcher::with_subtask_starts` と
/// `SystemGatewayActions::with_subtask_starts`）にしかなく、そこを外しても
/// `crates/actions` / `crates/server` 側のユニットテストは**自前で dispatcher や
/// gateway を組む**ので落ちない（配線の写しでしかない）。上の
/// `test_dispatched_subtask_carries_the_run_caller_to_settlement` と同じ理由。
#[tokio::test]
async fn test_run_counts_subtask_starts_from_both_launch_paths() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 決着通知を捨てるだけの sink（ここで見たいのは起動の計上だけ）。
    struct NoopSink;
    impl opencrab_actions::SubtaskCompletionSink for NoopSink {
        fn session_prefix(&self) -> &'static str {
            ""
        }
        fn forwards_progress(&self) -> bool {
            true
        }
        fn deliver_continuation(&self, _ev: opencrab_actions::SubtaskSettled) {}
    }

    // (ツール名, 引数) — 左が auto-dispatch 経路、右が明示 spawn_subtask 経路。
    let cases = [
        (
            "learn_from_experience",
            serde_json::json!({
                "skill_name": "background_work",
                "description": "d",
                "situation_pattern": "s",
                "guidance": "g"
            }),
        ),
        (
            "spawn_subtask",
            serde_json::json!({ "task": "ログを調べる", "label": "dig" }),
        ),
    ];

    for (tool_name, args) in cases {
        let (app, _db, mock, state) = create_test_app_with_state();
        let (agent_id, _app) = create_test_agent_named(app, "StartCounter", "TestPersona").await;
        let session_id = format!("web-{agent_id}-u431");

        mock.push_tool_call_response(vec![ToolCall {
            id: "tc-431".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: tool_name.to_string(),
                arguments: args.to_string(),
            },
        }]);
        // 親ターンの締め。明示 spawn 経路は sub-engine も同じモックから引くので多めに積む。
        mock.push_text_response("調べますね");
        mock.push_text_response("調べますね");

        let starts = Arc::new(AtomicUsize::new(0));
        let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = Arc::new(NoopSink);
        let run_req = opencrab_actions::RunRequest::new(
            &agent_id,
            "StartCounter",
            &session_id,
            "system",
            "user: ログを調べて",
            "web",
            opencrab_actions::CallerIdentity::Owner,
        )
        .with_dispatch(
            Some(state.subtask_registries.registry_for(&session_id)),
            sink,
        )
        .with_subtask_starts(starts.clone());

        opencrab_server::process::run_agent_response(&state, run_req)
            .await
            .expect("subtask を起こす run が失敗した");

        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "{tool_name} 経路で起動した subtask が親ターンのカウンタに載っていない\
             （このターンに 🏁 が付き、数分後に続きが届く逆情報になる）"
        );
    }
}
