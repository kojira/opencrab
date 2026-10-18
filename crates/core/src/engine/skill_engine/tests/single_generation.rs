    // -----------------------------------------------------------------------
    // D-1085: single generation. The first generation without tool calls ends the run.
    // -----------------------------------------------------------------------

    fn recording_continuation(
        engine: &mut SkillEngine,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        let delivered = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        engine.set_on_continuation_speech({
            let delivered = delivered.clone();
            std::sync::Arc::new(move |text| {
                let delivered = delivered.clone();
                Box::pin(async move {
                    delivered.lock().unwrap().push(text);
                    Ok(())
                })
            })
        });
        delivered
    }

    #[tokio::test]
    async fn single_generation_text_without_marker_is_the_final_response() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("了解。"),
            text_response("二回目\nNO_REPLY"),
        ]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 30);
        engine.set_single_generation(true);
        let delivered = recording_continuation(&mut engine);

        let result = engine
            .run("system", "了解って言って", "test-model")
            .await
            .expect("single generation ends");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.iterations, 1);
        assert_eq!(result.response, "了解。");
        assert!(delivered.lock().unwrap().is_empty(), "final text is not continuation speech");
        assert!(!result.last_generation_had_continuation_speech);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn single_generation_bare_no_reply_is_silent() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("NO_REPLY"),
            text_response("二回目\nNO_REPLY"),
        ]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 30);
        engine.set_single_generation(true);
        let delivered = recording_continuation(&mut engine);

        let result = engine
            .run("system", "相槌だけ", "test-model")
            .await
            .expect("single generation ends");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.response, "");
        assert_eq!(
            result.explicit_termination,
            Some(crate::engine::ExplicitTermination::NoReply)
        );
        assert!(delivered.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn single_generation_strips_a_trailing_marker() {
        let (llm, _) = MockLlm::counting(vec![text_response("うん、いいよ。\nNO_REPLY")]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 30);
        engine.set_single_generation(true);

        let result = engine.run("system", "いい？", "test-model").await.unwrap();

        assert_eq!(result.iterations, 1);
        assert_eq!(result.response, "うん、いいよ。");
    }
