    // ---- RFC #152 S3a: 自動 dispatch（非ブロック / 全ツール subtask 化） ----

    /// #880: 複数 reply は 1 生成に並べた分をすべて配送し、ack 往復を起こさない。
    #[tokio::test]
    async fn utterance_reply_batch_completes_in_one_llm_call() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let (llm, chat_calls) = MockLlm::counting(vec![tool_call_response(vec![
            tc("reply-1", "reply", serde_json::json!({"text": "one"})),
            tc("reply-2", "reply", serde_json::json!({"text": "two"})),
            tc("reply-3", "reply", serde_json::json!({"text": "three"})),
        ])]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        let tool_results = Arc::new(Mutex::new(Vec::new()));
        let seen_results = tool_results.clone();
        engine.add_on_tool_result(move |_id, _name, json, _err| {
            seen_results.lock().unwrap().push(json);
        });

        let result = engine
            .run("system", "3回に分けて返信して", "test-model")
            .await;

        let result = result.expect("純発話生成は空の resume 応答なしで完了する");
        assert_eq!(
            executor_calls.lock().unwrap().as_slice(),
            &["reply", "reply", "reply"]
        );
        assert_eq!(
            chat_calls.load(Ordering::SeqCst),
            1,
            "reply×3 は ack を積んで LLM を呼び直さない"
        );
        assert_eq!(result.iterations, 1);
        assert_eq!(result.tool_calls_made, 3);
        assert_eq!(
            result.last_posting_utterance_id.as_deref(),
            Some("reply-3"),
            "最終生成の最後の posting utterance call_id を surface する"
        );
        assert!(
            tool_results.lock().unwrap().is_empty(),
            "純発話の最小 ack は on_tool_result に流さない"
        );
    }

    /// #880: reply と通常 content が同居しても、reply 配送後に content を最終応答として返す。
    #[tokio::test]
    async fn utterance_reply_with_content_completes_in_one_llm_call_without_machine_hooks() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        const CONTENT: &str = "通常本文も同じ生成で返す";
        let (llm, chat_calls) = MockLlm::counting(vec![resp(
            Some(CONTENT),
            vec![tc(
                "reply-1",
                "reply",
                serde_json::json!({"text": "返信本文"}),
            )],
        )]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        let tool_results = Arc::new(Mutex::new(Vec::new()));
        let seen_results = tool_results.clone();
        engine.add_on_tool_result(move |_id, _name, json, _err| {
            seen_results.lock().unwrap().push(json);
        });
        let tool_calls = Arc::new(Mutex::new(Vec::new()));
        let seen_calls = tool_calls.clone();
        engine.add_on_tool_call(move |content, json| {
            seen_calls.lock().unwrap().push((content, json));
        });

        let result = engine
            .run("system", "返信して本文も添えて", "test-model")
            .await
            .expect("純発話生成は完了する");

        assert_eq!(executor_calls.lock().unwrap().as_slice(), &["reply"]);
        assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.iterations, 1);
        assert_eq!(result.response, CONTENT);
        assert!(tool_results.lock().unwrap().is_empty());
        assert!(
            tool_calls.lock().unwrap().is_empty(),
            "純発話は空 calls_json の機械行も残さない"
        );
    }

    /// #880: 照会が混在すると次の LLM 呼び出しが必要なので、発話にも最小 ack を対で積む。
    #[tokio::test]
    async fn utterance_reply_mixed_with_resolve_keeps_ack_and_second_llm_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        struct CapturingCountingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            calls: Arc<AtomicUsize>,
            requests: Arc<Mutex<Vec<ChatRequest>>>,
        }

        #[async_trait]
        impl LlmClient for CapturingCountingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.requests.lock().unwrap().push(request);
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let chat_calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingCountingLlm {
            responses: Mutex::new(vec![
                tool_call_response(vec![
                    tc("reply-1", "reply", serde_json::json!({"text": "返信本文"})),
                    tc("resolve-1", "resolve", serde_json::json!({"ref": "e1"})),
                ]),
                text_response("照会を開始しました"),
            ]),
            calls: chat_calls.clone(),
            requests: requests.clone(),
        };
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let dispatcher = Arc::new(RecordingDispatcher::new(&[]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let result = engine
            .run("system", "返信してから全文を見て", "test-model")
            .await
            .expect("混在生成は tool_result を読んで完了する");

        assert_eq!(executor_calls.lock().unwrap().as_slice(), &["reply"]);
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["resolve"]
        );
        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(result.iterations, 2);
        let requests = requests.lock().unwrap();
        let second_messages = &requests[1].messages;
        assert!(
            second_messages.iter().any(|message| {
                message.role == Role::Tool
                    && message.tool_call_id.as_deref() == Some("reply-1")
                    && message.text_content() == Some("{}")
            }),
            "混在時は reply の最小 ack {{}} を次の LLM 呼び出しへ積む"
        );
        assert!(
            second_messages.iter().any(|message| {
                message.role == Role::Tool
                    && message.tool_call_id.as_deref() == Some("resolve-1")
                    && message
                        .text_content()
                        .is_some_and(|text| text.contains("\"status\":\"spawned\""))
            }),
            "resolve は従来どおり spawned マーカーを次の LLM 呼び出しへ積む"
        );
    }

