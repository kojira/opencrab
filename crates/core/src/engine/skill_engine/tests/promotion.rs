    // -----------------------------------------------------------------------
    // D-1085 §12: a provisional run stops at a gated tool without running it.
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

    fn stopped(result: &anyhow::Result<crate::EngineResult>) -> bool {
        result.as_ref().is_err_and(|e| {
            e.downcast_ref::<crate::engine::StoppedAtGatedTool>()
                .is_some()
        })
    }

    #[tokio::test]
    async fn pending_gate_stops_at_the_gated_tool_and_aborts() {
        let gate = crate::engine::PromotionGate::new();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = gated_engine(lookup_then_act(), calls.clone(), &gate);
        let result = engine.run("system", "go", "m").await;
        assert!(stopped(&result), "{result:?}");
        assert_eq!(*calls.lock().unwrap(), ["lookup"], "read-only runs, the gated tool does not");
        assert!(!gate.promote(), "a run that stopped is never promoted");
    }

    #[tokio::test]
    async fn promoted_gate_still_stops_at_the_gated_tool() {
        let gate = crate::engine::PromotionGate::new();
        assert!(gate.promote());
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = gated_engine(lookup_then_act(), calls.clone(), &gate);
        let result = engine.run("system", "go", "m").await;
        assert!(stopped(&result), "{result:?}");
        assert_eq!(*calls.lock().unwrap(), ["lookup"]);
    }

    #[tokio::test]
    async fn a_batch_with_a_gated_tool_runs_nothing() {
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
        let result = engine.run("system", "go", "m").await;
        assert!(stopped(&result), "{result:?}");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_only_tools_alone_run_to_the_final_text() {
        let gate = crate::engine::PromotionGate::new();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let engine = gated_engine(
            vec![
                tool_call_response(vec![tc("c1", "lookup", serde_json::json!({}))]),
                text_response("見つけた。"),
            ],
            calls.clone(),
            &gate,
        );
        let result = engine.run("system", "go", "m").await.unwrap();
        assert_eq!(result.response, "見つけた。");
        assert_eq!(*calls.lock().unwrap(), ["lookup"]);
        assert!(gate.promote(), "the gate is still open to promotion");
    }
