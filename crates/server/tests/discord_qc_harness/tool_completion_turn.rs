// Issue #975 の production 再現契約。
//
// 一つの発言から background tool を起動し、spawned を見た LLM が `CONTINUE` を選んだ
// 直後に実結果が到着する。この結果は古い固定 messages ではなく、同じ因果的 turn の次の
// LLM request へ追加されなければならない。

const ORIGIN: &str = "97501";
const REQUEST_MARKER: &str = "ISSUE975-SUMMARIZE";
const SHELL_SCRIPT: &str = "echo issue975-tool-result-once";
const COMPLETION_EVIDENCE: &str = "終了コード 0・出力: issue975-tool-result-once";
const HOLDING: &str = "確認しているので少し待ってね";
const FINAL: &str = "取得結果を確認して要約したよ";
const FOLLOWUP_MARKER: &str = "ISSUE975-FOLLOWUP";
const FAILED_SCRIPT: &str = "printf issue975-failed >&2; exit 7";
const FAILED_HOLDING: &str = "失敗処理の完了を待っているよ";
const FAILED_FINAL: &str = "失敗結果を確認したよ";

struct SameTurnCompletionMock {
    script: &'static str,
    completion_marker: &'static str,
    holding_text: &'static str,
    final_text: &'static str,
    calls: std::sync::atomic::AtomicUsize,
    dispatched: std::sync::atomic::AtomicBool,
    tool_calls_emitted: std::sync::atomic::AtomicUsize,
    active_calls: std::sync::atomic::AtomicUsize,
    max_active_calls: std::sync::atomic::AtomicUsize,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait::async_trait]
impl LlmProvider for SameTurnCompletionMock {
    fn name(&self) -> &str {
        "mock"
    }

    fn sends_max_output_tokens(&self) -> bool {
        false
    }

    fn measure_request_tokens(&self, _request: &ChatRequest) -> Option<usize> {
        Some(1)
    }

    async fn available_models(&self) -> anyhow::Result<Vec<opencrab_llm::traits::ModelInfo>> {
        Ok(vec![])
    }

    async fn chat_completion(&self, request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let active = self
            .active_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.max_active_calls
            .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let text = request_text(&request);
        self.requests.lock().unwrap().push(request.clone());

        let response = if text.contains(self.completion_marker) {
            // 完了結果を同じ因果的 turn で受け取った時だけ最終回答する。
            text_response(self.final_text)
        } else if has_tool_role(&request) {
            // dispatch 直後はrunningしか見えていない。LLM自身が継続を選ぶ。
            text_response("CONTINUE")
        } else if text.contains(REQUEST_MARKER)
            && !self
                .dispatched
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // 元発言は一度だけtool dispatchする。古い入力を再解釈して再実行しない。
            self.tool_calls_emitted
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            shell_with_content_response(self.holding_text, "sh", &["-c", self.script])
        } else {
            text_response("NO_REPLY")
        };

        self.active_calls
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        Ok(response)
    }
}

