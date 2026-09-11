// ---------------------------------------------------------------------------
// #900(b)【§13 #9（reply×N＋継続 のみ→reply N 配送・進む）／ターン合計 reply1＋継続 ×2→reply1】:
// reply×1 + 末尾 継続 を 2 回、最後に reply×1 → 配送 3・LLM 3。
//
// 現契約では発話だけで終了せず、NO_REPLY が出るまで次iterationへ進む。
// ---------------------------------------------------------------------------
const B900_1: &str = "b900-reply-one 一通目";
const B900_2: &str = "b900-reply-two 二通目";
const B900_3: &str = "b900-reply-three 三通目";

fn reply_with_optional_continue(text: &str, cont: bool) -> ChatResponse {
    let msg = Message {
        role: Role::Assistant,
        content: if cont {
            None
        } else {
            Some(MessageContent::Text("NO_REPLY".to_string()))
        },
        name: None,
        function_call: None,
        tool_calls: Some(vec![ToolCall {
            id: format!("tc-{}", uuid::Uuid::new_v4()),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "reply".to_string(),
                arguments: serde_json::json!({"event": "e1", "text": text}).to_string(),
            },
        }]),
        tool_call_id: None,
    };
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: Some(FinishReason::ToolCalls),
        }],
        usage: Usage::default(),
        created: 0,
    }
}

struct ReplyContinueMock {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReplyContinueMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(match n {
            0 => reply_with_optional_continue(B900_1, true),
            1 => reply_with_optional_continue(B900_2, true),
            _ => reply_with_optional_continue(B900_3, false),
        })
    }
}

#[tokio::test]
async fn audit_900b_reply_plus_continue_delivers_three_over_three_llm_calls() {
    let buf = install_capture();
    let mock = Arc::new(ReplyContinueMock {
        calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, _session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "900b".repeat(16);
    fixture.append_line(&mention_event(&ev, "B900-MARK reply 3回に分けて"));

    // 3 通目まで配送されるのを待つ（現 tip は 1 通目で止まる → タイムアウト後に赤 assert）。
    let all_three = {
        let buf = buf.clone();
        wait_until(move || {
            [B900_1, B900_2, B900_3].iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b))
            })
        })
        .await
    };
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 配送 3: reply が 3 通（現 tip は 1 通で止まる → 赤）。
    for b in [B900_1, B900_2, B900_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "reply" && c.body.contains(b))
            .count();
        assert_eq!(
            n,
            1,
            "reply {b} の配送回数が 1 でない（#900(b): 発話＋継続 継続が効かない）: {:?}",
            captured(&buf)
        );
    }
    assert!(
        all_three,
        "reply×3（発話＋継続 継続）が揃わない: {:?}",
        captured(&buf)
    );

    // LLM 3 回（現 tip は 1 回で止まる → 赤）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        3,
        "発話＋末尾 継続 が次イテレーションを起こしていない（LLM 呼び出しが 3 でない）"
    );
}

// ---------------------------------------------------------------------------
// §13 #7【reply×N＋本文】: reply×2 と本文を 1 生成で並べる → 配送 reply2＋本文1・保存 3・
// LLM 1・🤐 なし（発話がある）。発話 op と本文の同時配送＝発話クラス化（#883）の契約点。
// 現 tip で緑なら**非回帰ピン**（発話＋本文の同時配送が壊れていないことを固定）。
// ---------------------------------------------------------------------------
const R7_REPLY_1: &str = "r7-reply-one 返信その1";
const R7_REPLY_2: &str = "r7-reply-two 返信その2";
const R7_BODY: &str = "r7-body-gamma まとめの本文だよ";

fn replies_with_body(replies: &[&str], body: &str) -> ChatResponse {
    let tool_calls: Vec<ToolCall> = replies
        .iter()
        .map(|text| ToolCall {
            id: format!("tc-{}", uuid::Uuid::new_v4()),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "reply".to_string(),
                arguments: serde_json::json!({"event": "e1", "text": text}).to_string(),
            },
        })
        .collect();
    let msg = Message {
        role: Role::Assistant,
        content: Some(MessageContent::Text(format!("{body}\nNO_REPLY"))),
        name: None,
        function_call: None,
        tool_calls: Some(tool_calls),
        tool_call_id: None,
    };
    ChatResponse {
        id: uuid::Uuid::new_v4().to_string(),
        model: "mock-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: msg,
            finish_reason: Some(FinishReason::ToolCalls),
        }],
        usage: Usage::default(),
        created: 0,
    }
}

struct ReplyBodyMock {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for ReplyBodyMock {
    fn name(&self) -> &str {
        "mock"
    }
    fn sends_max_output_tokens(&self) -> bool {
        false
    }
    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }
    async fn chat_completion(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(replies_with_body(&[R7_REPLY_1, R7_REPLY_2], R7_BODY))
    }
}

#[tokio::test]
async fn audit_s13_7_replies_plus_body_deliver_all_and_save_all() {
    let buf = install_capture();
    let mock = Arc::new(ReplyBodyMock {
        calls: AtomicUsize::new(0),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "1307".repeat(16);
    fixture.append_line(&mention_event(&ev, "R7-MARK 2 回返信して最後にまとめて"));

    // 本文 say（standalone）が出るまで待つ。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, R7_BODY).is_some()).await
    };
    assert!(done, "本文 say が出ない: {:?}", captured(&buf));
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 配送: reply2（kind=reply）＋本文1（kind=standalone）。
    for r in [R7_REPLY_1, R7_REPLY_2] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "reply" && c.body.contains(r))
            .count();
        assert_eq!(
            n,
            1,
            "reply {r} の配送回数が 1 でない: {:?}",
            captured(&buf)
        );
    }
    let body_says = captured(&buf)
        .iter()
        .filter(|c| c.kind == "standalone" && c.body.contains(R7_BODY))
        .count();
    assert_eq!(
        body_says,
        1,
        "本文 say の配送回数が 1 でない: {:?}",
        captured(&buf)
    );

    // 保存: reply2 本文＋本文1 = 3 件（speech）。
    let saved = agent_speech_contents(&core, &session_id);
    for m in [R7_REPLY_1, R7_REPLY_2, R7_BODY] {
        assert!(
            saved.iter().any(|s| s.contains(m)),
            "{m} が speech に保存されていない（reply＋本文の保存 N+1）: {saved:?}"
        );
    }

    // LLM 1・subtask なし（1 生成で完結・撃ちっぱなし）。
    assert_eq!(
        mock.calls.load(Ordering::SeqCst),
        1,
        "reply×N＋本文は 1 生成で完結する"
    );
    assert!(
        !core.state.subtask_registries.has_running(&session_id),
        "reply×N＋本文が subtask 化された"
    );
}

