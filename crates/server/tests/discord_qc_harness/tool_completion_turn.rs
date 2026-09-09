// Issue #975: provider mockへ実際に渡ったChatRequest.messagesを正本に、background
// completionの本文がresume requestへ一度だけ入り、応答後は永続参照へ縮退することを検証する。

const ORIGIN: &str = "97501";
const REQUEST_MARKER: &str = "ISSUE975-SUMMARIZE";
const HOLDING: &str = "確認しているので少し待ってね";
const FINAL: &str = "取得結果を確認して要約したよ";
const FOLLOWUP_MARKER: &str = "ISSUE975-FOLLOWUP";

struct CompletionHistoryMock {
    script: String,
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
impl LlmProvider for CompletionHistoryMock {
    fn name(&self) -> &str {
        "mock"
    }

    fn sends_max_output_tokens(&self) -> bool {
        false
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
        tokio::time::sleep(Duration::from_millis(100)).await;

        let text = request_text(&request);
        self.requests.lock().unwrap().push(request.clone());
        let response = if text.contains(self.completion_marker) {
            text_response(self.final_text)
        } else if has_tool_role(&request) {
            // background実行中のrunはいったん静かに終わる。既存completion sinkが、DBへ
            // 永続化された結果から会話を再構築してresumeする。
            text_response("NO_REPLY")
        } else if text.contains(REQUEST_MARKER)
            && !self
                .dispatched
                .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.tool_calls_emitted
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            shell_with_content_response(self.holding_text, "sh", &["-c", &self.script])
        } else {
            text_response("NO_REPLY")
        };

        self.active_calls
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        Ok(response)
    }
}

fn enable_sh(core: &Core) {
    let mut tools = shell_enabled_tools_config();
    tools
        .shell
        .as_mut()
        .expect("shell config")
        .allowed_commands
        .push("sh".to_string());
    *core.state.tools_config.write().unwrap() = tools;
}

