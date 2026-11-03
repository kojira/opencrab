    // -----------------------------------------------------------------------
    // D-1110: a text-only generation right after a text-only continuation, with nothing new in
    // between (no tool result, no new inbound), ends the turn and is not delivered.
    // Production evidence: QC のすたろう repeated 「投票メニューを出したよ」 18 times after a
    // `send_ui` result; prod sleep-condense repeated its summary up to 26 times.
    // -----------------------------------------------------------------------

    fn recording_speech_hook(
        delivered: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> ContinuationSpeechHook {
        Arc::new(move |text| {
            let delivered = delivered.clone();
            Box::pin(async move {
                delivered.lock().unwrap().push(text);
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn operation_result_then_repeated_speech_delivers_once_and_ends() {
        use std::sync::atomic::Ordering;
        use std::sync::Mutex;

        let (llm, chat_calls) = MockLlm::counting(vec![
            tool_call_response(vec![tc("ui-1", "send_ui", serde_json::json!({}))]),
            text_response("投票メニューを出したよ"),
            text_response("投票メニューを出したよ"),
            text_response("投票メニューを1つ出したよ"),
        ]);
        let executor = MockExecutor::new().add_result("send_ui", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 30);
        engine.set_assistant_history_name("のすたろう");
        let delivered = Arc::new(Mutex::new(Vec::new()));
        engine.set_on_continuation_speech(recording_speech_hook(delivered.clone()));

        let history = "<conversation_history>\n[u1]:\n投票フォームを出して\n</conversation_history>";
        let result = engine.run("system", history, "test-model").await.unwrap();

        assert_eq!(delivered.lock().unwrap().as_slice(), ["投票メニューを出したよ"]);
        assert_eq!(chat_calls.load(Ordering::SeqCst), 3, "the repeat is generated once, then the turn ends");
        assert!(!result.stopped_by_limit);
        assert!(result.response.is_empty(), "the repeated text is not delivered as the final reply");
        assert!(result.explicit_termination.is_some());
    }

    #[tokio::test]
    async fn speech_then_tool_call_then_speech_still_runs() {
        use std::sync::Mutex;

        let (llm, _) = MockLlm::counting(vec![
            text_response("調べるね"),
            tool_call_response(vec![tc("t-1", "test_tool", serde_json::json!({}))]),
            text_response("結果はXだよ"),
            text_response("NO_REPLY"),
        ]);
        let executor = MockExecutor::new().add_result("test_tool", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_assistant_history_name("らぼみ");
        let delivered = Arc::new(Mutex::new(Vec::new()));
        engine.set_on_continuation_speech(recording_speech_hook(delivered.clone()));

        let history = "<conversation_history>\n[u1]:\n調べて\n</conversation_history>";
        let result = engine.run("system", history, "test-model").await.unwrap();

        assert_eq!(result.iterations, 4);
        assert_eq!(delivered.lock().unwrap().as_slice(), ["調べるね", "結果はXだよ"]);
    }

    #[tokio::test]
    async fn run_without_delivery_hook_stops_repeating_and_keeps_first_text() {
        use std::sync::atomic::Ordering;

        // prod sleep-condense shape: no delivery hook, the model restates its summary forever.
        let (llm, chat_calls) = MockLlm::counting(vec![
            tool_call_response(vec![tc("t-1", "test_tool", serde_json::json!({}))]),
            text_response("m6 と照合しました。新しい凝縮はありません。"),
            text_response("m6 と照合しました。すでに残っています。"),
            text_response("今回の3件は m6 に残っています。"),
        ]);
        let executor = MockExecutor::new().add_result("test_tool", successful_action_result());
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 30);

        let result = engine.run("system", "凝縮して", "test-model").await.unwrap();

        assert_eq!(chat_calls.load(Ordering::SeqCst), 3);
        assert!(!result.stopped_by_limit);
        assert_eq!(result.response, "m6 と照合しました。新しい凝縮はありません。");
    }
