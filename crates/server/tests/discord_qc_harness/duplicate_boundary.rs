const DB_MARKER: &str = "GENERIC_BOUNDARY_DISPATCH";
const DB_INBOUND: &str = "Please run the configured background command and report its result.";
const DB_VISIBLE: &str = "I am starting the requested check now.";
const DB_FINAL: &str = "The background check completed successfully.";
const DB_TOOL_CALL_ID: &str = "tc-boundary-regression";
const DB_TOOL_OUTPUT: &str = "boundary-regression-ok";

struct DuplicateBoundaryProvider {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    emitted: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl LlmProvider for DuplicateBoundaryProvider {
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
        let has_tool_result = request.messages.iter().any(|message| message.role == Role::Tool);
        self.requests.lock().unwrap().push(request);
        if has_tool_result {
            return Ok(text_response("NO_REPLY"));
        }
        if !self.emitted.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let mut response = shell_with_content_response(DB_VISIBLE, "echo", &[DB_TOOL_OUTPUT]);
            response.choices[0].message.tool_calls.as_mut().unwrap()[0].id =
                DB_TOOL_CALL_ID.to_string();
            return Ok(response);
        }
        Ok(text_response(&format!("{DB_FINAL}\nNO_REPLY")))
    }
}

fn message_text(message: &Message) -> String {
    match message.content.as_ref() {
        Some(MessageContent::Text(text)) => text.clone(),
        Some(MessageContent::Multi(parts)) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[tokio::test]
async fn canonical_history_survives_visible_speech_and_native_dispatch_end_to_end() {
    let buf = install_capture();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(DuplicateBoundaryProvider {
        requests: requests.clone(),
        emitted: std::sync::atomic::AtomicBool::new(false),
    });
    let core = start_core(provider as Arc<dyn LlmProvider>).await;
    *core.state.tools_config.write().unwrap() = shell_enabled_tools_config();

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;
    fixture.append_message("926001", &format!("{DB_MARKER} {DB_INBOUND}"));

    let completed = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf).iter().any(|event| {
                event.kind == "say" && event.channel == CHANNEL && event.body.contains(DB_FINAL)
            })
        })
        .await
    };

    let request_snapshot = requests.lock().unwrap().clone();
    assert!(
        completed,
        "post-response path did not dispatch, persist completion, resume, and deliver the final result; requests={}, visible_says={}, failure_reactions={}, first_request_boundary_mentions={} (the regression counts the system guidance pair plus the canonical user-history pair as two boundaries)",
        request_snapshot.len(),
        captured(&buf).iter().filter(|event| event.kind == "say" && event.body.contains(DB_VISIBLE)).count(),
        captured(&buf).iter().filter(|event| event.kind == "system_reaction" && event.emoji.contains('❌')).count(),
        request_snapshot.first().map(|request| request.messages.iter().map(message_text).map(|text| text.matches("<conversation_history>").count()).sum::<usize>()).unwrap_or(0),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    let request_snapshot = requests.lock().unwrap().clone();
    assert_eq!(request_snapshot.len(), 3, "one initial request, one post-dispatch request, and one completion resume request");
    for request in &request_snapshot {
        let user_history = request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(user_history.matches("<conversation_history>").count(), 1, "{request:?}");
        assert_eq!(user_history.matches("</conversation_history>").count(), 1, "{request:?}");
        assert!(user_history.find("<conversation_history>").unwrap() < user_history.find("</conversation_history>").unwrap());
        assert_eq!(user_history.matches(DB_MARKER).count(), 1, "exact inbound once: {request:?}");
    }

    let post_dispatch = &request_snapshot[1];
    let user_history = post_dispatch
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(user_history.matches(DB_VISIBLE).count(), 1, "visible speech once inside canonical history: {post_dispatch:?}");
    let assistant_index = post_dispatch
        .messages
        .iter()
        .position(|message| message.role == Role::Assistant && message.tool_calls.as_ref().is_some_and(|calls| calls.iter().any(|call| call.id == DB_TOOL_CALL_ID)))
        .expect("native assistant tool_call with original id");
    assert_eq!(post_dispatch.messages[assistant_index + 1].role, Role::Tool);
    assert_eq!(post_dispatch.messages[assistant_index + 1].tool_call_id.as_deref(), Some(DB_TOOL_CALL_ID));

    let events = captured(&buf);
    assert_eq!(events.iter().filter(|event| event.kind == "say" && event.body.contains(DB_VISIBLE)).count(), 1, "visible speech delivered once");
    assert_eq!(events.iter().filter(|event| event.kind == "say" && event.body.contains(DB_FINAL)).count(), 1, "final result delivered once");
    assert_eq!(events.iter().filter(|event| event.kind == "system_reaction" && event.emoji.contains('❌')).count(), 0, "no turn_failed reaction");

    let conn = core.extgate.db.lock().unwrap();
    let session_id: String = conn.query_row(
        "SELECT session_id FROM memory_sessions WHERE content LIKE ?1 LIMIT 1",
        [format!("%{DB_MARKER}%")],
        |row| row.get(0),
    ).unwrap();
    let dispatches: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id=?1 AND log_type='tool_call'",
        [&session_id],
        |row| row.get(0),
    ).unwrap();
    let completions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id=?1 AND log_type='system' AND content LIKE '%subtask_completed%'",
        [&session_id],
        |row| row.get(0),
    ).unwrap();
    let visible_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id=?1 AND log_type='speech' AND content=?2",
        rusqlite::params![session_id, DB_VISIBLE],
        |row| row.get(0),
    ).unwrap();
    let final_rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id=?1 AND log_type='speech' AND content=?2",
        rusqlite::params![session_id, DB_FINAL],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(dispatches, 1, "dispatch persisted exactly once");
    assert_eq!(completions, 1, "completion persisted exactly once and therefore resumed once");
    assert_eq!(visible_rows, 1, "visible speech persisted once");
    assert_eq!(final_rows, 1, "final reply persisted once");
}
