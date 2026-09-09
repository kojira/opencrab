mod issue_975_live_tool_completion_requests {
    use super::*;
    use crate::FoldedToolCompletion;
    use std::sync::{Arc, Mutex};

    struct CapturingLlm {
        responses: Mutex<Vec<anyhow::Result<ChatResponse>>>,
        requests: Arc<Mutex<Vec<ChatRequest>>>,
        metered_tokens: Option<(usize, usize)>,
    }

    #[async_trait]
    impl LlmClient for CapturingLlm {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.requests.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("no scripted response")
            }
            responses.remove(0)
        }

        fn measure_request_tokens(
            &self,
            request: &ChatRequest,
        ) -> Option<crate::RequestTokenMeasurement> {
            let (full, omitted) = self.metered_tokens?;
            let wire = serde_json::to_string(request).ok()?;
            Some(crate::RequestTokenMeasurement {
                tokens: if wire.contains("result_omitted:true") {
                    omitted
                } else {
                    full
                },
                capability: crate::RequestTokenMeterCapability::ExactTokenizer,
            })
        }
    }

    struct ScriptedCompletions {
        batches: Mutex<Vec<Vec<FoldedToolCompletion>>>,
        included: Mutex<Vec<Vec<String>>>,
        consumed: Mutex<Vec<Vec<String>>>,
        recovered: Mutex<Option<(String, ChatRequest)>>,
        pending_effect: Mutex<Option<(String, ChatRequest, ChatResponse)>>,
        tool_effects: Mutex<std::collections::HashMap<String, crate::RecoveredToolEffect>>,
        correlations: Mutex<std::collections::HashMap<String, String>>,
        applied: Mutex<Vec<String>>,
    }

    impl ScriptedCompletions {
        fn new(batches: Vec<Vec<FoldedToolCompletion>>) -> Self {
            Self {
                batches: Mutex::new(batches),
                included: Mutex::new(Vec::new()),
                consumed: Mutex::new(Vec::new()),
                recovered: Mutex::new(None),
                pending_effect: Mutex::new(None),
                tool_effects: Mutex::new(std::collections::HashMap::new()),
                correlations: Mutex::new(std::collections::HashMap::new()),
                applied: Mutex::new(Vec::new()),
            }
        }

        fn with_recovered(self, request_id: &str, request: ChatRequest) -> Self {
            *self.recovered.lock().unwrap() = Some((request_id.to_string(), request));
            self
        }

        fn with_pending_effect(
            self,
            request_id: &str,
            request: ChatRequest,
            response: ChatResponse,
        ) -> Self {
            *self.pending_effect.lock().unwrap() =
                Some((request_id.to_string(), request, response));
            self
        }

        fn with_tool_effect(self, tool_call_id: &str, content: &str) -> Self {
            self.tool_effects.lock().unwrap().insert(
                tool_call_id.to_string(),
                crate::RecoveredToolEffect {
                    content: content.to_string(),
                    is_error: false,
                    lifecycle_status: "completed".to_string(),
                },
            );
            self
        }

        fn with_correlation(self, provider_id: &str, short_id: &str) -> Self {
            self.correlations
                .lock()
                .unwrap()
                .insert(provider_id.to_string(), short_id.to_string());
            self
        }
    }

    impl LiveToolCompletionSource for ScriptedCompletions {
        fn poll_tool_completions(&self) -> Vec<FoldedToolCompletion> {
            let mut batches = self.batches.lock().unwrap();
            if batches.is_empty() {
                Vec::new()
            } else {
                batches.remove(0)
            }
        }

        fn recover_pending_effect(
            &self,
        ) -> Result<Option<(String, ChatRequest, ChatResponse)>, String> {
            Ok(self.pending_effect.lock().unwrap().take())
        }

        fn recover_tool_effect(&self, tool_call_id: &str) -> Option<crate::RecoveredToolEffect> {
            self.tool_effects.lock().unwrap().get(tool_call_id).cloned()
        }

        fn resolve_conversation_tool_id(&self, provider_call_id: &str) -> Option<String> {
            self.correlations.lock().unwrap().get(provider_call_id).cloned()
        }

        fn mark_effect_applied(&self, request_id: &str) -> Result<(), String> {
            self.applied.lock().unwrap().push(request_id.to_string());
            Ok(())
        }

        fn recover_included_request(
            &self,
            _event_ids: &[String],
        ) -> Result<Option<(String, ChatRequest)>, String> {
            Ok(self.recovered.lock().unwrap().take())
        }

        fn mark_included(
            &self,
            event_ids: &[String],
            _request_id: &str,
            _request_digest: &str,
            _request_json: &str,
        ) -> Result<(), String> {
            self.included.lock().unwrap().push(event_ids.to_vec());
            Ok(())
        }

        fn mark_consumed(&self, event_ids: &[String], _request_id: &str) -> Result<(), String> {
            self.consumed.lock().unwrap().push(event_ids.to_vec());
            Ok(())
        }
    }

    fn event(id: &str, text: &str) -> FoldedToolCompletion {
        FoldedToolCompletion {
            event_id: id.to_string(),
            text: text.to_string(),
            omitted_text: format!(
                "{} result_omitted:true path:memory_sessions:fixture bytes:{} lines:{}",
                text.lines().next().unwrap_or("[<t?] status:unknown"),
                text.len(),
                text.lines().count()
            ),
        }
    }

    fn non_system_messages(request: &ChatRequest) -> Vec<serde_json::Value> {
        serde_json::to_value(&request.messages[1..])
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    fn configure_completion_budget(engine: &mut SkillEngine) {
        engine.set_model_input_limits(crate::context_budget::ModelInputLimits {
            max_input_tokens: Some(100),
            max_output_tokens: Some(32),
            max_total_tokens: None,
        });
    }

    #[tokio::test]
    async fn reverse_and_equal_time_completion_order_reaches_actual_llm_request_once() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                Ok(text_response("CONTINUE")),
                Ok(text_response("CONTINUE")),
                Ok(text_response("done")),
            ]),
            requests: requests.clone(),
            metered_tokens: Some((50, 50)),
        };
        let source = Arc::new(ScriptedCompletions::new(vec![
            vec![],
            vec![
                event("event-b", "[<t2] status:completed\nsecond finished first"),
                event("event-a", "[<t1] status:completed\nfirst finished second"),
            ],
            // 同じeventが再pollされても会話ログへ二重追記しない。
            vec![
                event("event-b", "[<t2] status:completed\nsecond finished first"),
                event("event-a", "[<t1] status:completed\nfirst finished second"),
            ],
        ]));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 5);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source.clone());

        let result = engine.run("system", "origin", "model").await.unwrap();
        assert_eq!(result.response, "done");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let second = non_system_messages(&requests[1]);
        assert_eq!(
            second,
            vec![
                serde_json::json!({"role": "user", "content": "origin"}),
                serde_json::json!({"role": "user", "content": "[<t2] status:completed\nsecond finished first"}),
                serde_json::json!({"role": "user", "content": "[<t1] status:completed\nfirst finished second"}),
            ],
            "completion到着順を保った会話ログがLLMへ渡る"
        );
        assert_eq!(
            non_system_messages(&requests[2]),
            second,
            "同じeventの再pollでLLM会話ログを二重化しない"
        );
        assert_eq!(
            source.consumed.lock().unwrap().as_slice(),
            &[vec!["event-b".to_string(), "event-a".to_string()]]
        );
    }

    #[tokio::test]
    async fn every_terminal_status_reaches_actual_llm_request_with_its_call_id() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![Ok(text_response("CONTINUE")), Ok(text_response("done"))]),
            requests: requests.clone(),
            metered_tokens: Some((50, 50)),
        };
        let expected = [
            "[<t1] status:completed\nresult",
            "[<t2] status:failed\nexit 1",
            "[<t3] status:timed_out\nafter 30s",
            "[<t4] status:cancelled\nby user",
        ];
        let source = Arc::new(ScriptedCompletions::new(vec![
            vec![],
            expected
                .iter()
                .enumerate()
                .map(|(index, text)| event(&format!("event-{index}"), text))
                .collect(),
        ]));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source);
        engine.run("system", "origin", "model").await.unwrap();

        let requests = requests.lock().unwrap();
        let actual = non_system_messages(&requests[1]);
        assert_eq!(actual.len(), 1 + expected.len());
        for (index, expected_text) in expected.iter().enumerate() {
            assert_eq!(
                actual[index + 1],
                serde_json::json!({"role": "user", "content": expected_text}),
                "terminal statusごとの実LLM会話ログ"
            );
        }
    }

    #[tokio::test]
    async fn final_request_meter_replaces_oversized_completion_with_persistent_reference() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                Ok(text_response("CONTINUE")),
                Ok(text_response("CONTINUE")),
                Ok(text_response("done")),
            ]),
            requests: requests.clone(),
            // 本文入りは上限超過、参照版は上限内というexact meter fixture。
            metered_tokens: Some((101, 50)),
        };
        let source = Arc::new(ScriptedCompletions::new(vec![
            vec![],
            vec![event(
                "event-1",
                &format!("[<t1] status:completed\n{}TAIL", "x".repeat(200)),
            )],
        ]));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);
        engine.set_live_tool_completions(source);
        engine.set_max_output_tokens(32);
        engine.set_model_input_limits(crate::context_budget::ModelInputLimits {
            max_input_tokens: Some(100),
            max_output_tokens: Some(32),
            max_total_tokens: None,
        });
        engine.run("system", "origin", "model").await.unwrap();

        let requests = requests.lock().unwrap();
        for request in [&requests[1], &requests[2]] {
            let wire = serde_json::to_string(&request.messages).unwrap();
            assert!(wire.contains("result_omitted:true"), "{wire}");
            assert!(!wire.contains("TAIL"), "{wire}");
            assert!(wire.contains("xxxxxxxx"), "safe prefix should be retained: {wire}");
        }
    }

    #[tokio::test]
    async fn failed_provider_request_does_not_consume_the_completion_it_received() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                Ok(text_response("CONTINUE")),
                Err(anyhow::anyhow!("provider unavailable")),
            ]),
            requests: requests.clone(),
            metered_tokens: Some((50, 50)),
        };
        let source = Arc::new(ScriptedCompletions::new(vec![
            vec![],
            vec![event(
                "event-1",
                "[<t1] status:completed\nrecover me",
            )],
        ]));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source.clone());
        let durable_events = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let captured = Arc::clone(&durable_events);
        engine.set_durable_exchange_log_callback(move |_log, event_ids, _request_id| {
            captured.lock().unwrap().push(event_ids.to_vec());
            Ok(())
        });
        assert!(engine.run("system", "origin", "model").await.is_err());

        let requests = requests.lock().unwrap();
        assert_eq!(
            non_system_messages(&requests[1]).last(),
            Some(&serde_json::json!({
                "role": "user",
                "content": "[<t1] status:completed\nrecover me",
            })),
            "失敗したproviderへ実際に渡したcompletionを確認する"
        );
        assert_eq!(source.included.lock().unwrap().len(), 1);
        assert!(source.consumed.lock().unwrap().is_empty());
        assert_eq!(
            durable_events.lock().unwrap().as_slice(),
            &[Vec::<String>::new(), Vec::<String>::new()],
            "provider失敗時はdurable writerにもconsume対象を渡さない"
        );
    }

    #[tokio::test]
    async fn synchronous_tool_result_requires_final_request_meter() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![Ok(tool_call_response(vec![tc(
                "provider-call",
                "echo",
                serde_json::json!({"text": "result"}),
            )]))]),
            requests: requests.clone(),
            metered_tokens: None,
        };
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
        configure_completion_budget(&mut engine);
        let error = engine.run("system", "history", "model").await.unwrap_err();
        assert!(error.to_string().contains("certified request token meter"));
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn missing_certified_meter_blocks_provider_before_completion_request() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![Ok(text_response("must not run"))]),
            requests: requests.clone(),
            metered_tokens: None,
        };
        let source = Arc::new(ScriptedCompletions::new(vec![vec![event(
            "event-1",
            "[<t1] status:completed\nbody",
        )]]));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source);
        let error = engine.run("system", "history", "model").await.unwrap_err();
        assert!(error.to_string().contains("certified request token meter"));
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_effect_restart_replays_response_without_provider_call() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(Vec::new()),
            requests: requests.clone(),
            metered_tokens: Some((50, 50)),
        };
        let exact = ChatRequest::new("model", vec![Message::user("persisted request")]);
        let source = Arc::new(ScriptedCompletions::new(vec![Vec::new()]).with_pending_effect(
            "request-crashed-after-response",
            exact,
            text_response("replayed final response"),
        ));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source.clone());
        let result = engine.run("new system", "new history", "model").await.unwrap();

        assert_eq!(result.response, "replayed final response");
        assert!(requests.lock().unwrap().is_empty());
        assert!(
            source.applied.lock().unwrap().is_empty(),
            "final deliveryはouter gatewayの送信ACK前にappliedへ進めない"
        );
    }

    #[tokio::test]
    async fn recovered_tool_response_reuses_persisted_result_without_reexecution() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![Ok(text_response("done after recovered tool"))]),
            requests,
            metered_tokens: Some((50, 50)),
        };
        let exact = ChatRequest::new("model", vec![Message::user("persisted request")]);
        let response = tool_call_response(vec![tc(
            "provider-call-abc",
            "test_tool",
            serde_json::json!({}),
        )]);
        let source = Arc::new(
            ScriptedCompletions::new(vec![Vec::new()])
                .with_pending_effect("request-with-tool", exact, response)
                .with_correlation("provider-call-abc", "t7")
                .with_tool_effect("t7", r#"{"value":"already applied"}"#),
        );
        let executor = MockExecutor::new().with_call_log(calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 2);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source.clone());
        engine.set_next_tool_sequence(8);

        let result = engine.run("system", "history", "model").await.unwrap();

        assert_eq!(result.response, "done after recovered tool");
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            source.applied.lock().unwrap().as_slice(),
            &["request-with-tool".to_string()]
        );
    }

    #[tokio::test]
    async fn included_restart_reuses_exact_persisted_request() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![Ok(text_response("done"))]),
            requests: requests.clone(),
            metered_tokens: Some((50, 50)),
        };
        let exact = ChatRequest::new(
            "model",
            vec![Message::user("persisted exact request—not rebuilt")],
        );
        let source = Arc::new(
            ScriptedCompletions::new(vec![vec![event(
                "event-restart",
                "[<t9] status:completed\nresult",
            )]])
            .with_recovered("request-before-crash", exact.clone()),
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source.clone());
        engine.run("new system", "new history", "model").await.unwrap();

        let sent = requests.lock().unwrap();
        assert_eq!(
            serde_json::to_value(&sent[0]).unwrap(),
            serde_json::to_value(exact).unwrap()
        );
        assert!(source.included.lock().unwrap().is_empty());
        assert_eq!(
            source.consumed.lock().unwrap().as_slice(),
            &[vec!["event-restart".to_string()]]
        );
    }

    #[tokio::test]
    async fn completion_queued_before_resume_is_included_and_consumed_in_first_request() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![Ok(text_response("done"))]),
            requests: requests.clone(),
            metered_tokens: Some((50, 50)),
        };
        let source = Arc::new(ScriptedCompletions::new(vec![vec![event(
            "event-before-resume",
            "[<t2] status:completed\nbatched result",
        )]]));
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 2);
        configure_completion_budget(&mut engine);
        engine.set_live_tool_completions(source.clone());
        engine.run("system", "existing history", "model").await.unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            non_system_messages(&requests[0]).last(),
            Some(&serde_json::json!({
                "role": "user",
                "content": "[<t2] status:completed\nbatched result",
            }))
        );
        assert_eq!(
            source.consumed.lock().unwrap().as_slice(),
            &[vec!["event-before-resume".to_string()]]
        );
    }
}