#[tokio::test]
async fn issue_975_completion_arriving_during_continue_is_folded_into_same_turn_once() {
    let buf = install_capture();
    let mock = Arc::new(SameTurnCompletionMock {
        script: SHELL_SCRIPT,
        completion_marker: COMPLETION_EVIDENCE,
        holding_text: HOLDING,
        final_text: FINAL,
        calls: std::sync::atomic::AtomicUsize::new(0),
        dispatched: std::sync::atomic::AtomicBool::new(false),
        tool_calls_emitted: std::sync::atomic::AtomicUsize::new(0),
        active_calls: std::sync::atomic::AtomicUsize::new(0),
        max_active_calls: std::sync::atomic::AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let mut tools = shell_enabled_tools_config();
    tools
        .shell
        .as_mut()
        .expect("shell config")
        .allowed_commands
        .push("sh".to_string());
    *core.state.tools_config.write().unwrap() = tools;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;
    fixture.append_message(ORIGIN, &format!("{REQUEST_MARKER} このURLを要約して"));

    let completed = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(FINAL))
        })
        .await
    };
    assert!(
        completed,
        "background completionを受けた最終回答が配送されない: {:?}",
        captured(&buf)
    );
    // 元completion callbackが予約した独立resumeが、turn解放後に二重起動しないことも観測する。
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        mock.calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "初回→runningを見た継続判断→completedを見た最終回答の3 stepだけで完了する"
    );

    // 独立した次turnでは、完了本文を再掲せず永続参照へ縮退する。
    fixture.append_message("97502", &format!("{FOLLOWUP_MARKER} 次の質問"));
    let followup_seen = {
        let mock = mock.clone();
        wait_until(move || {
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request_text(request).contains(FOLLOWUP_MARKER))
        })
        .await
    };
    assert!(followup_seen, "後続turnがLLMへ届かない");

    let requests = mock.requests.lock().unwrap().clone();
    let request_texts = requests.iter().map(request_text).collect::<Vec<_>>();
    let result_requests = request_texts
        .iter()
        .filter(|request| request.contains(COMPLETION_EVIDENCE))
        .count();
    assert_eq!(
        result_requests, 1,
        "一つのcompletion eventはLLMへ新規結果として一度だけ渡す: {request_texts:#?}"
    );
    assert_eq!(
        mock.tool_calls_emitted
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "元依頼から同じtoolを再実行しない"
    );
    assert_eq!(
        mock.max_active_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "同一sessionのprovider呼び出しを並行実行しない"
    );
    assert!(
        request_texts
            .iter()
            .all(|request| request.matches(REQUEST_MARKER).count() == 1),
        "各LLM入力で元発言を重複挿入しない: {request_texts:#?}"
    );

    // 検証対象はDB表現ではなく、provider mockへ実際に渡ったChatRequest.messagesそのもの。
    let llm_logs = requests
        .iter()
        .map(|request| serde_json::to_value(&request.messages).unwrap())
        .collect::<Vec<_>>();
    let first = llm_logs[0].as_array().unwrap();
    let running = llm_logs[1].as_array().unwrap();
    let completed = llm_logs[2].as_array().unwrap();
    let followup = llm_logs
        .iter()
        .find(|messages| messages.to_string().contains(FOLLOWUP_MARKER))
        .and_then(serde_json::Value::as_array)
        .expect("後続turnの実ChatRequest.messages");

    assert_eq!(first.len(), 2, "初回LLM会話ログはsystem+元発言だけ: {first:#?}");
    assert_eq!(&running[..2], &first[..], "2回目で初回会話ログを書き換えない");
    assert_eq!(running.len(), 4, "2回目はassistant callとrunningだけを追記: {running:#?}");
    assert_eq!(
        running[2]["role"].as_str(),
        Some("assistant"),
        "3行目はassistant tool call"
    );
    assert_eq!(running[2]["content"].as_str(), Some(HOLDING));
    assert_eq!(running[2]["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(running[2]["tool_calls"][0]["id"].as_str(), Some("t1"));
    assert_eq!(
        running[2]["tool_calls"][0]["function"]["name"].as_str(),
        Some("execute_shell")
    );
    let actual_args: serde_json::Value = serde_json::from_str(
        running[2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        actual_args,
        serde_json::json!({"command": "sh", "args": ["-c", SHELL_SCRIPT]}),
        "LLMへ渡すcall引数も元呼び出しと一致する"
    );
    assert_eq!(
        running[3],
        serde_json::json!({
            "role": "tool",
            "content": "[<t1] status:running tool:execute_shell",
            "tool_call_id": "t1",
        }),
        "2回目にLLMが見る結果は同じt1のrunningだけ"
    );

    assert_eq!(&completed[..4], &running[..], "3回目でも既存の因果ログを同一順序で保持する");
    assert_eq!(completed.len(), 5, "3回目はcompletion一行だけを追記: {completed:#?}");
    assert_eq!(
        completed[4],
        serde_json::json!({
            "role": "user",
            "content": "[<t1] status:completed\n終了コード 0・出力: issue975-tool-result-once\n",
        }),
        "3回目にLLMへ渡す実会話ログは同じt1と実結果本文を含む"
    );
    let followup_wire = serde_json::Value::Array(followup.clone()).to_string();
    assert!(
        followup_wire.contains("[<t1] status:completed result_omitted:true"),
        "後続turnはcompletion参照を保持する: {followup:#?}"
    );
    assert!(
        !followup_wire.contains(COMPLETION_EVIDENCE),
        "後続turnへ結果本文を再掲しない: {followup:#?}"
    );
    assert!(
        followup_wire.contains("read_my_history(around_id="),
        "後続turnの省略表現に永続参照が必要: {followup:#?}"
    );

    let wire = llm_logs
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>();
    assert!(
        wire.iter().skip(1).all(|request| !request.contains("tc-")),
        "provider call IDをLLM会話ログへ露出しない: {wire:#?}"
    );
    assert!(
        requests.iter().all(|request| request.messages.iter().skip(1).all(|message| {
            message
                .text_content()
                .is_none_or(|text| !text.lines().any(|line| line.trim() == "CONTINUE"))
        })),
        "制御記号CONTINUEをLLM会話ログへ保存しない: {wire:#?}"
    );

    let holding_count = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(HOLDING))
        .count();
    let final_count = captured(&buf)
        .iter()
        .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(FINAL))
        .count();
    assert_eq!(holding_count, 1, "途中発言を重複配送しない");
    assert_eq!(final_count, 1, "最終回答を重複配送しない");
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL)
            .all(|c| !c.body.contains("CONTINUE")),
        "CONTINUEは制御記号でありDiscordへ配送しない"
    );
}

