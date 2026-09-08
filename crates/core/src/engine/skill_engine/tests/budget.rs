    /// 圧縮可能な user message（先頭 OldHistory / 末尾 8 ブロックは RecentVerbatim）を組む。
    fn compactible_user(blocks: usize, words_per_block: usize) -> Message {
        let mut text = String::new();
        for i in 0..blocks {
            text.push_str(&format!("[s{i}]:\n"));
            text.push_str(&"word ".repeat(words_per_block));
            text.push('\n');
        }
        Message {
            role: Role::User,
            content: Some(MessageContent::Text(text)),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn sys_msg() -> Message {
        Message {
            role: Role::System,
            content: Some(MessageContent::Text("sys".into())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn user_from(text: &str) -> Message {
        Message {
            role: Role::User,
            content: Some(MessageContent::Text(text.into())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn json_of_words(n: usize) -> String {
        format!(r#"{{"data":"{}"}}"#, "word ".repeat(n))
    }

    /// 結果を 1 件ずつ seat → append。予約しない。失敗しても turn は死なない（Ok）。
    fn seat_batch(
        conversation: &str,
        results: &[(&str, String)],
        high: usize,
        low: usize,
    ) -> Result<Vec<String>, String> {
        let mut messages = vec![sys_msg(), user_from(conversation)];
        let conv = crate::tokens::estimate_tokens(conversation);
        let mut ledger = crate::context_budget::TokenLedger::new();
        ledger.record_tokens("user", conv);
        let mut gov = Some(crate::context_budget::TurnGovernor::new(high, low));
        let mut out = Vec::new();
        for (i, (name, body)) in results.iter().enumerate() {
            let capped = seat_tool_result(
                &mut gov,
                &mut ledger,
                &mut messages,
                name,
                body,
                |remaining| {
                    crate::tool_result_log::sanitize_tool_result_for_append(
                        name,
                        body,
                        "sess",
                        &format!("tc{i}"),
                        None,
                        remaining,
                    )
                },
            )
            .map_err(|e| e.to_string())?;
            messages.push(Message::tool(format!("tc{i}"), capped.clone()));
            ledger.record(format!("tool:{}", messages.len()), &capped);
            apply_turn_budget(&mut gov, &mut ledger, &mut messages, 0)
                .map_err(|e| e.to_string())?;
            out.push(capped);
        }
        Ok(out)
    }

    /// QC 19:05:42。ws_read×2 + search を会話 18k に載せる。予約はせず、全文が入る。
    #[test]
    fn qc_two_small_ws_reads_and_search_fit_without_truncation() {
        let high = 71_000usize;
        let low = 31_000usize;
        let conv = "word ".repeat(18_000);
        let read = json_of_words(1_800);
        let search = json_of_words(400);
        assert!(
            crate::tokens::estimate_tokens(&read) < 3_000,
            "QC の 120 行相当は数千トークン"
        );
        let seated = seat_batch(
            &conv,
            &[
                ("ws_read", read.clone()),
                ("ws_read", read.clone()),
                ("search_my_history", search.clone()),
            ],
            high,
            low,
        )
        .expect("ツール結果が理由で turn は死なない");
        assert_eq!(seated[0], read, "1 本目 ws_read は切り詰めない");
        assert_eq!(seated[1], read, "2 本目 ws_read は切り詰めない");
        assert_eq!(seated[2], search, "search は切り詰めない");
    }

    /// 予約モデル撤廃後の代表境界。どんな構成でも turn は死なず、足りなければスタブ。
    #[test]
    fn append_model_boundary_matrix() {
        let high = 71_000usize;
        let low = 31_000usize;
        let small_conv = "hello";
        let compactible = message_plain_text(&compactible_user(40, 300));
        let mut inviolable = String::new();
        for i in 0..5 {
            inviolable.push_str(&format!("[owner{i}] [2026-08-30 00:00:0{i}]:\n"));
            inviolable.push_str(&"word ".repeat(4_000));
            inviolable.push('\n');
        }
        let small_read = json_of_words(1_800);
        let large_read = json_of_words(28_000);
        let unread = json_of_words(28_000);
        let write = json_of_words(2_000);

        struct Case {
            name: &'static str,
            conv: String,
            results: Vec<(&'static str, String)>,
        }
        let cases = [
            Case {
                name: "small-conv + 1 small ws_read",
                conv: small_conv.into(),
                results: vec![("ws_read", small_read.clone())],
            },
            Case {
                name: "small-conv + 1 large ws_read",
                conv: small_conv.into(),
                results: vec![("ws_read", large_read.clone())],
            },
            Case {
                name: "small-conv + 1 unestimable read",
                conv: small_conv.into(),
                results: vec![("ws_list", unread.clone())],
            },
            Case {
                name: "small-conv + 1 non-read",
                conv: small_conv.into(),
                results: vec![("ws_write", write.clone())],
            },
            Case {
                name: "compactible + 1 large ws_read",
                conv: compactible.clone(),
                results: vec![("ws_read", large_read.clone())],
            },
            Case {
                name: "compactible + QC trio",
                conv: compactible.clone(),
                results: vec![
                    ("ws_read", small_read.clone()),
                    ("ws_read", small_read.clone()),
                    ("search_my_history", json_of_words(400)),
                ],
            },
            Case {
                name: "compactible + 3 large reads",
                conv: compactible.clone(),
                results: vec![
                    ("ws_list", unread.clone()),
                    ("ws_list", unread.clone()),
                    ("ws_list", unread.clone()),
                ],
            },
            Case {
                name: "inviolable + 1 small ws_read",
                conv: inviolable.clone(),
                results: vec![("ws_read", small_read.clone())],
            },
            Case {
                name: "inviolable + 3 large reads",
                conv: inviolable.clone(),
                results: vec![
                    ("ws_list", unread.clone()),
                    ("ws_list", unread.clone()),
                    ("ws_list", unread.clone()),
                ],
            },
            Case {
                name: "small-conv + 5 mixed",
                conv: small_conv.into(),
                results: vec![
                    ("ws_read", small_read),
                    ("ws_read", large_read),
                    ("ws_list", unread),
                    ("ws_write", write.clone()),
                    ("ws_write", write),
                ],
            },
        ];

        for case in &cases {
            let seated = seat_batch(&case.conv, &case.results, high, low)
                .unwrap_or_else(|e| panic!("{}: ツール結果で turn が死んだ: {e}", case.name));
            assert_eq!(seated.len(), case.results.len(), "{}", case.name);
            for (i, capped) in seated.iter().enumerate() {
                assert!(
                    !capped.is_empty(),
                    "{}: result[{i}] が空（切り詰め済みかスタブが載ること）",
                    case.name
                );
            }
        }
    }

    #[test]
    fn user_line_items_marks_newest_speech_must_keep_not_trailing_tools() {
        let text = "[owner] [2026-08-30 17:57:20]:\n東京！\n\
                    [agent] [2026-08-30 17:57:54]:\n[tool_call]:\n[id=c1]: execute_shell({})\n\
                    [system: subtask_completed] [2026-08-30 17:58:08]:\n{\"exit_reason\":\"completed\"}\n";
        let messages = vec![
            sys_msg(),
            Message {
                role: Role::User,
                content: Some(MessageContent::Text(text.into())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let items = user_line_items(&messages);
        let origin = items
            .iter()
            .find(|i| i.text.contains("東京！"))
            .expect("origin block");
        assert!(origin.must_keep, "発端 speech が must_keep: {items:#?}");
        assert!(items
            .iter()
            .any(|i| i.text.contains("[tool_call]") && !i.must_keep));

        let mut long = String::new();
        for i in 0..30 {
            long.push_str(&format!("[old{i}] [2026-08-01 00:00:00]:\n"));
            long.push_str(&"word ".repeat(250));
            long.push('\n');
        }
        long.push_str("[owner] [2026-08-30 17:57:00]:\n明日の天気教えて\n");
        long.push_str("[agent] [2026-08-30 17:57:10]:\nどこの地域？\n");
        long.push_str("[owner] [2026-08-30 17:57:20]:\n東京！\n");
        for i in 0..10 {
            long.push_str(&format!(
                "[agent] [2026-08-30 17:57:{i:02}]:\n[tool_call]:\n[id=c{i}]: execute_shell({{}})\n"
            ));
            long.push_str(&format!(
                "[system: subtask_completed] [2026-08-30 17:58:{i:02}]:\n{{\"exit_reason\":\"completed\"}}\n"
            ));
        }
        let long_msgs = vec![
            sys_msg(),
            Message {
                role: Role::User,
                content: Some(MessageContent::Text(long)),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            },
        ];
        let long_items = user_line_items(&long_msgs);
        let origin = long_items
            .iter()
            .find(|i| i.text.contains("東京！"))
            .expect("origin in long conversation");
        assert!(
            origin.must_keep,
            "長い会話でも発端は must_keep: keep={} lane={:?} idx-ish={}",
            origin.must_keep,
            origin.lane,
            origin.log_id.unwrap_or(-1)
        );
    }

    /// QC: ツール決着を消費するイテレーションのプロンプトから発端 user 発話が消える。
    /// 圧縮しても must_keep の発端は残す。
    #[tokio::test]
    async fn settlement_iteration_prompt_keeps_originating_user_utterance() {
        use std::sync::{Arc, Mutex};

        const ORIGIN: &str = "東京！";
        let mut conversation = String::new();
        for i in 0..30 {
            conversation.push_str(&format!("[old{i}] [2026-08-01 00:00:00]:\n"));
            conversation.push_str(&"word ".repeat(250));
            conversation.push('\n');
        }
        conversation.push_str("[owner] [2026-08-30 17:57:00]:\n明日の天気教えて\n");
        conversation.push_str("[agent] [2026-08-30 17:57:10]:\nどこの地域？\n");
        conversation.push_str(&format!("[owner] [2026-08-30 17:57:20]:\n{ORIGIN}\n"));
        // 発端の後にツール残骸を十分置き、末尾 8 ブロックだけ must_keep では
        // 「東京！」が OldHistory になる（auto_dispatch 決着ターンの実形）。
        for i in 0..10 {
            conversation.push_str(&format!(
                "[agent] [2026-08-30 17:57:{i:02}]:\n[tool_call]:\n[id=c{i}]: execute_shell({{}})\n"
            ));
            conversation.push_str(&format!(
                "[system: subtask_completed] [2026-08-30 17:58:{i:02}]:\n{{\"exit_reason\":\"completed\"}}\n"
            ));
        }
        conversation.push_str("[subtask_completed: subtask_id=st-1, exit_reason=completed]\n");

        let conv = crate::tokens::estimate_tokens(&conversation);
        let reserved = crate::tool_result_log::READ_TOOL_RESULT_TOKEN_LIMIT;
        let high = reserved.saturating_add(4_000);
        let low = high / 2;
        assert!(
            conv < high,
            "初回は圧縮せず会話が載る (conv={conv} high={high})"
        );
        assert!(
            conv + reserved > high,
            "ws_read 予約で高水位超過になること (conv={conv} reserved={reserved} high={high})"
        );

        struct CapturingLlm {
            responses: Mutex<Vec<ChatResponse>>,
            captured: Arc<Mutex<Vec<Vec<Message>>>>,
        }
        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn chat(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
                self.captured.lock().unwrap().push(request.messages.clone());
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    anyhow::bail!("no more mock responses");
                }
                Ok(responses.remove(0))
            }
        }

        let captured = Arc::new(Mutex::new(Vec::<Vec<Message>>::new()));
        let llm = CapturingLlm {
            responses: Mutex::new(vec![
                tool_call_response(vec![tc(
                    "tc-read",
                    "ws_read",
                    serde_json::json!({"path": "weather.txt"}),
                )]),
                text_response("NO_REPLY"),
            ]),
            captured: captured.clone(),
        };
        let executor = MockExecutor::new().add_result(
            "ws_read",
            ActionResult {
                success: true,
                data: serde_json::json!({"content": "Tokyo: sunny"}),
                error: None,
            },
        );
        let mut engine = SkillEngine::new(Box::new(llm), Box::new(executor), 10);
        engine.set_conversation_waters(high, low);

        let result = engine
            .run("system", &conversation, "test-model")
            .await
            .expect("turn should complete");
        assert_eq!(result.iterations, 2);

        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 2, "初回 + 決着消費の 2 回");
        let first_user = message_plain_text(&calls[0][1]);
        assert!(
            first_user.contains(ORIGIN),
            "初回プロンプトには発端がある: {}",
            first_user.chars().rev().take(200).collect::<String>()
        );
        let settle_user = message_plain_text(&calls[1][1]);
        assert!(
            settle_user.contains(ORIGIN),
            "決着イテレーションのプロンプトに発端 user 発話が残ること。実際の末尾: {}",
            settle_user.chars().rev().take(400).collect::<String>()
        );
    }

