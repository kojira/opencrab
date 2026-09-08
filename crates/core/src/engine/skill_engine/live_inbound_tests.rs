    use super::*;
    use async_trait::async_trait;
    use opencrab_llm_types::{
        ChatResponse, Choice, FunctionCall, FunctionDefinition, MessageContent, Usage,
    };

    /// LLM へ実際に渡ったリクエストを記録するモック。
    struct RecordingLlm {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
        requests: std::sync::Mutex<Vec<ChatRequest>>,
    }

    impl RecordingLlm {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// n 回目（0 始まり）の呼び出しに載った user ロールの本文。
        fn user_texts(&self, nth: usize) -> Vec<String> {
            let requests = self.requests.lock().unwrap();
            requests[nth]
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .filter_map(|m| match m.content.as_ref() {
                    Some(MessageContent::Text(t)) => Some(t.clone()),
                    _ => None,
                })
                .collect()
        }

        fn call_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl LlmClient for RecordingLlm {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.requests.lock().unwrap().push(request);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("no more mock responses");
            }
            Ok(responses.remove(0))
        }
    }

    struct NoopExecutor;

    #[async_trait]
    impl ActionExecutor for NoopExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            ActionResult {
                success: true,
                data: serde_json::json!("ok"),
                error: None,
            }
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            vec![FunctionDefinition {
                name: "test_tool".to_string(),
                description: Some("A test tool".to_string()),
                parameters: serde_json::json!({}),
            }]
        }
    }

    /// 実装側の契約（前回 poll 以降だけを返す）を再現する source。
    ///
    /// 「まだ配っていない分」を配り切ったら以後は空を返す。本番実装（server 側）は
    /// 同じことを log id の watermark で行う。
    struct ScriptedInbound {
        pending: std::sync::Mutex<Vec<Vec<String>>>,
        polls: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedInbound {
        fn new(batches: Vec<Vec<&str>>) -> Self {
            Self {
                pending: std::sync::Mutex::new(
                    batches
                        .into_iter()
                        .map(|b| b.into_iter().map(str::to_string).collect())
                        .collect(),
                ),
                polls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn polls(&self) -> usize {
            self.polls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl LiveInboundSource for ScriptedInbound {
        fn poll_new_messages(&self) -> Vec<String> {
            self.polls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut pending = self.pending.lock().unwrap();
            if pending.is_empty() {
                Vec::new()
            } else {
                pending.remove(0)
            }
        }
    }

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "test_tool".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn response(text: Option<&str>, calls: Vec<ToolCall>) -> ChatResponse {
        ChatResponse {
            id: String::new(),
            model: String::new(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: text.map(|s| MessageContent::Text(s.to_string())),
                    name: None,
                    function_call: None,
                    tool_calls: if calls.is_empty() { None } else { Some(calls) },
                    tool_call_id: None,
                },
                finish_reason: None,
            }],
            usage: Usage::default(),
            created: 0,
        }
    }

    /// ループ実行中に届いた発言が、**次のイテレーションの入力**に載る。
    ///
    /// これが #289 の本体: 1 回目の LLM 呼び出し時点では入力に無く、ツール往復を挟んだ
    /// 2 回目には載っていること。
    #[tokio::test]
    async fn new_speech_reaches_the_next_iteration() {
        let llm = std::sync::Arc::new(RecordingLlm::new(vec![
            response(None, vec![tool_call("call-1")]),
            response(Some("了解、止めるね"), vec![]),
        ]));
        let source = std::sync::Arc::new(ScriptedInbound::new(vec![vec!["[owner]:\nやめて"]]));

        let mut engine =
            SkillEngine::new(Box::new(LlmHandle(llm.clone())), Box::new(NoopExecutor), 10);
        engine.set_live_inbound(source.clone());
        engine
            .run("system", "作業して", "test-model")
            .await
            .unwrap();

        assert_eq!(llm.call_count(), 2);
        let first = llm.user_texts(0);
        assert!(
            !first.iter().any(|t| t.contains("やめて")),
            "ターン開始時にはまだ届いていない: {first:?}"
        );
        let second = llm.user_texts(1);
        assert!(
            second.iter().any(|t| t.contains("やめて")),
            "走行中の新着が次のイテレーションに載る: {second:?}"
        );
    }

    /// 同じ発言は二度注入されない。
    ///
    /// source は「前回以降」だけを返す契約なので、3 イテレーション回しても該当の本文は
    /// 全リクエストを通じて 1 回しか現れない。毎回足すとプロンプトが際限なく膨らむ。
    #[tokio::test]
    async fn the_same_speech_is_never_injected_twice() {
        let llm = std::sync::Arc::new(RecordingLlm::new(vec![
            response(None, vec![tool_call("call-1")]),
            response(None, vec![tool_call("call-2")]),
            response(Some("done"), vec![]),
        ]));
        let source = std::sync::Arc::new(ScriptedInbound::new(vec![vec!["[owner]:\nやめて"]]));

        let mut engine =
            SkillEngine::new(Box::new(LlmHandle(llm.clone())), Box::new(NoopExecutor), 10);
        engine.set_live_inbound(source.clone());
        engine
            .run("system", "作業して", "test-model")
            .await
            .unwrap();

        assert_eq!(llm.call_count(), 3);
        let occurrences = llm
            .user_texts(2)
            .iter()
            .filter(|t| t.contains("やめて"))
            .count();
        assert_eq!(occurrences, 1, "最終リクエストにも 1 件だけ載る");
    }

    /// 1 回目の LLM 呼び出しの前には poll しない（履歴と二重になるため）。
    #[tokio::test]
    async fn the_first_iteration_does_not_poll() {
        let llm = std::sync::Arc::new(RecordingLlm::new(vec![response(Some("hi"), vec![])]));
        let source = std::sync::Arc::new(ScriptedInbound::new(vec![]));

        let mut engine =
            SkillEngine::new(Box::new(LlmHandle(llm.clone())), Box::new(NoopExecutor), 10);
        engine.set_live_inbound(source.clone());
        engine.run("system", "hi", "test-model").await.unwrap();

        assert_eq!(llm.call_count(), 1);
        assert_eq!(source.polls(), 0, "ツール往復が無ければ引かない");
    }

    /// 新着が無ければ入力は従来と同一（1 バイトも増えない）。
    #[tokio::test]
    async fn no_new_speech_changes_nothing() {
        let script = vec![
            response(None, vec![tool_call("call-1")]),
            response(Some("done"), vec![]),
        ];
        let with_source = std::sync::Arc::new(RecordingLlm::new(script.clone()));
        let without_source = std::sync::Arc::new(RecordingLlm::new(script));

        let mut engine = SkillEngine::new(
            Box::new(LlmHandle(with_source.clone())),
            Box::new(NoopExecutor),
            10,
        );
        engine.set_live_inbound(std::sync::Arc::new(ScriptedInbound::new(vec![])));
        engine.run("system", "go", "test-model").await.unwrap();

        let baseline = SkillEngine::new(
            Box::new(LlmHandle(without_source.clone())),
            Box::new(NoopExecutor),
            10,
        );
        baseline.run("system", "go", "test-model").await.unwrap();

        assert_eq!(
            with_source.user_texts(1),
            without_source.user_texts(1),
            "新着ゼロなら注入口の有無でプロンプトは変わらない"
        );
    }

    /// #964: request 境界と直列性を 1 本の制御可能な LLM で固定する。
    struct BoundaryLlm {
        requests: std::sync::Mutex<Vec<ChatRequest>>,
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        first_entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release_first: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl LlmClient for BoundaryLlm {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            let call = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(request);
                requests.len()
            };
            self.events
                .lock()
                .unwrap()
                .push(format!("llm{call}_invoked"));
            if call == 1 {
                if let Some(tx) = self.first_entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let rx = self.release_first.lock().unwrap().take().unwrap();
                let _ = rx.await;
                self.events
                    .lock()
                    .unwrap()
                    .push("llm1_completed".to_string());
                Ok(response(None, vec![tool_call("call-1")]))
            } else {
                Ok(response(Some("done"), vec![]))
            }
        }
    }

    struct BoundaryExecutor {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ActionExecutor for BoundaryExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            self.events
                .lock()
                .unwrap()
                .push("result1_completed".to_string());
            ActionResult {
                success: true,
                data: serde_json::json!("result-one"),
                error: None,
            }
        }

        fn list_tools(&self) -> Vec<FunctionDefinition> {
            NoopExecutor.list_tools()
        }
    }

    struct BoundaryInbound {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        polls: std::sync::atomic::AtomicUsize,
    }

    impl LiveInboundSource for BoundaryInbound {
        fn poll_new_messages(&self) -> Vec<String> {
            Vec::new()
        }

        fn poll_new_with_origin(&self) -> Vec<crate::FoldedInbound> {
            let poll = self
                .polls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.events
                .lock()
                .unwrap()
                .push("request2_construction_started".to_string());
            if poll == 0 {
                vec![crate::FoldedInbound {
                    text: "[owner]:\nfolded inbound".to_string(),
                    origin: Some("origin-b".to_string()),
                }]
            } else {
                Vec::new()
            }
        }
    }

    #[tokio::test]
    async fn read_notifications_are_at_exact_sequential_request_boundaries() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let llm = std::sync::Arc::new(BoundaryLlm {
            requests: std::sync::Mutex::new(Vec::new()),
            events: events.clone(),
            first_entered: std::sync::Mutex::new(Some(entered_tx)),
            release_first: std::sync::Mutex::new(Some(release_rx)),
        });
        let source = std::sync::Arc::new(BoundaryInbound {
            events: events.clone(),
            polls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut engine = SkillEngine::new(
            Box::new(BoundaryLlmHandle(llm.clone())),
            Box::new(BoundaryExecutor {
                events: events.clone(),
            }),
            10,
        );
        engine.set_live_inbound(source.clone());
        engine.set_initial_read_origin("origin-a".to_string());
        engine.set_on_folded_origin({
            let events = events.clone();
            std::sync::Arc::new(move |origin| {
                let events = events.clone();
                Box::pin(async move {
                    events.lock().unwrap().push(format!("read:{origin}"));
                })
            })
        });

        let run = tokio::spawn(async move { engine.run("system", "initial", "model").await });
        entered_rx.await.unwrap();
        assert_eq!(source.polls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(llm.requests.lock().unwrap().len(), 1);
        assert_eq!(
            events.lock().unwrap().as_slice(),
            ["read:origin-a", "llm1_invoked"],
            "発端 read は request1 の直前、request1 完了までは request2 を構築しない"
        );

        release_tx.send(()).unwrap();
        run.await.unwrap().unwrap();

        let requests = llm.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let second = &requests[1].messages;
        let result_pos = second
            .iter()
            .position(|m| m.role == Role::Tool)
            .expect("result1 in request2");
        let folded_pos = second
            .iter()
            .position(|m| {
                m.role == Role::User
                    && matches!(&m.content, Some(MessageContent::Text(t)) if t.contains("folded inbound"))
            })
            .expect("folded inbound in request2");
        assert!(result_pos < folded_pos, "result1 の後に folded inbound を積む");
        assert!(matches!(
            &second[result_pos].content,
            Some(MessageContent::Text(t)) if t.contains("result-one")
        ));
        drop(requests);

        assert_eq!(
            events.lock().unwrap().as_slice(),
            [
                "read:origin-a",
                "llm1_invoked",
                "llm1_completed",
                "result1_completed",
                "request2_construction_started",
                "read:origin-b",
                "llm2_invoked",
            ],
            "request1 完了→result1→request2 構築→folded read→request2 呼出しの順"
        );
    }

    struct FailingRequestSetupExecutor;

    #[async_trait]
    impl ActionExecutor for FailingRequestSetupExecutor {
        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
            panic!("execute must not be reached")
        }

        fn list_tools(&self) -> Vec<FunctionDefinition> {
            panic!("simulated request setup failure")
        }
    }

    /// DC-964 v0.5 §7(8): origin が pending でも request 構築が完了しなければ read しない。
    #[tokio::test]
    async fn pending_origin_is_not_read_when_request_setup_fails_before_chat() {
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let llm = std::sync::Arc::new(RecordingLlm::new(Vec::new()));
        let mut engine = SkillEngine::new(
            Box::new(LlmHandle(llm.clone())),
            Box::new(FailingRequestSetupExecutor),
            10,
        );
        engine.set_initial_read_origin("origin-pending".to_string());
        engine.set_on_folded_origin({
            let reads = reads.clone();
            std::sync::Arc::new(move |_| {
                let reads = reads.clone();
                Box::pin(async move {
                    reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
            })
        });

        let failed = tokio::spawn(async move { engine.run("system", "initial", "model").await })
            .await
            .expect_err("request setup must fail before chat");
        assert!(failed.is_panic(), "setup failure is the injected panic");
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(llm.call_count(), 0);
    }

    #[tokio::test]
    async fn no_origin_emits_no_read_notification() {
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let llm = std::sync::Arc::new(RecordingLlm::new(vec![response(Some("done"), vec![])]));
        let mut engine = SkillEngine::new(
            Box::new(LlmHandle(llm)),
            Box::new(NoopExecutor),
            10,
        );
        engine.set_on_folded_origin({
            let reads = reads.clone();
            std::sync::Arc::new(move |_| {
                let reads = reads.clone();
                Box::pin(async move {
                    reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
            })
        });
        engine.run("system", "initial", "model").await.unwrap();
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    struct BoundaryLlmHandle(std::sync::Arc<BoundaryLlm>);

    #[async_trait]
    impl LlmClient for BoundaryLlmHandle {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.0.chat(request).await
        }
    }

    /// `Arc<RecordingLlm>` を `Box<dyn LlmClient>` として engine に渡すための薄い委譲。
    struct LlmHandle(std::sync::Arc<RecordingLlm>);

    #[async_trait]
    impl LlmClient for LlmHandle {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.0.chat(request).await
        }
    }
