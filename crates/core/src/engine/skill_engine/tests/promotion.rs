    // -----------------------------------------------------------------------
    // D-1085 §12: a provisional run waits for its promotion before a gated tool.
    // -----------------------------------------------------------------------

    fn gated_engine(
        responses: Vec<ChatResponse>,
        calls: Arc<std::sync::Mutex<Vec<String>>>,
        gate: &crate::engine::PromotionGate,
    ) -> SkillEngine {
        let executor = MockExecutor::new()
            .add_result("lookup", successful_action_result())
            .add_result("act", successful_action_result())
            .with_call_log(calls);
        let mut engine = SkillEngine::new(Box::new(MockLlm::new(responses)), Box::new(executor), 30);
        engine.set_single_generation(true);
        engine.set_promotion_gate(gate.clone(), ["lookup".to_string()].into_iter().collect());
        engine
    }

    fn lookup_then_act() -> Vec<ChatResponse> {
        vec![
            tool_call_response(vec![tc("c1", "lookup", serde_json::json!({}))]),
            tool_call_response(vec![tc("c2", "act", serde_json::json!({}))]),
            text_response("済んだ。"),
        ]
    }

    async fn wait_for_calls(calls: &Arc<std::sync::Mutex<Vec<String>>>, n: usize) {
        for _ in 0..200 {
            if calls.lock().unwrap().len() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("expected {n} calls, got {:?}", calls.lock().unwrap());
    }

    #[tokio::test]
    async fn promoted_gate_runs_the_gated_tool_once_after_promotion() {
        let gate = crate::engine::PromotionGate::new();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = gated_engine(lookup_then_act(), calls.clone(), &gate);
        let run = tokio::spawn(async move { engine.run("system", "go", "m").await });

        wait_for_calls(&calls, 1).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(*calls.lock().unwrap(), ["lookup"], "read-only runs, the gated tool waits");
        assert!(!run.is_finished());

        gate.promote();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("run ends after promotion")
            .unwrap()
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["lookup", "act"]);
        assert_eq!(result.response, "済んだ。");
    }

    #[tokio::test]
    async fn aborted_gate_never_runs_the_gated_tool() {
        let gate = crate::engine::PromotionGate::new();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = gated_engine(lookup_then_act(), calls.clone(), &gate);
        let run = tokio::spawn(async move { engine.run("system", "go", "m").await });

        wait_for_calls(&calls, 1).await;
        gate.abort();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .expect("run ends after abort")
            .unwrap();
        assert!(result.is_err());
        assert_eq!(*calls.lock().unwrap(), ["lookup"]);
    }

    #[tokio::test]
    async fn a_batch_with_a_gated_tool_runs_nothing_before_promotion() {
        let gate = crate::engine::PromotionGate::new();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = gated_engine(
            vec![
                tool_call_response(vec![
                    tc("c1", "lookup", serde_json::json!({})),
                    tc("c2", "act", serde_json::json!({})),
                ]),
                text_response("済んだ。"),
            ],
            calls.clone(),
            &gate,
        );
        let run = tokio::spawn(async move { engine.run("system", "go", "m").await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(calls.lock().unwrap().is_empty());
        gate.abort();
        assert!(run.await.unwrap().is_err());
        assert!(calls.lock().unwrap().is_empty());
    }
