// D-1087: `run_agent_response` wires the run's `tool_event_sink` into the executor, so every
// tool execution of the run reaches the observer as `started` then its terminal state, in
// occurrence order. This is the production seam (not a helper): the sink is installed on the
// `RunRequest` exactly as the extgate turn installs it.

/// One observed tool event, reduced to what the test compares.
#[derive(Clone, Debug, PartialEq)]
struct ObservedToolEvent {
    tool: String,
    call_id: String,
    status: &'static str,
    depth: u32,
    has_duration: bool,
    has_result: bool,
}

#[derive(Default)]
struct RecordingToolSink {
    events: std::sync::Mutex<Vec<ObservedToolEvent>>,
}

impl RecordingToolSink {
    fn events(&self) -> Vec<ObservedToolEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl opencrab_actions::ToolEventSink for RecordingToolSink {
    fn on_event(&self, event: &opencrab_actions::ToolEvent<'_>) {
        let status = match event.status {
            opencrab_actions::ToolEventStatus::Started => "started",
            opencrab_actions::ToolEventStatus::Completed => "completed",
            opencrab_actions::ToolEventStatus::Failed => "failed",
            opencrab_actions::ToolEventStatus::Rejected => "rejected",
        };
        self.events.lock().unwrap().push(ObservedToolEvent {
            tool: event.tool_name.to_string(),
            call_id: event.tool_call_id.to_string(),
            status,
            depth: event.depth,
            has_duration: event.duration_ms.is_some(),
            has_result: event.result.is_some(),
        });
    }
}

/// The run's observer sees each executed tool in occurrence order: `started` then `completed`.
#[tokio::test]
async fn run_agent_response_delivers_each_tool_execution_to_the_run_sink() {
    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "SinkSeen", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");
    insert_member_session(&state, &session_id, &agent_id);

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-sink-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "describe_tools".to_string(),
            arguments: serde_json::json!({"names": ["reply"]}).to_string(),
        },
    }]);
    mock.push_text_response("できたよ⚡\nNO_REPLY");

    let sink = Arc::new(RecordingToolSink::default());
    let req = opencrab_actions::RunRequest::new(
        &agent_id,
        "SinkSeen",
        &session_id,
        "system",
        "user: 状態を教えて",
        "rest",
        opencrab_actions::CallerIdentity::Agent,
    )
    .with_tool_event_sink(Arc::clone(&sink) as Arc<dyn opencrab_actions::ToolEventSink>);

    opencrab_server::process::run_agent_response(&state, req)
        .await
        .expect("turn ran");

    let events = sink.events();
    assert_eq!(
        events
            .iter()
            .map(|e| (e.tool.as_str(), e.status))
            .collect::<Vec<_>>(),
        vec![("describe_tools", "started"), ("describe_tools", "completed")],
        "{events:?}"
    );
    assert_eq!(events[0].call_id, "tc-sink-1");
    assert_eq!(events[1].call_id, "tc-sink-1");
    assert!(events.iter().all(|e| e.depth == 0));
    assert!(!events[0].has_duration && !events[0].has_result, "{events:?}");
    assert!(events[1].has_duration && events[1].has_result, "{events:?}");
}

/// Without an observer the turn is unchanged: no sink, no events, the turn still runs.
#[tokio::test]
async fn run_agent_response_without_a_sink_is_unchanged() {
    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "NoSink", "TestPersona").await;
    let session_id = format!("agent-msg-{agent_id}-u1");
    insert_member_session(&state, &session_id, &agent_id);

    mock.push_text_response("そのまま⚡\nNO_REPLY");
    let req = opencrab_actions::RunRequest::new(
        &agent_id,
        "NoSink",
        &session_id,
        "system",
        "user: こんにちは",
        "rest",
        opencrab_actions::CallerIdentity::Agent,
    );
    assert!(req.tool_event_sink.is_none());

    let result = opencrab_server::process::run_agent_response(&state, req)
        .await
        .expect("turn ran");
    assert!(result.response.contains("そのまま"), "{result:?}");
}

/// Records the subtask lifecycles the run reports (the neutral slot a gateway listens to).
#[derive(Default)]
struct RecordingLifecycleSink {
    events: std::sync::Mutex<
        Vec<(opencrab_actions::SubtaskLifecycleStatus, String, Option<String>)>,
    >,
}

impl RecordingLifecycleSink {
    fn statuses(&self) -> Vec<(opencrab_actions::SubtaskLifecycleStatus, String)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|(status, run_id, _)| (*status, run_id.clone()))
            .collect()
    }

    fn tasks(&self) -> Vec<Option<String>> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, task)| task.clone())
            .collect()
    }
}

