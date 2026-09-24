    use crate::authorization::{AuthorizationBoundary, AuthorizationCheck};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn mutation_value(mutation: &str) -> u64 {
        match mutation {
            "revoke" => 0,
            "revision_bump" => 2,
            _ => unreachable!(),
        }
    }

    fn mutating_check(
        state: Arc<AtomicU64>,
        mutation: &str,
        mutate_after: AuthorizationBoundary,
    ) -> AuthorizationCheck {
        let mutated = mutation_value(mutation);
        Arc::new(move |boundary| {
            let current = state.load(Ordering::SeqCst) == 1;
            if boundary == mutate_after && current {
                state.store(mutated, Ordering::SeqCst);
            }
            current
        })
    }

    #[derive(Clone, Copy)]
    enum ContinuationSpeechPath {
        Holding,
        Ordinary,
        LateInbound,
    }

    struct OneLateInbound(Mutex<Option<Vec<String>>>);

    impl LiveInboundSource for OneLateInbound {
        fn poll_new_messages(&self) -> Vec<String> {
            self.0.lock().unwrap().take().unwrap_or_default()
        }
    }

    async fn assert_continuation_speech_path_denied(
        path: ContinuationSpeechPath,
        mutation: &str,
    ) {
        let response = match path {
            ContinuationSpeechPath::Holding => resp(
                Some("blocked speech"),
                vec![tc("tool-1", "test_tool", serde_json::json!({}))],
            ),
            ContinuationSpeechPath::Ordinary => text_response("blocked speech"),
            ContinuationSpeechPath::LateInbound => final_text_response("blocked speech"),
        };
        let state = Arc::new(AtomicU64::new(1));
        let (llm, _) = MockLlm::counting(vec![response]);
        let tool_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("test_tool", successful_action_result())
            .with_call_log(tool_calls.clone());
        let speech_calls = Arc::new(AtomicUsize::new(0));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 2);
        engine.set_authorization_check(mutating_check(
            state,
            mutation,
            AuthorizationBoundary::InitialModelTurn,
        ));
        engine.set_on_continuation_speech({
            let calls = speech_calls.clone();
            Arc::new(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            })
        });
        if matches!(path, ContinuationSpeechPath::LateInbound) {
            engine.set_live_inbound(Arc::new(OneLateInbound(Mutex::new(Some(vec![
                "late message".to_string(),
            ])))));
        }
        assert!(engine.run("system", "input", "model").await.is_err());
        assert_eq!(speech_calls.load(Ordering::SeqCst), 0, "{mutation}");
        assert!(tool_calls.lock().unwrap().is_empty(), "{mutation}");
    }

    #[tokio::test]
    async fn s6_continuation_speech_callbacks_revalidate_after_revoke_and_revision_bump() {
        for mutation in ["revoke", "revision_bump"] {
            assert_continuation_speech_path_denied(ContinuationSpeechPath::Holding, mutation).await;
            assert_continuation_speech_path_denied(ContinuationSpeechPath::Ordinary, mutation).await;
            assert_continuation_speech_path_denied(ContinuationSpeechPath::LateInbound, mutation)
                .await;
        }
    }

    #[tokio::test]
    async fn s6_actual_engine_boundaries_block_model_tool_and_continuation_effects() {
        for mutation in ["revoke", "revision_bump"] {
            // Initial model turn: stale authority is rejected before the real LLM client.
            let state = Arc::new(AtomicU64::new(mutation_value(mutation)));
            let (llm, model_calls) = MockLlm::counting(vec![final_text_response("unused")]);
            let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
            engine.set_authorization_check(Arc::new(move |_| state.load(Ordering::SeqCst) == 1));
            assert!(engine.run("system", "input", "model").await.is_err());
            assert_eq!(model_calls.load(Ordering::SeqCst), 0, "{mutation}:initial");

            // Tool invocation: authority changes after initial admission, before the real executor.
            let state = Arc::new(AtomicU64::new(1));
            let (llm, model_calls) = MockLlm::counting(vec![tool_call_response(vec![tc(
                "tool-1",
                "test_tool",
                serde_json::json!({}),
            )])]);
            let tool_calls = Arc::new(Mutex::new(Vec::new()));
            let executor = MockExecutor::new()
                .add_result("test_tool", successful_action_result())
                .with_call_log(tool_calls.clone());
            let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 2);
            engine.set_authorization_check(mutating_check(
                state,
                mutation,
                AuthorizationBoundary::InitialModelTurn,
            ));
            assert!(engine.run("system", "input", "model").await.is_err());
            assert_eq!(model_calls.load(Ordering::SeqCst), 1, "{mutation}:tool:model");
            assert!(tool_calls.lock().unwrap().is_empty(), "{mutation}:tool");

            // Operation-driven continuation: the generic tool boundary passes, then the
            // operation-specific production check observes the mutation.
            let state = Arc::new(AtomicU64::new(1));
            let (llm, _) = MockLlm::counting(vec![tool_call_response(vec![tc(
                "reply-1",
                "reply",
                serde_json::json!({"text":"blocked"}),
            )])]);
            let tool_calls = Arc::new(Mutex::new(Vec::new()));
            let executor = MockExecutor::new()
                .add_result("reply", successful_action_result())
                .with_call_log(tool_calls.clone());
            let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 2);
            engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
            engine.set_authorization_check(mutating_check(
                state,
                mutation,
                AuthorizationBoundary::ToolInvocation,
            ));
            assert!(engine.run("system", "input", "model").await.is_err());
            assert!(tool_calls.lock().unwrap().is_empty(), "{mutation}:operation");

            // Automatic continuation: authority changes after initial admission. The real
            // continuation callback and second model iteration both remain untouched.
            let state = Arc::new(AtomicU64::new(1));
            let (llm, model_calls) = MockLlm::counting(vec![text_response("blocked speech")]);
            let speech_calls = Arc::new(AtomicUsize::new(0));
            let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
            engine.set_authorization_check(mutating_check(
                state,
                mutation,
                AuthorizationBoundary::InitialModelTurn,
            ));
            engine.set_on_continuation_speech({
                let calls = speech_calls.clone();
                Arc::new(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                })
            });
            assert!(engine.run("system", "input", "model").await.is_err());
            assert_eq!(model_calls.load(Ordering::SeqCst), 1, "{mutation}:automatic:model");
            assert_eq!(speech_calls.load(Ordering::SeqCst), 0, "{mutation}:automatic:speech");

            // Outbound commit: model/automatic checks pass, then authority changes before the
            // immediately following outbound check. The production callback is not invoked.
            let state = Arc::new(AtomicU64::new(1));
            let (llm, _) = MockLlm::counting(vec![text_response("blocked speech")]);
            let speech_calls = Arc::new(AtomicUsize::new(0));
            let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
            engine.set_authorization_check(mutating_check(
                state,
                mutation,
                AuthorizationBoundary::AutomaticContinuation,
            ));
            engine.set_on_continuation_speech({
                let calls = speech_calls.clone();
                Arc::new(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                })
            });
            assert!(engine.run("system", "input", "model").await.is_err());
            assert_eq!(speech_calls.load(Ordering::SeqCst), 0, "{mutation}:outbound");
        }
    }
