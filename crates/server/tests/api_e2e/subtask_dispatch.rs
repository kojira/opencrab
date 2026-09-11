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
        mock.push_text_response("バックグラウンドで実行を開始しました\nNO_REPLY");

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