#[tokio::test]
async fn issue_975_failed_background_tool_reaches_actual_llm_log_as_failed_not_completed() {
    let buf = install_capture();
    let mock = Arc::new(SameTurnCompletionMock {
        script: FAILED_SCRIPT,
        completion_marker: "status:failed",
        holding_text: FAILED_HOLDING,
        final_text: FAILED_FINAL,
        calls: std::sync::atomic::AtomicUsize::new(0),
        dispatched: std::sync::atomic::AtomicBool::new(false),
        tool_calls_emitted: std::sync::atomic::AtomicUsize::new(0),
        active_calls: std::sync::atomic::AtomicUsize::new(0),
        max_active_calls: std::sync::atomic::AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let mut tools = shell_enabled_tools_config();
    tools
        .shell
        .as_mut()
        .expect("shell config")
        .allowed_commands
        .push("sh".to_string());
    *core.state.tools_config.write().unwrap() = tools;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;
    fixture.append_message("97502", &format!("{REQUEST_MARKER} 失敗する処理を確認して"));
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|entry| entry.kind == "say" && entry.body.contains(FAILED_FINAL))
        })
        .await
    };
    assert!(
        delivered,
        "失敗結果を見た最終回答が配送されない: capture={:?}, requests={:#?}",
        captured(&buf),
        mock.requests.lock().unwrap()
    );

    let requests = mock.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    let running = serde_json::to_value(&requests[1].messages).unwrap();
    assert_eq!(
        running.as_array().unwrap().last(),
        Some(&serde_json::json!({
            "role": "tool",
            "content": "[<t1] status:running tool:execute_shell",
            "tool_call_id": "t1",
        }))
    );
    let completed = serde_json::to_value(&requests[2].messages).unwrap();
    let terminal = completed.as_array().unwrap().last().unwrap();
    assert_eq!(terminal["role"].as_str(), Some("user"));
    let terminal_text = terminal["content"].as_str().unwrap();
    assert!(terminal_text.starts_with("[<t1] status:failed\n"));
    assert!(terminal_text.contains("issue975-failed"));
    assert!(terminal_text.contains("exit_code\\\":7") || terminal_text.contains("\"exit_code\":7"));
    assert!(!terminal_text.contains("status:completed"));
    // completion sinkが予約したturn終了処理まで完了させ、後続harnessケースへ持ち越さない。
    tokio::time::sleep(Duration::from_millis(400)).await;
}