#[tokio::test]
async fn issue_975_completion_is_full_in_resume_request_once_then_becomes_reference() {
    let buf = install_capture();
    let counter_dir = tempfile::tempdir().unwrap();
    let counter_path = counter_dir.path().join("shell-runs");
    let script = format!(
        "printf 'run\\n' >> '{}'; echo issue975-tool-result-once",
        counter_path.display()
    );
    let mock = Arc::new(CompletionHistoryMock {
        script,
        completion_marker: "issue975-tool-result-once",
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
    enable_sh(&core);

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;
    fixture.append_message(ORIGIN, &format!("{REQUEST_MARKER} このURLを要約して"));
    let completed = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|entry| entry.kind == "say" && entry.body.contains(FINAL))
        })
        .await
    };
    assert!(completed, "completionを読んだ最終回答が配送されない");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(mock.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(
        std::fs::read_to_string(&counter_path)
            .unwrap()
            .lines()
            .count(),
        1,
        "実shellプロセスの副作用でexactly-onceを検証する"
    );
    assert_eq!(
        mock.tool_calls_emitted
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(
        mock.max_active_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "同一sessionのLLM処理は直列"
    );

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
    assert!(followup_seen);

    let requests = mock.requests.lock().unwrap();
    let result_requests = requests
        .iter()
        .filter(|request| {
            let text = request_text(request);
            text.contains("status:completed") && text.contains("issue975-tool-result-once")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        result_requests.len(),
        1,
        "実結果本文はresume requestへ一度だけ載る: {:#?}",
        requests.iter().map(request_text).collect::<Vec<_>>()
    );
    let resume_wire = serde_json::to_string(&result_requests[0].messages).unwrap();
    assert!(resume_wire.contains("[tool_call]"));
    assert!(resume_wire.contains("[c1]: execute_shell"));
    assert!(resume_wire.contains("subtask s1"));
    assert!(resume_wire.contains("[s1 完了]"));
    assert!(resume_wire.contains("status:completed"));
    assert!(!resume_wire.contains("tc-"), "provider raw call IDを再構築履歴へ出さない");

    let followup = requests
        .iter()
        .find(|request| request_text(request).contains(FOLLOWUP_MARKER))
        .unwrap();
    let followup_wire = serde_json::to_string(&followup.messages).unwrap();
    assert!(!followup_wire.contains("issue975-tool-result-once"));
    assert!(followup_wire.contains("status:completed result_omitted:true"));
    assert!(followup_wire.contains("read_my_history(around_id="));
    assert!(requests.iter().all(|request| request.messages.iter().all(|message| {
        message
            .text_content()
            .is_none_or(|text| !text.lines().any(|line| line.trim() == "CONTINUE"))
    })));

    assert_eq!(
        captured(&buf)
            .iter()
            .filter(|entry| entry.kind == "say" && entry.body.contains(HOLDING))
            .count(),
        1
    );
    assert_eq!(
        captured(&buf)
            .iter()
            .filter(|entry| entry.kind == "say" && entry.body.contains(FINAL))
            .count(),
        1
    );
}

#[tokio::test]
async fn issue_975_slow_shell_still_executes_once_and_resumes_once() {
    let buf = install_capture();
    let counter_dir = tempfile::tempdir().unwrap();
    let counter_path = counter_dir.path().join("slow-shell-runs");
    let script = format!(
        "sleep 1; printf 'run\\n' >> '{}'; echo issue975-slow-result",
        counter_path.display()
    );
    let mock = Arc::new(CompletionHistoryMock {
        script,
        completion_marker: "issue975-slow-result",
        holding_text: "低速処理を待っているよ",
        final_text: "低速結果を確認したよ",
        calls: std::sync::atomic::AtomicUsize::new(0),
        dispatched: std::sync::atomic::AtomicBool::new(false),
        tool_calls_emitted: std::sync::atomic::AtomicUsize::new(0),
        active_calls: std::sync::atomic::AtomicUsize::new(0),
        max_active_calls: std::sync::atomic::AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    enable_sh(&core);
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;
    fixture.append_message("97503", &format!("{REQUEST_MARKER} 低速処理を確認して"));
    let delivered = {
        let buf = buf.clone();
        wait_until(move || captured(&buf).iter().any(|e| e.body.contains("低速結果を確認したよ")))
            .await
    };
    assert!(delivered);
    assert_eq!(mock.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(
        std::fs::read_to_string(counter_path)
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| {
                let text = request_text(request);
                text.contains("status:completed") && text.contains("issue975-slow-result")
            })
            .count(),
        1
    );
}

#[tokio::test]
async fn issue_975_nonzero_exit_is_failed_in_actual_resume_messages() {
    let buf = install_capture();
    let counter_dir = tempfile::tempdir().unwrap();
    let counter_path = counter_dir.path().join("failed-shell-runs");
    let script = format!(
        "printf 'run\\n' >> '{}'; printf issue975-failed >&2; exit 7",
        counter_path.display()
    );
    let mock = Arc::new(CompletionHistoryMock {
        script,
        completion_marker: "\"stderr\":\"issue975-failed\"",
        holding_text: "失敗処理を待っているよ",
        final_text: "失敗結果を確認したよ",
        calls: std::sync::atomic::AtomicUsize::new(0),
        dispatched: std::sync::atomic::AtomicBool::new(false),
        tool_calls_emitted: std::sync::atomic::AtomicUsize::new(0),
        active_calls: std::sync::atomic::AtomicUsize::new(0),
        max_active_calls: std::sync::atomic::AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    enable_sh(&core);
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;
    fixture.append_message("97504", &format!("{REQUEST_MARKER} 失敗を確認して"));
    let delivered = {
        let buf = buf.clone();
        wait_until(move || captured(&buf).iter().any(|e| e.body.contains("失敗結果を確認したよ")))
            .await
    };
    assert!(delivered);

    let requests = mock.requests.lock().unwrap();
    let resume = requests
        .iter()
        .find(|request| {
            let text = request_text(request);
            text.contains("status:failed") && text.contains("issue975-failed")
        })
        .unwrap_or_else(|| {
            panic!(
                "failed completion request missing: {:#?}",
                requests.iter().map(request_text).collect::<Vec<_>>()
            )
        });
    let wire = serde_json::to_string(&resume.messages).unwrap();
    assert!(wire.contains("status:failed"));
    assert!(wire.contains("exit_code\\\":7") || wire.contains("exit_code\\\": 7"));
    assert!(!wire.contains("status:completed"));
    assert_eq!(
        std::fs::read_to_string(counter_path)
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[tokio::test]
async fn issue_975_large_result_is_bounded_and_recoverable_in_resume_messages() {
    let buf = install_capture();
    let counter_dir = tempfile::tempdir().unwrap();
    let counter_path = counter_dir.path().join("large-shell-runs");
    let script = format!(
        "printf 'run\\n' >> '{}'; i=1; while [ $i -le 3000 ]; do echo issue975-large-$i; i=$((i+1)); done",
        counter_path.display()
    );
    let mock = Arc::new(CompletionHistoryMock {
        script,
        completion_marker: "Tool result withheld",
        holding_text: "大容量処理を待っているよ",
        final_text: "大容量結果の保存先を確認したよ",
        calls: std::sync::atomic::AtomicUsize::new(0),
        dispatched: std::sync::atomic::AtomicBool::new(false),
        tool_calls_emitted: std::sync::atomic::AtomicUsize::new(0),
        active_calls: std::sync::atomic::AtomicUsize::new(0),
        max_active_calls: std::sync::atomic::AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    enable_sh(&core);
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;
    fixture.append_message("97505", &format!("{REQUEST_MARKER} 大容量処理を確認して"));
    let delivered = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|e| e.body.contains("大容量結果の保存先を確認したよ"))
        })
        .await
    };
    assert!(delivered);

    let requests = mock.requests.lock().unwrap();
    let resume = requests
        .iter()
        .find(|request| request_text(request).contains("Tool result withheld"))
        .unwrap();
    let text = request_text(resume);
    assert!(text.contains("tmp/"));
    assert!(!text.contains("issue975-large-3000"));
    assert_eq!(
        std::fs::read_to_string(counter_path)
            .unwrap()
            .lines()
            .count(),
        1
    );
}
