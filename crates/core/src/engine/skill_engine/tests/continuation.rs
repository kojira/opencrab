    fn assert_one_conversation_boundary(messages: &[Message]) {
        let starts = messages
            .iter()
            .map(message_plain_text)
            .map(|text| text.matches("<conversation_history>").count())
            .sum::<usize>();
        let ends = messages
            .iter()
            .map(message_plain_text)
            .map(|text| text.matches("</conversation_history>").count())
            .sum::<usize>();
        assert_eq!((starts, ends), (1, 1), "{messages:?}");
    }

    fn assert_canonical_assistant_speech(history: &str, name: &str, speech: &str) {
        let speech_at = history.find(speech).expect("speech is present");
        let header = history[..speech_at]
            .lines()
            .last()
            .expect("speech has a canonical header");
        let prefix = format!("[{name}][");
        assert!(header.starts_with(&prefix) && header.ends_with("]:"), "{header}");
        let timestamp = &header[prefix.len()..header.len() - 2];
        chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%d %H:%M:%S")
            .expect("canonical assistant timestamp");
    }

    // -----------------------------------------------------------------------
    // 明示終端: NO_REPLY が出るまで既定継続する。
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn explicit_termination_plain_speech_without_no_reply_runs_next_call() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("今から調べる"),
            text_response("調査結果はX\nNO_REPLY"),
        ]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "調べて", "test-model")
            .await
            .expect("NO_REPLY まで継続する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(result.iterations, 2);
        assert_eq!(result.response, "調査結果はX");
    }

    /// Production regression: a visible no-tool reply without `NO_REPLY` must become part of
    /// the one bounded conversation history before the next request. Carrying it as a standalone
    /// assistant message made the model repeat the same visible reply and delivered it twice.
    #[tokio::test]
    async fn continuation_speech_rebuilds_bounded_history_without_duplicate_delivery() {
        use std::sync::{Arc, Mutex};

        struct BoundarySensitiveLlm {
            requests: Arc<Mutex<Vec<Vec<Message>>>>,
            calls: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl LlmClient for BoundarySensitiveLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let has_standalone_assistant = request
                    .messages
                    .iter()
                    .any(|message| message.role == Role::Assistant);
                self.requests.lock().unwrap().push(request.messages);
                Ok(match call {
                    0 => text_response("生きてるよ〜！"),
                    1 if has_standalone_assistant => text_response("生きてるよ〜！"),
                    _ => text_response("NO_REPLY"),
                })
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let llm = BoundarySensitiveLlm {
            requests: requests.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 4);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech({
            let delivered = delivered.clone();
            Arc::new(move |text| {
                let delivered = delivered.clone();
                Box::pin(async move {
                    delivered.lock().unwrap().push(text);
                    Ok(())
                })
            })
        });

        let history = "<conversation_history>\n[u1][2026-09-21 13:24:59]:\nらぼみちゃんいきてる？\n</conversation_history>";
        let result = engine
            .run("system", history, "test-model")
            .await
            .expect("bounded continuation should terminate without repeating speech");

        assert_eq!(result.iterations, 2);
        assert_eq!(delivered.lock().unwrap().as_slice(), ["生きてるよ〜！"]);

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let first_user_text = requests[0]
            .iter()
            .find(|message| message.role == Role::User)
            .map(message_plain_text)
            .expect("first request has bounded user history");
        assert_eq!(first_user_text.matches("<conversation_history>").count(), 1);
        assert_eq!(first_user_text.matches("</conversation_history>").count(), 1);
        assert_eq!(first_user_text.matches("らぼみちゃんいきてる？").count(), 1);
        assert_eq!(first_user_text.matches("生きてるよ〜！").count(), 0);

        let second = &requests[1];
        assert_eq!(
            second.iter().filter(|message| message.role == Role::User).count(),
            1
        );
        assert!(
            second
                .iter()
                .all(|message| message.role != Role::Assistant),
            "prior visible speech must not remain outside the bounded history: {second:?}"
        );
        let user_text = second
            .iter()
            .find(|message| message.role == Role::User)
            .map(message_plain_text)
            .expect("second request has bounded user history");
        assert_eq!(user_text.matches("<conversation_history>").count(), 1);
        assert_eq!(user_text.matches("</conversation_history>").count(), 1);
        assert_eq!(user_text.matches("らぼみちゃんいきてる？").count(), 1);
        assert_eq!(user_text.matches("生きてるよ〜！").count(), 1);
        assert_eq!(user_text.matches("[u1][2026-09-21 13:24:59]:").count(), 1);
        assert_canonical_assistant_speech(&user_text, "らぼみ", "生きてるよ〜！");
        assert!(!user_text.contains("[assistant]:"));
        let user_at = user_text.find("らぼみちゃんいきてる？").unwrap();
        let assistant_at = user_text.find("生きてるよ〜！").unwrap();
        let end_at = user_text.find("</conversation_history>").unwrap();
        assert!(user_at < assistant_at && assistant_at < end_at, "{user_text}");
    }

    #[tokio::test]
    async fn multipart_plain_speech_stays_ordered_inside_one_bounded_history() {
        use std::sync::{Arc, Mutex};

        struct RecordingContinuationLlm {
            responses: Mutex<Vec<ChatResponse>>,
            requests: Arc<Mutex<Vec<Vec<Message>>>>,
        }

        #[async_trait]
        impl LlmClient for RecordingContinuationLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.requests.lock().unwrap().push(request.messages);
                Ok(self.responses.lock().unwrap().remove(0))
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let llm = RecordingContinuationLlm {
            responses: Mutex::new(vec![
                text_response("途中1"),
                text_response("途中2"),
                text_response("NO_REPLY"),
            ]),
            requests: requests.clone(),
        };
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 4);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech({
            let delivered = delivered.clone();
            Arc::new(move |text| {
                let delivered = delivered.clone();
                Box::pin(async move {
                    delivered.lock().unwrap().push(text);
                    Ok(())
                })
            })
        });

        let history = "<conversation_history>\n[u1]:\n続きを話して\n</conversation_history>";
        let result = engine.run("system", history, "test-model").await.unwrap();

        assert_eq!(result.iterations, 3);
        assert_eq!(delivered.lock().unwrap().as_slice(), ["途中1", "途中2"]);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for request in requests.iter().skip(1) {
            assert!(request
                .iter()
                .all(|message| message.role != Role::Assistant));
            let user_text = request
                .iter()
                .find(|message| message.role == Role::User)
                .map(message_plain_text)
                .unwrap();
            assert_eq!(user_text.matches("<conversation_history>").count(), 1);
            assert_eq!(user_text.matches("</conversation_history>").count(), 1);
        }
        let final_history = requests[2]
            .iter()
            .find(|message| message.role == Role::User)
            .map(message_plain_text)
            .unwrap();
        assert_canonical_assistant_speech(&final_history, "らぼみ", "途中1");
        assert_canonical_assistant_speech(&final_history, "らぼみ", "途中2");
        let user_at = final_history.find("続きを話して").unwrap();
        let first_at = final_history.find("途中1").unwrap();
        let second_at = final_history.find("途中2").unwrap();
        let end_at = final_history.find("</conversation_history>").unwrap();
        assert!(
            user_at < first_at && first_at < second_at && second_at < end_at,
            "{final_history}"
        );
    }

    #[tokio::test]
    async fn live_inbound_then_plain_continuation_rebuilds_one_ordered_history() {
        use std::sync::{Arc, Mutex};

        struct OneInbound {
            pending: Mutex<Option<Vec<String>>>,
        }

        impl LiveInboundSource for OneInbound {
            fn poll_new_messages(&self) -> Vec<String> {
                self.pending.lock().unwrap().take().unwrap_or_default()
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(
            vec![
                text_response("先に確認するね"),
                text_response("新着も読んだよ"),
                text_response("NO_REPLY"),
            ],
            requests.clone(),
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 5);
        engine.set_assistant_history_name("らぼみ");
        engine.set_live_inbound(Arc::new(OneInbound {
            pending: Mutex::new(Some(vec![
                "[u2][2026-09-21 13:25:01]:\n追加でこれも見て".to_string(),
            ])),
        }));
        engine.set_on_continuation_speech({
            let delivered = delivered.clone();
            Arc::new(move |text| {
                let delivered = delivered.clone();
                Box::pin(async move {
                    delivered.lock().unwrap().push(text);
                    Ok(())
                })
            })
        });

        let history = "<conversation_history>\n[u1][2026-09-21 13:24:59]:\n確認して\n</conversation_history>";
        let result = engine.run("system", history, "test-model").await.unwrap();

        assert_eq!(result.iterations, 3);
        assert_eq!(
            delivered.lock().unwrap().as_slice(),
            ["先に確認するね", "新着も読んだよ"]
        );
        let requests = requests.lock().unwrap();
        for request in requests.iter() {
            assert_one_conversation_boundary(request);
        }
        let third = &requests[2];
        assert!(
            third.iter().all(|message| message.role != Role::Assistant),
            "visible assistant speech escaped the boundary: {third:?}"
        );
        assert_eq!(
            third.iter().filter(|message| message.role == Role::User).count(),
            1,
            "live inbound must be rebuilt into the one bounded user history: {third:?}"
        );
        let rebuilt = third
            .iter()
            .find(|message| message.role == Role::User)
            .map(message_plain_text)
            .unwrap();
        for text in ["確認して", "先に確認するね", "追加でこれも見て", "新着も読んだよ"] {
            assert_eq!(rebuilt.matches(text).count(), 1, "{rebuilt}");
        }
        let positions = [
            rebuilt.find("確認して").unwrap(),
            rebuilt.find("先に確認するね").unwrap(),
            rebuilt.find("追加でこれも見て").unwrap(),
            rebuilt.find("新着も読んだよ").unwrap(),
        ];
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{rebuilt}");
        assert_canonical_assistant_speech(&rebuilt, "らぼみ", "先に確認するね");
        assert_canonical_assistant_speech(&rebuilt, "らぼみ", "新着も読んだよ");
        assert!(!rebuilt.contains("[assistant]:"), "noncanonical header: {rebuilt}");
    }

    #[tokio::test]
    async fn mixed_visible_speech_and_native_tool_call_rebuilds_bounded_history() {
        use std::sync::{Arc, Mutex};

        let requests = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(
            vec![
                resp(
                    Some("確認してくるね"),
                    vec![tc("tc-mixed", "test_tool", serde_json::json!({}))],
                ),
                text_response("NO_REPLY"),
            ],
            requests.clone(),
        );
        let executor = MockExecutor::new().add_result("test_tool", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 4);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech({
            let delivered = delivered.clone();
            Arc::new(move |text| {
                let delivered = delivered.clone();
                Box::pin(async move {
                    delivered.lock().unwrap().push(text);
                    Ok(())
                })
            })
        });

        let history = "<conversation_history>\n[u1][2026-09-21 13:24:59]:\n道具で確認して\n</conversation_history>";
        let result = engine.run("system", history, "test-model").await.unwrap();

        assert_eq!(result.iterations, 2);
        assert_eq!(delivered.lock().unwrap().as_slice(), ["確認してくるね"]);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let second = &requests[1];
        assert_one_conversation_boundary(second);
        assert_eq!(
            second
                .iter()
                .map(message_plain_text)
                .map(|text| text.matches("確認してくるね").count())
                .sum::<usize>(),
            1,
            "delivered speech must occur exactly once in the request: {second:?}"
        );

        let assistant_tool_index = second
            .iter()
            .position(|message| {
                message.role == Role::Assistant
                    && message.tool_calls.as_ref().is_some_and(|calls| {
                        calls.len() == 1 && calls[0].id == "tc-mixed"
                    })
            })
            .expect("native assistant tool call remains in the request");
        let tool_result_index = second
            .iter()
            .position(|message| {
                message.role == Role::Tool
                    && message.tool_call_id.as_deref() == Some("tc-mixed")
            })
            .expect("native tool result remains in the request");
        assert_eq!(
            tool_result_index,
            assistant_tool_index + 1,
            "native assistant(tool_calls) -> tool-result adjacency changed: {second:?}"
        );
        assert!(
            message_plain_text(&second[assistant_tool_index])
                .trim()
                .is_empty(),
            "assistant text must not remain on the native tool-call message: {second:?}"
        );

        let bounded = second
            .iter()
            .find(|message| message_plain_text(message).contains("<conversation_history>"))
            .map(message_plain_text)
            .expect("one bounded conversation history");
        assert_eq!(bounded.matches("確認してくるね").count(), 1);
        assert_canonical_assistant_speech(&bounded, "らぼみ", "確認してくるね");
        let user_at = bounded.find("道具で確認して").unwrap();
        let assistant_at = bounded.find("確認してくるね").unwrap();
        let end_at = bounded.find("</conversation_history>").unwrap();
        assert!(user_at < assistant_at && assistant_at < end_at, "{bounded}");
    }

    #[tokio::test]
    async fn all_utterance_tool_continuation_moves_delivered_content_into_bounded_history() {
        use std::sync::{Arc, Mutex};

        let requests = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(
            vec![
                resp(
                    Some("返信も続けるね"),
                    vec![tc("reply-1", "reply", serde_json::json!({"text": "返信したよ"}))],
                ),
                text_response("NO_REPLY"),
            ],
            requests.clone(),
        );
        let executor = MockExecutor::new().add_result("reply", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 4);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech({
            let delivered = delivered.clone();
            Arc::new(move |text| {
                let delivered = delivered.clone();
                Box::pin(async move {
                    delivered.lock().unwrap().push(text);
                    Ok(())
                })
            })
        });

        let history = "<conversation_history>\n[u1][2026-09-21 13:24:59]:\n返信して\n</conversation_history>";
        let result = engine.run("system", history, "test-model").await.unwrap();

        assert_eq!(result.iterations, 2);
        assert_eq!(delivered.lock().unwrap().as_slice(), ["返信も続けるね"]);
        let requests = requests.lock().unwrap();
        let second = &requests[1];
        assert_one_conversation_boundary(second);
        assert_eq!(
            second
                .iter()
                .map(message_plain_text)
                .map(|text| text.matches("返信も続けるね").count())
                .sum::<usize>(),
            1,
            "delivered content must occur exactly once: {second:?}"
        );
        let assistant_tool_index = second
            .iter()
            .position(|message| {
                message.role == Role::Assistant
                    && message.tool_calls.as_ref().is_some_and(|calls| {
                        calls.len() == 1 && calls[0].id == "reply-1"
                    })
            })
            .expect("native assistant utterance tool call remains");
        assert!(
            message_plain_text(&second[assistant_tool_index]).is_empty(),
            "only native assistant text is removed: {second:?}"
        );
        assert_eq!(
            second[assistant_tool_index + 1].role,
            Role::Tool,
            "utterance tool result remains adjacent: {second:?}"
        );
        assert_eq!(
            second[assistant_tool_index + 1].tool_call_id.as_deref(),
            Some("reply-1")
        );
        let bounded = second
            .iter()
            .find(|message| message_plain_text(message).contains("<conversation_history>"))
            .map(message_plain_text)
            .expect("one bounded conversation history");
        assert_canonical_assistant_speech(&bounded, "らぼみ", "返信も続けるね");
    }

    #[tokio::test]
    async fn continuation_without_existing_boundary_creates_one_canonical_history() {
        use std::sync::{Arc, Mutex};

        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(
            vec![text_response("境界を作って続けるね"), text_response("NO_REPLY")],
            requests.clone(),
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech(Arc::new(|_| Box::pin(async { Ok(()) })));

        let result = engine
            .run("system", "境界なしの依頼", "test-model")
            .await
            .unwrap();

        assert_eq!(result.iterations, 2);
        let requests = requests.lock().unwrap();
        let second = &requests[1];
        assert_one_conversation_boundary(second);
        assert!(
            second
                .iter()
                .all(|message| message.role != Role::Assistant),
            "continued speech must not fall back to a standalone assistant message: {second:?}"
        );
        let bounded = second
            .iter()
            .find(|message| message_plain_text(message).contains("<conversation_history>"))
            .map(message_plain_text)
            .expect("canonical history was created");
        assert_eq!(bounded.matches("境界なしの依頼").count(), 1);
        assert_eq!(bounded.matches("境界を作って続けるね").count(), 1);
        assert_canonical_assistant_speech(&bounded, "らぼみ", "境界を作って続けるね");
        let user_at = bounded.find("境界なしの依頼").unwrap();
        let assistant_at = bounded.find("境界を作って続けるね").unwrap();
        let end_at = bounded.find("</conversation_history>").unwrap();
        assert!(user_at < assistant_at && assistant_at < end_at, "{bounded}");
    }

    #[tokio::test]
    async fn malformed_history_boundary_stops_continuation_with_clear_error() {
        use std::sync::{Arc, Mutex};

        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(vec![text_response("続けるね")], requests.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech(Arc::new(|_| Box::pin(async { Ok(()) })));

        let error = engine
            .run(
                "system",
                "<conversation_history>\n[u1]:\n閉じタグがない",
                "test-model",
            )
            .await
            .expect_err("malformed boundary must stop the turn");

        assert!(
            error
                .to_string()
                .contains("malformed or multiple <conversation_history> boundaries"),
            "unexpected error: {error:#}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn multiple_history_boundaries_stop_continuation_with_clear_error() {
        use std::sync::{Arc, Mutex};

        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(vec![text_response("続けるね")], requests.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech(Arc::new(|_| Box::pin(async { Ok(()) })));

        let error = engine
            .run(
                "system",
                "<conversation_history>\n[u1]:\n一つ目\n</conversation_history>\n<conversation_history>\n[u2]:\n二つ目\n</conversation_history>",
                "test-model",
            )
            .await
            .expect_err("multiple boundaries must stop the turn");

        assert!(
            error
                .to_string()
                .contains("malformed or multiple <conversation_history> boundaries"),
            "unexpected error: {error:#}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn native_tool_result_then_plain_continuation_keeps_roles_and_bounded_speech() {
        use std::sync::{Arc, Mutex};

        let requests = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(
            vec![
                tool_call_response(vec![tc("tc-1", "test_tool", serde_json::json!({}))]),
                text_response("結果を確認したよ"),
                text_response("NO_REPLY"),
            ],
            requests.clone(),
        );
        let executor = MockExecutor::new().add_result("test_tool", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 5);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech({
            let delivered = delivered.clone();
            Arc::new(move |text| {
                let delivered = delivered.clone();
                Box::pin(async move {
                    delivered.lock().unwrap().push(text);
                    Ok(())
                })
            })
        });

        let history = "<conversation_history>\n[u1][2026-09-21 13:24:59]:\n道具で確認して\n</conversation_history>";
        let result = engine.run("system", history, "test-model").await.unwrap();

        assert_eq!(result.iterations, 3);
        assert_eq!(delivered.lock().unwrap().as_slice(), ["結果を確認したよ"]);
        let requests = requests.lock().unwrap();
        for request in requests.iter() {
            assert_one_conversation_boundary(request);
        }
        let third = &requests[2];
        let assistant_tool_index = third
            .iter()
            .position(|message| {
                message.role == Role::Assistant
                    && message.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty())
            })
            .expect("native assistant tool call remains in the request");
        let tool_result_index = third
            .iter()
            .position(|message| message.role == Role::Tool && message.tool_call_id.as_deref() == Some("tc-1"))
            .expect("native tool result remains in the request");
        assert_eq!(
            tool_result_index,
            assistant_tool_index + 1,
            "native assistant(tool_calls) -> tool adjacency changed: {third:?}"
        );
        assert!(
            third.iter().all(|message| {
                message.role != Role::Assistant
                    || message.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty())
            }),
            "plain assistant speech escaped the boundary: {third:?}"
        );
        let bounded = third
            .iter()
            .find(|message| message_plain_text(message).contains("<conversation_history>"))
            .map(message_plain_text)
            .expect("one bounded conversation history");
        assert_eq!(bounded.matches("結果を確認したよ").count(), 1);
        assert!(
            bounded.find("道具で確認して").unwrap() < bounded.find("結果を確認したよ").unwrap(),
            "{bounded}"
        );
        assert_canonical_assistant_speech(&bounded, "らぼみ", "結果を確認したよ");
        assert!(!bounded.contains("[assistant]:"), "noncanonical header: {bounded}");
    }

    #[tokio::test]
    async fn standalone_no_reply_keeps_prior_text_when_no_delivery_hook_exists() {
        let (llm, _) = MockLlm::counting(vec![
            text_response("64"),
            text_response("NO_REPLY"),
        ]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);

        let result = engine.run("system", "23+41", "test-model").await.unwrap();

        assert_eq!(result.response, "64");
    }

    #[tokio::test]
    async fn explicit_termination_utterance_without_no_reply_runs_next_call() {
        use std::sync::atomic::Ordering;
        use std::sync::Mutex;

        let (llm, chat_calls) = MockLlm::counting(vec![
            tool_call_response(vec![tc(
                "reply-1",
                "reply",
                serde_json::json!({"text": "確認するね"}),
            )]),
            text_response("確認結果はY\nNO_REPLY"),
        ]);
        let executor_calls = Arc::new(Mutex::new(Vec::new()));
        let executor = MockExecutor::new()
            .add_result("reply", successful_action_result())
            .with_call_log(executor_calls.clone());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "確認して", "test-model")
            .await
            .expect("reply 後も NO_REPLY まで継続する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(result.iterations, 2);
        assert_eq!(executor_calls.lock().unwrap().as_slice(), &["reply"]);
        assert_eq!(result.response, "確認結果はY");
    }

    #[tokio::test]
    async fn explicit_termination_body_then_no_reply_ends_in_one_call() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) =
            MockLlm::counting(vec![text_response("これが最終回答\nNO_REPLY")]);
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&[])));

        let result = engine
            .run("system", "答えて", "test-model")
            .await
            .expect("NO_REPLY で明示終了する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 1);
        assert_eq!(result.iterations, 1);
        assert_eq!(result.response, "これが最終回答");
    }

    #[tokio::test]
    async fn explicit_termination_query_result_is_read_before_end() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            tool_call_response(vec![tc(
                "resolve-1",
                "resolve",
                serde_json::json!({"ref": "e1"}),
            )]),
            text_response("取得結果を確認した\nNO_REPLY"),
        ]);
        let executor = MockExecutor::new().add_result("resolve", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_tool_dispatcher(Arc::new(RecordingDispatcher::new(&["resolve"])));

        let result = engine
            .run("system", "取得して", "test-model")
            .await
            .expect("tool result 後に明示終了する");

        assert_eq!(chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(result.iterations, 2);
        assert_eq!(result.response, "取得結果を確認した");
    }


    #[tokio::test]
    async fn missing_explicit_termination_stops_only_at_iteration_limit() {
        use std::sync::atomic::Ordering;

        let (llm, chat_calls) = MockLlm::counting(vec![
            text_response("まだ作業中1"),
            text_response("まだ作業中2"),
            text_response("まだ作業中3"),
        ]);
        let engine = SkillEngine::new(Box::new(llm), Box::new(MockExecutor::new()), 3);

        let result = engine
            .run("system", "続けて", "test-model")
            .await
            .expect("既存上限で停止する");

        assert!(result.stopped_by_limit);
        assert_eq!(chat_calls.load(Ordering::SeqCst), 3);
        assert_eq!(result.iterations, 4);
        assert!(result.response.is_empty(), "資源切れ本文を投稿してはならない");
    }
