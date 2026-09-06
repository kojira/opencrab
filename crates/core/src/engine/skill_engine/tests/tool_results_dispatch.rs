    /// dispatch 対象ツールは inline 実行（executor）されず、**同ターンで** spawned
    /// マーカーが tool_result として返り、次イテレーションでエージェントが継続すること。
    #[tokio::test]
    async fn test_auto_dispatch_returns_spawned_marker_same_turn() {
        use std::sync::{Arc, Mutex};

        // 1回目: ツール呼び出し（dispatch 対象）。2回目: 最終テキスト。
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc(
                "tc-1",
                "nostr_generate_key",
                serde_json::json!({}),
            )]),
            text_response("鍵の生成を開始しました"),
        ]);
        // executor が呼ばれたら記録する（dispatch 対象は呼ばれてはならない）。
        struct SpyExecutor {
            called: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ActionExecutor for SpyExecutor {
            async fn execute(&self, name: &str, _args: &Value) -> ActionResult {
                self.called.lock().unwrap().push(name.to_string());
                ActionResult {
                    success: true,
                    data: serde_json::json!(null),
                    error: None,
                }
            }
            fn list_tools(&self) -> Vec<FunctionDefinition> {
                vec![]
            }
        }
        let called = Arc::new(Mutex::new(Vec::new()));
        let executor = SpyExecutor {
            called: called.clone(),
        };

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let dispatcher = Arc::new(RecordingDispatcher::new(&[
            "spawn_subtask",
            "report_progress",
            "cancel_subtask",
        ]));
        engine.set_tool_dispatcher(dispatcher.clone());

        // 2回目の LLM 呼び出しが見る messages を記録し、spawned マーカーの再注入を検証する。
        let seen_tool_results: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen_tool_results.clone();
        engine.add_on_tool_result(move |_id, name, json, _err| {
            seen_clone.lock().unwrap().push(format!("{name}:{json}"));
        });

        let result = engine
            .run("system", "鍵を作って", "test-model")
            .await
            .unwrap();

        // dispatch されたので executor は呼ばれない。
        assert!(
            called.lock().unwrap().is_empty(),
            "dispatch 対象ツールは inline executor で実行されてはならない"
        );
        // dispatcher.dispatch が1回呼ばれた。
        assert_eq!(
            dispatcher.dispatched.lock().unwrap().as_slice(),
            &["nostr_generate_key"]
        );
        // tool_result は spawned マーカー（同ターン返却）。
        let seen = seen_tool_results.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("\"status\":\"spawned\""));
        assert!(seen[0].contains("\"subtask_id\":\"sub-for-nostr_generate_key\""));
        // エージェントは自分のターンで継続して最終応答を出す。
        assert_eq!(result.response, "鍵の生成を開始しました");
        assert_eq!(result.iterations, 2);
    }

    /// #284: **巨大なツール結果を生のまま LLM へ返さない。**
    ///
    /// 実事故では 76,661 バイトのフォロー一覧がそのままプロンプトへ積まれ、同ターンの
    /// 会話（ユーザー発言を含む）が押し出された。DB 永続化側には上限があったのに
    /// `messages.push(Message::tool(...))` だけが素通りしていた非対称が原因。
    /// ここでは「LLM が次の呼び出しで実際に見る tool メッセージ」を捕まえて上限内で
    /// あることと、全文の在り処が案内されることを固定する。
    #[tokio::test]
    async fn huge_tool_result_is_capped_before_reaching_the_llm() {
        use std::sync::{Arc, Mutex};

        /// 2 回目の呼び出しで受け取った messages を記録する LLM。
        struct CapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            seen_tool_messages: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                for m in &request.messages {
                    if m.role == Role::Tool {
                        if let Some(MessageContent::Text(t)) = &m.content {
                            self.seen_tool_messages.lock().unwrap().push(t.clone());
                        }
                    }
                }
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let workspace = tempfile::TempDir::new().unwrap();
        let seen_tool_messages: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                text_response("ok"),
            ]),
            seen_tool_messages: seen_tool_messages.clone(),
        };
        // 事故と同規模の結果を返すツール。
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({ "list": "npub1abcdefgh ".repeat(7_000) }),
                error: None,
            },
        );

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_result_offload("sess1", Some(workspace.path().to_path_buf()));
        // DB へ渡る本文（callback）も同じ capped 本文であること。
        let logged: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logged_clone = logged.clone();
        engine.add_on_tool_result(move |_id, _name, json, _err| {
            logged_clone.lock().unwrap().push(json);
        });

        let result = engine
            .run("system", "一覧を見せて", "test-model")
            .await
            .unwrap();
        assert_eq!(result.response, "ok");

        let seen = seen_tool_messages.lock().unwrap();
        let tool_msg = seen
            .first()
            .expect("LLM が tool メッセージを受け取っていない");
        assert!(
            crate::tokens::estimate_tokens(tool_msg)
                < crate::tool_result_log::TOOL_RESULT_TOKEN_LIMIT,
            "LLM へ {} トークンの tool_result が渡っている（上限 {}）",
            crate::tokens::estimate_tokens(tool_msg),
            crate::tool_result_log::TOOL_RESULT_TOKEN_LIMIT
        );
        // #294: 生データは 1 バイトも渡らない（プレビューも無い）。
        assert!(
            !tool_msg.contains("npub1abcdefgh"),
            "生データが LLM へ渡っている: {tool_msg}"
        );
        assert!(
            tool_msg.contains("withheld"),
            "退避の案内が無い: {tool_msg}"
        );
        assert!(
            tool_msg.contains("tmp/sess1-tc-1.json"),
            "全文の在り処が案内されていない: {tool_msg}"
        );
        assert!(tool_msg.contains("lines"), "行数が無い: {tool_msg}");
        assert!(tool_msg.contains("tokens"), "トークン数が無い: {tool_msg}");
        // 全文はワークスペースに残り、エージェントが読める。
        assert!(workspace.path().join("tmp/sess1-tc-1.json").exists());
        // 同ターンで見えた本文と、DB へ渡る本文が一致する（次ターンで内容が変わらない）。
        assert_eq!(
            logged.lock().unwrap().as_slice(),
            std::slice::from_ref(tool_msg)
        );
    }

    /// 退避先が未設定でも上限は効く（sub-engine / 直呼びでも素通りさせない）。
    #[tokio::test]
    async fn tool_result_is_capped_even_without_an_offload_target() {
        use std::sync::{Arc, Mutex};

        struct CapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            seen: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                for m in &request.messages {
                    if m.role == Role::Tool {
                        if let Some(MessageContent::Text(t)) = &m.content {
                            self.seen.lock().unwrap().push(t.clone());
                        }
                    }
                }
                let mut responses = self.responses.lock().unwrap();
                Ok(responses.remove(0))
            }
        }

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                text_response("ok"),
            ]),
            seen: seen.clone(),
        };
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({ "blob": "z".repeat(100_000) }),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.run("system", "やって", "test-model").await.unwrap();

        let seen = seen.lock().unwrap();
        let tool_msg = seen.first().unwrap();
        assert!(
            crate::tokens::estimate_tokens(tool_msg)
                < crate::tool_result_log::TOOL_RESULT_TOKEN_LIMIT
        );
        assert!(tool_msg.contains("could not be saved"));
        // 退避できなくても生データは流さない（#294）。
        assert!(
            !tool_msg.contains("zzz"),
            "生データが流れている: {tool_msg}"
        );
    }

    /// control 系ツール（report_progress 等）は dispatch されず inline 実行される。
    #[tokio::test]
    async fn test_control_tools_not_dispatched() {
        use std::sync::Arc;

        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            text_response("done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
            },
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        // test_tool を control 扱いにして dispatch させない。
        let dispatcher = Arc::new(RecordingDispatcher::new(&["test_tool"]));
        engine.set_tool_dispatcher(dispatcher.clone());

        let result = engine.run("system", "go", "test-model").await.unwrap();
        // dispatch されず inline 実行された（dispatched は空）。
        assert!(dispatcher.dispatched.lock().unwrap().is_empty());
        assert_eq!(result.tool_calls_made, 1);
        assert_eq!(result.response, "done");
    }