impl opencrab_actions::SubtaskCompletionSink for RecordingLifecycleSink {
    fn session_prefix(&self) -> &'static str {
        ""
    }

    fn forwards_progress(&self) -> bool {
        false
    }

    fn deliver_continuation(&self, _ev: opencrab_actions::SubtaskSettled) {}

    fn on_subtask_lifecycle(&self, ev: &opencrab_actions::SubtaskLifecycleEvent<'_>) {
        self.events.lock().unwrap().push((
            ev.status,
            ev.subtask_id.to_string(),
            (!ev.task.is_empty()).then(|| ev.task.to_string()),
        ));
    }
}

/// `spawn_subtask` reports `started` (with the task) before the run, and the settle reports
/// `completed` — both through the neutral sink slot, in that order.
#[tokio::test]
async fn spawn_subtask_reports_started_then_completed_to_the_lifecycle_observer() {
    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "Lifecycle", "TestPersona").await;
    let session_id = format!("web-{agent_id}-u-lifecycle");
    insert_member_session(&state, &session_id, &agent_id);

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-spawn".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "spawn_subtask".to_string(),
            arguments: serde_json::json!({"task": "短い仕事", "label": "short job"}).to_string(),
        },
    }]);
    mock.push_text_response("投げたよ⚡\nNO_REPLY");
    // The subtask's own run.
    mock.push_text_response("仕事おわった⚡\nNO_REPLY");

    let capture = Arc::new(RecordingLifecycleSink::default());
    let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = capture.clone();
    let run_req = opencrab_actions::RunRequest::new(
        &agent_id,
        "Lifecycle",
        &session_id,
        "system",
        "user: これやって",
        "web",
        opencrab_actions::CallerIdentity::Agent,
    )
    .with_dispatch(
        Some(state.subtask_registries.registry_for(&session_id)),
        sink,
    );
    opencrab_server::process::run_agent_response(&state, run_req)
        .await
        .expect("spawn する run が失敗した");

    // The subtask settles in its own task; poll briefly (it is normally immediate).
    for _ in 0..250 {
        if capture
            .statuses()
            .iter()
            .any(|(status, _)| *status == opencrab_actions::SubtaskLifecycleStatus::Completed)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let statuses = capture.statuses();
    assert_eq!(
        statuses
            .iter()
            .map(|(status, _)| *status)
            .collect::<Vec<_>>(),
        vec![
            opencrab_actions::SubtaskLifecycleStatus::Started,
            opencrab_actions::SubtaskLifecycleStatus::Completed,
        ],
        "{statuses:?}"
    );
    assert_eq!(statuses[0].1, statuses[1].1, "same run id for both events");
    assert_eq!(
        capture.tasks()[0].as_deref(),
        Some("短い仕事"),
        "started carries the task; the terminal event does not need it"
    );
    assert!(capture.tasks()[1].is_none());
}

/// `report_progress` reports `progress` with the message as its detail.
#[tokio::test]
async fn report_progress_reports_a_progress_lifecycle() {
    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "Progress", "TestPersona").await;
    let session_id = format!("web-{agent_id}-u-progress");
    insert_member_session(&state, &session_id, &agent_id);

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-spawn-p".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "spawn_subtask".to_string(),
            arguments: serde_json::json!({"task": "長い仕事", "label": "long job"}).to_string(),
        },
    }]);
    mock.push_text_response("投げたよ⚡\nNO_REPLY");
    // The subtask reports progress, then finishes.
    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-progress".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "report_progress".to_string(),
            arguments: serde_json::json!({"message": "半分おわった"}).to_string(),
        },
    }]);
    mock.push_text_response("進めた⚡\nNO_REPLY");

    let capture = Arc::new(RecordingLifecycleSink::default());
    let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = capture.clone();
    let run_req = opencrab_actions::RunRequest::new(
        &agent_id,
        "Progress",
        &session_id,
        "system",
        "user: これやって",
        "web",
        opencrab_actions::CallerIdentity::Agent,
    )
    .with_dispatch(
        Some(state.subtask_registries.registry_for(&session_id)),
        sink,
    );
    opencrab_server::process::run_agent_response(&state, run_req)
        .await
        .expect("spawn する run が失敗した");

    for _ in 0..250 {
        if capture
            .statuses()
            .iter()
            .any(|(status, _)| *status == opencrab_actions::SubtaskLifecycleStatus::Progress)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let statuses = capture
        .statuses()
        .iter()
        .map(|(status, _)| *status)
        .collect::<Vec<_>>();
    assert!(
        statuses.contains(&opencrab_actions::SubtaskLifecycleStatus::Progress),
        "{statuses:?}"
    );
}
