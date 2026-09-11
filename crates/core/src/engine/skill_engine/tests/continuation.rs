    // -----------------------------------------------------------------------
    // 明示終端: NO_REPLY が出るまで既定継続する。
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn explicit_termination_plain_speech_without_no_reply_runs_next_call() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("今から調べる"),
            text_response("調査結果はX\nNO_REPLY"),
        ]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "調べて", "test-model")
            .await
            .expect("NO_REPLY まで継続する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(result.iterations, 2);
        assert_eq!(result.response, "調査結果はX");
    }

    #[tokio::test]
    async fn standalone_no_reply_keeps_prior_text_when_no_delivery_hook_exists() {
        let (llm, _) = MockLlm::counting(vec![
            text_response("64"),
            text_response("NO_REPLY"),
        ]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);

        let result = engine.run("system", "23+41", "test-model").await.unwrap();

        assert_eq!(result.response, "64");
    }

    #[tokio::test]
    async fn explicit_termination_utterance_without_no_reply_runs_next_call() {
        use std::sync::atomic::Ordering;
        use std::sync::Mutex;

        let (llm, chat_calls) = MockLlm::counting(vec![
            tool_call_response(vec![tc(
                "reply-1",
                "reply",
                serde_json::json!({"text": "確認するね"}),
            )]),
            text_response("確認結果はY\nNO_REPLY"),
        ]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "確認して", "test-model")
            .await
            .expect("reply 後も NO_REPLY まで継続する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(result.iterations, 2);
        assert_eq!(executor_calls.lock().unwrap().as_slice(), &["reply"]);
        assert_eq!(result.response, "確認結果はY");
    }

    #[tokio::test]
    async fn explicit_termination_body_then_no_reply_ends_in_one_call() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) =
            MockLlm::counting(vec![text_response("これが最終回答\nNO_REPLY")]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "答えて", "test-model")
            .await
            .expect("NO_REPLY で明示終了する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.iterations, 1);
        assert_eq!(result.response, "これが最終回答");
    }

    #[tokio::test]
    async fn explicit_termination_query_result_is_read_before_end() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            tool_call_response(vec![tc(
                "resolve-1",
                "resolve",
                serde_json::json!({"ref": "e1"}),
            )]),
            text_response("取得結果を確認した\nNO_REPLY"),
        ]);
        let executor = MockExecutor::new().add_result("resolve", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&["resolve"])));

        let result = engine
            .run("system", "取得して", "test-model")
            .await
            .expect("tool result 後に明示終了する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(result.iterations, 2);
        assert_eq!(result.response, "取得結果を確認した");
    }


    #[tokio::test]
    async fn missing_explicit_termination_stops_only_at_iteration_limit() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("まだ作業中1"),
            text_response("まだ作業中2"),
            text_response("まだ作業中3"),
        ]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);

        let result = engine
            .run("system", "続けて", "test-model")
            .await
            .expect("既存上限で停止する");

        assert!(result.stopped_by_limit);
        assert_eq!(chat_calls.load(Ordering::SeqCst), 3);
        assert_eq!(result.iterations, 4);
        assert!(result.response.is_empty(), "資源切れ本文を投稿してはならない");
    }
