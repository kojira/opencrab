    /// Build a canonical tool call with JSON arguments (as a value, serialized).
    fn tc(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: serde_json::to_string(&args).unwrap(),
            },
        }
    }

    /// Build a single-choice ChatResponse with optional text and tool calls.
    fn resp(text: Option<&str>, calls: Vec<ToolCall>) -> ChatResponse {
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

    fn text_response(text: &str) -> ChatResponse {
        resp(Some(text), vec![])
    }

    fn tool_call_response(calls: Vec<ToolCall>) -> ChatResponse {
        resp(None, calls)
    }

    struct MockLlm {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MockLlm {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn counting(responses: Vec<ChatResponse>) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    responses: std::sync::Mutex::new(responses),
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("no more mock responses");
            }
            Ok(responses.remove(0))
        }
    }

    struct MockExecutor {
        results: std::collections::HashMap<String, ActionResult>,
        calls: Option<Arc<std::sync::Mutex<Vec<String>>>>,
    }

    impl MockExecutor {
        fn new() -> Self {
            Self {
                results: std::collections::HashMap::new(),
                calls: None,
            }
        }
        fn add_result(mut self, name: &str, result: ActionResult) -> Self {
            self.results.insert(name.to_string(), result);
            self
        }
        fn with_call_log(mut self, calls: Arc<std::sync::Mutex<Vec<String>>>) -> Self {
            self.calls = Some(calls);
            self
        }
    }

    #[async_trait]
    impl ActionExecutor for MockExecutor {
        async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
            if let Some(calls) = &self.calls {
                calls.lock().unwrap().push(name.to_string());
            }
            self.results.get(name).cloned().unwrap_or(ActionResult {
                success: false,
                data: serde_json::json!(null),
                error: Some(format!("Unknown action: {name}")),
            })
        }
        fn list_tools(&self) -> Vec<FunctionDefinition> {
            vec![FunctionDefinition {
                name: "test_tool".to_string(),
                description: Some("A test tool".to_string()),
                parameters: serde_json::json!({}),
            }]
        }
    }

    /// 記録用の最小 `ToolDispatcher`。`should_dispatch` は control 集合以外を真にし、
    /// `dispatch_batch` は inline 実行せずマーカーだけ返す（実処理は起こさない）。
    struct RecordingDispatcher {
        control: std::collections::HashSet<String>,
        /// dispatch されたツール名（バッチごとに 1 エントリ = カンマ連結）。
        dispatched: std::sync::Mutex<Vec<String>>,
        /// `dispatch_batch` の呼び出し回数（= 生成された subtask の本数）。
        batches: std::sync::atomic::AtomicUsize,
    }

    impl RecordingDispatcher {
        fn new(control: &[&str]) -> Self {
            Self {
                control: control.iter().map(|s| s.to_string()).collect(),
                dispatched: std::sync::Mutex::new(Vec::new()),
                batches: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl crate::ToolDispatcher for RecordingDispatcher {
        fn should_dispatch(&self, tool_name: &str) -> bool {
            !self.is_utterance(tool_name) && !self.control.contains(tool_name)
        }
        fn is_utterance(&self, tool_name: &str) -> bool {
            matches!(tool_name, "reply" | "reaction")
        }
        fn dispatch_batch(&self, calls: &[crate::DispatchCall]) -> crate::DispatchOutcome {
            self.batches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let names: Vec<String> = calls.iter().map(|c| c.tool_name.clone()).collect();
            self.dispatched.lock().unwrap().push(names.join(","));
            crate::DispatchOutcome {
                subtask_id: format!("sub-for-{}", names.join("+")),
                label: names.join(", "),
            }
        }
    }

    fn successful_action_result() -> ActionResult {
        ActionResult {
            success: true,
            data: serde_json::json!(null),
            error: None,
        }
    }

