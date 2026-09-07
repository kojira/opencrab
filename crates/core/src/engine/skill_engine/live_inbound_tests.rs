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

    /// `Arc<RecordingLlm>` を `Box<dyn LlmClient>` として engine に渡すための薄い委譲。
    struct LlmHandle(std::sync::Arc<RecordingLlm>);

    #[async_trait]
    impl LlmClient for LlmHandle {
        async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.0.chat(request).await
        }
    }
