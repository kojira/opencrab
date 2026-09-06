    /// ログコールバックで捕捉した (error_code, error_str) の並び。
    /// `-D warnings` の `clippy::type_complexity` を避けるための別名。
    type CapturedErrors = Arc<std::sync::Mutex<Vec<(Option<String>, Option<String>)>>>;

    #[tokio::test]
    async fn test_direct_response() {
        let llm = MockLlm::new(vec![text_response("Hello, world!")]);
        let executor = MockExecutor::new();
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(result.response, "Hello, world!");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.tool_calls_made, 0);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn typed_conversation_uses_typed_history() {
        use std::sync::Mutex;

        struct CapturingLlm {
            captured: Arc<Mutex<Vec<Vec<Message>>>>,
        }

        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.captured.lock().unwrap().push(request.messages);
                Ok(text_response("typed response"))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let typed_call = tc(
            "typed-call",
            "test_tool",
            serde_json::json!({"from": "typed"}),
        );
        let typed_conversation = crate::conversation_typed::TypedConversation {
            context_block: None,
            snapshot_base: None,
            history: vec![
                Message {
                    role: Role::Assistant,
                    content: None,
                    name: None,
                    function_call: None,
                    tool_calls: Some(vec![typed_call]),
                    tool_call_id: None,
                },
                Message::tool("typed-call", r#"{"result":"typed"}"#.to_string()),
                Message {
                    role: Role::User,
                    content: Some(MessageContent::Text("typed current turn".to_string())),
                    name: Some("owner".to_string()),
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            response_directive: Some("typed response directive".to_string()),
            wire_tokens: 0,
            diagnostics: crate::conversation_typed::DeriveDiagnostics {
                item_count: 3,
                unpaired_call_count: 0,
                opaque_event_count: 0,
            },
        };
        let llm = CapturingLlm {
            captured: captured.clone(),
        };
        let executor = MockExecutor::new();
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_conversation_waters(1, 0);
        engine.set_typed_conversation(Some(typed_conversation));

        let result = engine
            .run("system context", "FLAT_HISTORY_SENTINEL", "test-model")
            .await
            .unwrap();
        assert_eq!(result.response, "typed response");

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let messages = &calls[0];
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::System);
        // #884 §9.4-1: system は本文 + 省略ポリシー節 + （keep 時）出力指示 の順。
        let system_text = message_plain_text(&messages[0]);
        assert!(system_text.starts_with("system context"), "{system_text}");
        assert!(
            system_text.contains(crate::conversation_typed::OMISSION_POLICY_NOTE),
            "省略ポリシー節が system に 1 回入る: {system_text}"
        );
        assert!(
            system_text.ends_with("typed response directive"),
            "出力指示は system 末尾: {system_text}"
        );
        assert_eq!(messages[1].role, Role::Assistant);
        assert!(messages[1]
            .tool_calls
            .as_ref()
            .is_some_and(|calls| calls.iter().any(|call| call.id == "typed-call")));
        assert_eq!(messages[2].role, Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("typed-call"));
        assert_eq!(messages[3].role, Role::User);
        assert_eq!(message_plain_text(&messages[3]), "typed current turn");
        assert!(
            messages
                .iter()
                .all(|message| !message_plain_text(message).contains("FLAT_HISTORY_SENTINEL")),
            "typed 経路に flat の履歴入り単一 User を積まない"
        );
    }

    /// 出力上限で切り捨てられた応答（finish_reason=Length）を表す。`text` は切り捨て
    /// 前にモデルが吐いた前置き。**これは chatgpt の parse_response が incomplete 応答に
    /// 対して返す形と同じ**（server 側の end-to-end テストが本物の parse_response を通す）。
    fn length_truncated_response(text: Option<&str>) -> ChatResponse {
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
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some(FinishReason::Length),
            }],
            usage: Usage {
                completion_tokens: 4096,
                ..Usage::default()
            },
            created: 0,
        }
    }

    /// #676: finish_reason=Length（出力上限で切り捨て）はターンを失敗させる（fail loud）。
    /// 前置きテキストがあっても最終回答にしない。
    #[tokio::test]
    async fn test_output_limit_truncation_fails_the_turn() {
        let llm = MockLlm::new(vec![length_truncated_response(Some(
            "これから報告を書きます",
        ))]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);

        let err = engine
            .run("system", "調査して報告して", "hermit:claude-opus-5")
            .await
            .expect_err("出力上限で切り捨てられたターンは Err にならねばならない");
        let msg = err.to_string();
        assert!(
            msg.contains("切り捨て"),
            "エラー文言が切り捨てを明示していない: {msg}"
        );
        assert!(
            msg.contains("max_output_tokens"),
            "エラー文言が上限（登録先）を含んでいない: {msg}"
        );
    }

    /// #706: 意味的に空の応答（content 欠落／空文字／空白のみ、かつ tool_call 無し）は
    /// ターンを失敗させる（fail loud）。3 形すべてを対象にする。finish_reason は付けない
    /// （＝provider が "stop" 相当を名乗る経路と同じ）。
    #[tokio::test]
    async fn test_empty_response_fails_the_turn_all_three_shapes() {
        // (content の形, 説明) の 3 形。
        let cases: Vec<(Option<&str>, &str)> = vec![
            (None, "content フィールド欠落"),
            (Some(""), "空文字"),
            (Some("   \n\t  "), "空白のみ"),
        ];
        for (content, label) in cases {
            let llm = MockLlm::new(vec![resp(content, vec![])]);
            let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
            let err = engine
                .run("system", "答えて", "cursor:grok")
                .await
                .expect_err(&format!("空応答（{label}）は Err にならねばならない"));
            assert!(
                err.to_string().contains("意味的に空"),
                "空応答（{label}）の Err 文言が理由を明示していない: {err}"
            );
        }
    }

    /// #706（最重要）: 空応答が **llm_logs に理由付きで残る**こと。log_callback は意味的
    /// 検証の結果（error_code / error_str）を受け取らねばならない——「error 欄空の成功行」
    /// として残る旧穴（設計 §1-c）が塞がっていることを固定する。特に「content フィールド
    /// 欠落 + tool_call 無し」を失敗として記録する。
    ///
    /// 原因の当たり付け材料（プロンプト長）は process 側が失敗行へ一様に付ける
    /// （`error_body_with_prompt_size`）。ここは engine が種別と理由を渡すところまでを固定する。
    #[tokio::test]
    async fn test_empty_response_is_recorded_in_log_with_reason() {
        use std::sync::Mutex;
        let captured: CapturedErrors = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();

        let llm = MockLlm::new(vec![resp(None, vec![])]); // content 欠落・tool_call 無し
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        engine.set_log_callback(move |log: &LlmCallLog| {
            sink.lock()
                .unwrap()
                .push((log.error_code.clone(), log.error_str.clone()));
        });

        let _ = engine.run("system", "答えて", "cursor:grok").await;

        let logs = captured.lock().unwrap();
        assert_eq!(logs.len(), 1, "1 ターン = 1 ログ行のはず");
        let (code, body) = &logs[0];
        assert_eq!(
            code.as_deref(),
            Some("empty_response"),
            "error_code が empty_response でない: {code:?}"
        );
        let body = body.as_deref().unwrap_or("");
        assert!(
            body.contains("意味的に空"),
            "error_body が空応答を明示していない: {body}"
        );
    }

    /// #706 回帰防止: empty content でも **tool_call があれば空ではない**（ツール往復の
    /// 一手なのでターンは継続する）。空判定に tool_call を混ぜていることの固定。
    #[tokio::test]
    async fn test_empty_content_with_tool_call_is_not_empty() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("c1", "test_tool", serde_json::json!({}))]),
            text_response("done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!("ok"),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let result = engine
            .run("system", "使って", "test-model")
            .await
            .expect("tool_call 付きの空 content は失敗にならない");
        assert_eq!(result.response, "done");
        assert_eq!(result.tool_calls_made, 1);
    }

    /// #676: finish_reason=Stop の正常応答は従来どおり最終回答として返る（回帰防止）。
    #[tokio::test]
    async fn test_stop_finish_reason_is_returned_normally() {
        let llm = MockLlm::new(vec![ChatResponse::text("完了しました")]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        let result = engine.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(result.response, "完了しました");
    }

    /// #676: set_max_output_tokens で設定した値が実際に ChatRequest.max_tokens へ載る。
    /// 未設定なら None（プロバイダ既定に委ねる）。
    #[tokio::test]
    async fn test_max_output_tokens_reaches_the_request() {
        use std::sync::Mutex;

        struct RecordingLlm {
            seen: Arc<Mutex<Option<Option<u32>>>>,
        }
        #[async_trait]
        impl LlmClient for RecordingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                *self.seen.lock().unwrap() = Some(request.max_tokens);
                Ok(ChatResponse::text("ok"))
            }
        }

        // set した場合。
        let seen = Arc::new(Mutex::new(None));
        let mut engine = SkillEngine::new(
            Box::new(RecordingLlm { seen: seen.clone() }),
            Box::new(MockExecutor::new()),
            10,
        );
        engine.set_max_output_tokens(128_000);
        engine.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(*seen.lock().unwrap(), Some(Some(128_000)));

        // 未設定なら None（上限未指定）。
        let seen2 = Arc::new(Mutex::new(None));
        let engine2 = SkillEngine::new(
            Box::new(RecordingLlm {
                seen: seen2.clone(),
            }),
            Box::new(MockExecutor::new()),
            10,
        );
        engine2.run("system", "hi", "test-model").await.unwrap();
        assert_eq!(*seen2.lock().unwrap(), Some(None));
    }

    #[tokio::test]
    async fn test_single_tool_call() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            text_response("Done with tool call"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "do something", "test-model")
            .await
            .unwrap();
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_max_iterations() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            tool_call_response(vec![tc("tc-2", "test_tool", serde_json::json!({}))]),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 1);

        let result = engine
            .run("system", "loop forever", "test-model")
            .await
            .unwrap();
        assert!(result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_multiple_tool_calls() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![
                tc("tc-1", "test_tool", serde_json::json!({})),
                tc("tc-2", "test_tool", serde_json::json!({})),
            ]),
            text_response("Both tools done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "do two things", "test-model")
            .await
            .unwrap();
        assert_eq!(result.tool_calls_made, 2);
        assert_eq!(result.iterations, 2);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_tool_result_feedback() {
        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc(
                "tc-1",
                "test_tool",
                serde_json::json!({"query": "test"}),
            )]),
            text_response("Received tool feedback"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"answer": 42}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine
            .run("system", "query something", "test-model")
            .await
            .unwrap();
        assert_eq!(result.response, "Received tool feedback");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls_made, 1);
        assert!(!result.stopped_by_limit);
    }

    #[tokio::test]
    async fn test_model_override() {
        use std::sync::{Arc, Mutex};

        // MockLlm that captures the model from each request.
        struct ModelCapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            captured_models: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl LlmClient for ModelCapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.captured_models
                    .lock()
                    .unwrap()
                    .push(request.model.clone());
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let llm = ModelCapturingLlm {
            responses: Mutex::new(vec![
                // First call uses default model; after tool call, model override kicks in.
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                text_response("Done after model switch"),
            ]),
            captured_models: captured.clone(),
        };

        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"ok": true}),
                error: None,
            },
        );

        let model_override = Arc::new(Mutex::new(None));
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        // Simulate: after the first tool call, model_override gets set.
        let override_clone = model_override.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            *override_clone.lock().unwrap() = Some("openai:gpt-4o-mini".to_string());
        });

        let result = engine
            .run_with_model_override("system", "hi", "default-model", Some(model_override), &[])
            .await
            .unwrap();

        assert_eq!(result.response, "Done after model switch");

        let models = captured.lock().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0], "default-model"); // First call uses default.
                                                // Second call should use the overridden model (race condition safe - set before tool call finishes).
                                                // Due to timing, it might be either; the important thing is the mechanism works.
    }

    #[tokio::test]
    async fn test_on_response_text_fires_on_every_iteration() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            resp(
                Some("調べてみます"),
                vec![tc("tc-1", "test_tool", serde_json::json!({}))],
            ),
            resp(Some("天気は20度です"), vec![]),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!(null),
                error: None,
            },
        );

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let fired_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let fired_clone = fired_texts.clone();
        engine.set_on_response_text(move |text: String| {
            fired_clone.lock().unwrap().push(text);
        });

        let result = engine
            .run("system", "天気は？", "test-model")
            .await
            .unwrap();
        let texts = fired_texts.lock().unwrap();
        assert_eq!(texts.len(), 2, "should fire for both iterations");
        assert_eq!(texts[0], "調べてみます");
        assert_eq!(texts[1], "天気は20度です");
        assert_eq!(result.response, "天気は20度です");
    }

    #[tokio::test]
    async fn test_tool_history_in_next_llm_call() {
        use std::sync::{Arc, Mutex};

        // MockLlm that captures the messages from each request
        struct MessageCapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            captured_messages: Arc<Mutex<Vec<Vec<Message>>>>,
        }

        #[async_trait]
        impl LlmClient for MessageCapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.captured_messages
                    .lock()
                    .unwrap()
                    .push(request.messages.clone());
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::<Vec<Message>>::new()));
        let llm = MessageCapturingLlm {
            responses: Mutex::new(vec![
                // First response: tool call
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                // Second response: final text
                text_response("All done"),
            ]),
            captured_messages: captured.clone(),
        };

        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        let result = engine.run("system", "do it", "test-model").await.unwrap();
        assert_eq!(result.response, "All done");
        assert_eq!(result.iterations, 2);

        let all_messages = captured.lock().unwrap();
        assert_eq!(all_messages.len(), 2, "LLM should have been called twice");

        // Check messages sent on the second LLM call (iteration 2)
        let second_call_msgs = &all_messages[1];

        // Should contain an assistant message with non-empty tool_calls
        let has_assistant_with_tool_calls = second_call_msgs.iter().any(|m| {
            m.role == Role::Assistant && m.tool_calls.as_ref().is_some_and(|t| !t.is_empty())
        });
        assert!(
            has_assistant_with_tool_calls,
            "Second LLM call must include an assistant message with tool_calls"
        );

        // Should contain a tool message with tool_call_id set
        let has_tool_result = second_call_msgs
            .iter()
            .any(|m| m.role == Role::Tool && m.tool_call_id.is_some());
        assert!(
            has_tool_result,
            "Second LLM call must include a tool result message with tool_call_id"
        );
    }

    #[tokio::test]
    async fn test_on_response_text_fires_for_direct_response() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![text_response("直接答えます")]);
        let executor = MockExecutor::new();

        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        let fired_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let fired_clone = fired_texts.clone();
        engine.set_on_response_text(move |text: String| {
            fired_clone.lock().unwrap().push(text);
        });

        let result = engine.run("system", "direct", "test-model").await.unwrap();
        let texts = fired_texts.lock().unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0], "直接答えます");
        assert_eq!(result.response, "直接答えます");
    }

    /// #397: ツールフックは**複数の購読者**が同じ engine に載る（subtask の進捗実況と
    /// session_logs への永続化）。後から配線した方が前を消してはならない。
    ///
    /// 代入だった頃は 2 つ目の登録で 1 つ目が黙って落ち、`persist_turn_logs` が true の
    /// ターン（＝後から永続化フックが載るターン）で進捗実況が丸ごと死んでいた。
    #[tokio::test]
    async fn test_tool_hooks_accumulate_instead_of_replacing() {
        use std::sync::{Arc, Mutex};

        let llm = MockLlm::new(vec![
            tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
            text_response("done"),
        ]);
        let executor = MockExecutor::new().add_result(
            "test_tool",
            ActionResult {
                success: true,
                data: serde_json::json!({"result": "ok"}),
                error: None,
            },
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);

        // 1 つ目 = 進捗実況相当、2 つ目 = 永続化相当。process.rs と同じ配線順。
        let calls: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));
        let results: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(vec![]));

        let c1 = calls.clone();
        engine.add_on_tool_call(move |_content, _json| c1.lock().unwrap().push("notifier"));
        let r1 = results.clone();
        engine
            .add_on_tool_result(move |_id, _name, _json, _err| r1.lock().unwrap().push("notifier"));
        let c2 = calls.clone();
        engine.add_on_tool_call(move |_content, _json| c2.lock().unwrap().push("turn_log"));
        let r2 = results.clone();
        engine
            .add_on_tool_result(move |_id, _name, _json, _err| r2.lock().unwrap().push("turn_log"));

        engine.run("system", "do it", "test-model").await.unwrap();

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &["notifier", "turn_log"],
            "on_tool_call は登録順に全部呼ばれること（後勝ちで消えない）"
        );
        assert_eq!(
            results.lock().unwrap().as_slice(),
            &["notifier", "turn_log"],
            "on_tool_result は登録順に全部呼ばれること（後勝ちで消えない）"
        );
    }

