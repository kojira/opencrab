    // -----------------------------------------------------------------------
    // D-1111: the next request shows the turn's events in exactly the order they happened,
    // every event appended at the end (one turn-view writer).
    // -----------------------------------------------------------------------

    /// The events of a request after the initial history, in request order, as short labels:
    /// `say:<text>` (delivered speech), `call:<id>`, `result:<id>`, `in:<text>` (new inbound).
    fn event_labels(request: &[Message]) -> Vec<String> {
        let mut out = Vec::new();
        for message in request {
            match message.role {
                Role::Assistant => {
                    for call in message.tool_calls.iter().flatten() {
                        out.push(format!("call:{}", call.id));
                    }
                }
                Role::Tool => out.push(format!("result:{}", message.tool_call_id.clone().unwrap_or_default())),
                Role::User => {
                    let text = message_plain_text(message);
                    let tail = match text.find("</conversation_history>") {
                        Some(end) => {
                            let start = text.find("<conversation_history>").map(|s| s + 22).unwrap_or(0);
                            text[start..end].to_string()
                        }
                        None => text,
                    };
                    for line in tail.lines() {
                        if let Some(rest) = line.strip_prefix("SAY ") {
                            out.push(format!("say:{rest}"));
                        } else if let Some(rest) = line.strip_prefix("IN ") {
                            out.push(format!("in:{rest}"));
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    async fn run_and_capture(
        responses: Vec<ChatResponse>,
        inbound: Option<(usize, &'static str)>,
    ) -> Vec<Vec<Message>> {
        use std::sync::Mutex;

        struct InboundAt {
            at_poll: usize,
            text: &'static str,
            polls: Mutex<usize>,
        }
        impl LiveInboundSource for InboundAt {
            fn poll_new_messages(&self) -> Vec<String> {
                let mut polls = self.polls.lock().unwrap();
                *polls += 1;
                if *polls == self.at_poll {
                    vec![format!("[u2][2026-11-03 00:00:00]:\nIN {}", self.text)]
                } else {
                    Vec::new()
                }
            }
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm = CapturingLlm::new(responses, requests.clone());
        let executor = MockExecutor::new().add_result("test_tool", successful_action_result());
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_assistant_history_name("らぼみ");
        engine.set_on_continuation_speech(Arc::new(|_| Box::pin(async { Ok(()) })));
        if let Some((at_poll, text)) = inbound {
            engine.set_live_inbound(Arc::new(InboundAt {
                at_poll,
                text,
                polls: Mutex::new(0),
            }));
        }
        let history = "<conversation_history>\n[u1][2026-11-03 00:00:00]:\n依頼\n</conversation_history>";
        engine.run("system", history, "test-model").await.unwrap();
        let requests = requests.lock().unwrap().clone();
        requests
    }

    #[tokio::test]
    async fn text_then_next_request_ends_with_that_text() {
        let requests = run_and_capture(
            vec![text_response("SAY a"), text_response("NO_REPLY")],
            None,
        )
        .await;
        assert_eq!(event_labels(&requests[1]), ["say:a"]);
    }

    #[tokio::test]
    async fn tool_then_text_is_seen_after_the_result() {
        let requests = run_and_capture(
            vec![
                tool_call_response(vec![tc("t1", "test_tool", serde_json::json!({}))]),
                text_response("SAY b"),
                text_response("NO_REPLY"),
            ],
            None,
        )
        .await;
        assert_eq!(event_labels(&requests[2]), ["call:t1", "result:t1", "say:b"]);
    }

    #[tokio::test]
    async fn text_then_tool_then_text_keeps_every_event_in_order() {
        let requests = run_and_capture(
            vec![
                text_response("SAY a"),
                resp(Some("SAY b"), vec![tc("t1", "test_tool", serde_json::json!({}))]),
                text_response("SAY c"),
                text_response("NO_REPLY"),
            ],
            None,
        )
        .await;
        assert_eq!(
            event_labels(&requests[3]),
            ["say:a", "say:b", "call:t1", "result:t1", "say:c"]
        );
    }

    #[tokio::test]
    async fn inbound_arriving_mid_turn_is_seen_where_it_arrived() {
        // Poll 1 happens before the 2nd request (after the tool result).
        let requests = run_and_capture(
            vec![
                tool_call_response(vec![tc("t1", "test_tool", serde_json::json!({}))]),
                text_response("SAY b"),
                text_response("NO_REPLY"),
            ],
            Some((1, "x")),
        )
        .await;
        assert_eq!(event_labels(&requests[1]), ["call:t1", "result:t1", "in:x"]);
        assert_eq!(
            event_labels(&requests[2]),
            ["call:t1", "result:t1", "in:x", "say:b"]
        );
    }
