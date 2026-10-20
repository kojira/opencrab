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

/// 観測の記録先（ライフサイクルとツール実行の両方）。
///
/// D-1087: 1 つの記録先で「走行のライフサイクル」と「その走行の中で実行されたツール」を
/// 発生順に並べる。run key が突き合うことと順序を 1 箇所で見るため。
/// 走行の中で実行されたツール 1 件の観測値（`(tool, status, depth, run key)`）。
type ObservedRunTool = (String, &'static str, u32, Option<String>);

#[derive(Default)]
struct RecordingInner {
    events: std::sync::Mutex<
        Vec<(opencrab_actions::SubtaskLifecycleStatus, String, Option<String>)>,
    >,
    /// `events` と同じ index の `sub_session_id`。
    run_keys: std::sync::Mutex<Vec<String>>,
    /// `(tool, status, depth, sub_session_id)`。
    tools: std::sync::Mutex<Vec<ObservedRunTool>>,
    /// 両方を発生順に並べた `lifecycle:<status>` / `tool:<name>:<status>`。
    order: std::sync::Mutex<Vec<String>>,
}

/// 走行のライフサイクルを受ける側（`SubtaskCompletionSink`）。ツール実行の観測は
/// [`RecordingToolSink`] に渡し、同じ [`RecordingInner`] へ記録する。
#[derive(Default)]
struct RecordingLifecycleSink {
    inner: Arc<RecordingInner>,
}

impl RecordingLifecycleSink {
    fn statuses(&self) -> Vec<(opencrab_actions::SubtaskLifecycleStatus, String)> {
        self.inner
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|(status, run_id, _)| (*status, run_id.clone()))
            .collect()
    }

    fn tasks(&self) -> Vec<Option<String>> {
        self.inner
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, task)| task.clone())
            .collect()
    }

    fn run_keys(&self) -> Vec<String> {
        self.inner.run_keys.lock().unwrap().clone()
    }

    fn tools(&self) -> Vec<ObservedRunTool> {
        self.inner.tools.lock().unwrap().clone()
    }

    fn order(&self) -> Vec<String> {
        self.inner.order.lock().unwrap().clone()
    }
}

/// ツール実行の観測を受ける側。`RecordingLifecycleSink` と同じ記録先へ書く。
struct RunToolSink {
    inner: Arc<RecordingInner>,
}

impl opencrab_actions::ToolEventSink for RunToolSink {
    fn on_event(&self, event: &opencrab_actions::ToolEvent<'_>) {
        let status = match event.status {
            opencrab_actions::ToolEventStatus::Started => "started",
            opencrab_actions::ToolEventStatus::Completed => "completed",
            opencrab_actions::ToolEventStatus::Failed => "failed",
            opencrab_actions::ToolEventStatus::Rejected => "rejected",
        };
        self.inner.tools.lock().unwrap().push((
            event.tool_name.to_string(),
            status,
            event.depth,
            event.session_id.map(str::to_string),
        ));
        self.inner
            .order
            .lock()
            .unwrap()
            .push(format!("tool:{}:{status}", event.tool_name));
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
        self.inner.events.lock().unwrap().push((
            ev.status,
            ev.subtask_id.to_string(),
            (!ev.task.is_empty()).then(|| ev.task.to_string()),
        ));
        self.inner
            .run_keys
            .lock()
            .unwrap()
            .push(ev.sub_session_id.to_string());
        self.inner
            .order
            .lock()
            .unwrap()
            .push(format!("lifecycle:{}", ev.status.as_str()));
    }

    /// D-1087: 配信元（extgate）と同じく、この走行の活動 stream を副実行へ引き継ぐ。
    /// 副実行のツール実行も同じ記録先に載る。
    fn tool_event_sink(&self) -> Option<Arc<dyn opencrab_actions::ToolEventSink>> {
        Some(Arc::new(RunToolSink {
            inner: Arc::clone(&self.inner),
        }))
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

/// D-1087: サブタスクの**中で**実行されたツールも観測に載る（親ターンだけでなく）。
///
/// 走行のツール実行は、その走行のライフサイクルと同じ run key（`sub_session_id`）と、
/// 親より 1 つ深い `depth` を名乗る。これが無いと、gateway はサブタスクの中で何が
/// 起きたかを並べられない。順序は 開始 → 中のツール → 終端。
#[tokio::test]
async fn tools_inside_a_subtask_are_observed_with_the_subtask_run_key() {
    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "NestedTools", "TestPersona").await;
    let session_id = format!("web-{agent_id}-u-nested");
    insert_member_session(&state, &session_id, &agent_id);

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-spawn-n".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "spawn_subtask".to_string(),
            arguments: serde_json::json!({"task": "二つのツールを走らせる", "label": "two tools"})
                .to_string(),
        },
    }]);
    mock.push_text_response("投げたよ⚡\nNO_REPLY");
    // サブタスク自身が 2 つのツールを呼ぶ（単一バッチ）。
    mock.push_tool_call_response(vec![
        ToolCall {
            id: "tc-sub-1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "describe_tools".to_string(),
                arguments: serde_json::json!({"names": ["reply"]}).to_string(),
            },
        },
        ToolCall {
            id: "tc-sub-2".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "describe_tools".to_string(),
                arguments: serde_json::json!({"names": ["reaction"]}).to_string(),
            },
        },
    ]);
    mock.push_text_response("中の仕事も終わった⚡\nNO_REPLY");

    let capture = Arc::new(RecordingLifecycleSink::default());
    let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = capture.clone();
    let tool_sink: Arc<dyn opencrab_actions::ToolEventSink> = Arc::new(RunToolSink {
        inner: Arc::clone(&capture.inner),
    });
    let run_req = opencrab_actions::RunRequest::new(
        &agent_id,
        "NestedTools",
        &session_id,
        "system",
        "user: これやって",
        "web",
        opencrab_actions::CallerIdentity::Agent,
    )
    .with_dispatch(
        Some(state.subtask_registries.registry_for(&session_id)),
        sink,
    )
    .with_tool_event_sink(tool_sink);
    opencrab_server::process::run_agent_response(&state, run_req)
        .await
        .expect("spawn する run が失敗した");

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

    let run_key = capture.run_keys().first().cloned().expect("started run key");
    assert!(run_key.starts_with("subtask-"), "{run_key}");
    let nested: Vec<_> = capture
        .tools()
        .into_iter()
        .filter(|(tool, _, _, _)| tool == "describe_tools")
        .collect();
    assert_eq!(nested.len(), 4, "2 ツール × (started, completed): {nested:?}");
    for (tool, status, depth, session) in &nested {
        assert_eq!(*depth, 1, "{tool} {status}: 親の 1 つ下");
        assert_eq!(
            session.as_deref(),
            Some(run_key.as_str()),
            "{tool} {status}: 走行の run key を名乗る"
        );
    }
    // 親の `spawn_subtask`（制御ツール・inline）の間に走行が始まり、中のツールが走り、
    // 走行が終端する。中のツール実行は必ず開始と終端のあいだに、2 本とも順に並ぶ。
    assert_eq!(
        capture.order(),
        vec![
            "tool:spawn_subtask:started".to_string(),
            "lifecycle:started".to_string(),
            "tool:spawn_subtask:completed".to_string(),
            "tool:describe_tools:started".to_string(),
            "tool:describe_tools:completed".to_string(),
            "tool:describe_tools:started".to_string(),
            "tool:describe_tools:completed".to_string(),
            "lifecycle:completed".to_string(),
        ],
        "開始 → 中のツール → 終端の順"
    );
}

/// D-1087: 自動 dispatch された背景バッチも、同じ「開始 → ツール → 終端」で観測に載る。
///
/// 配線点は `process.rs` の `SubtaskToolDispatcher::with_depth` にしかなく、そこを外すと
/// バッチのツール実行が depth 0（親ターン）と名乗って走行へ突き合わせられなくなる。
#[tokio::test]
async fn a_dispatched_batch_is_observed_as_a_run_with_its_tools() {
    let (app, _db, mock, state) = create_test_app_with_state();
    let (agent_id, _app) = create_test_agent_named(app, "BatchRun", "TestPersona").await;
    let session_id = format!("web-{agent_id}-u-batch");
    insert_member_session(&state, &session_id, &agent_id);

    mock.push_tool_call_response(vec![ToolCall {
        id: "tc-batch-1".to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "summarize_and_save".to_string(),
            arguments: serde_json::json!({"content": "c", "filename": "notes/batch.md"})
                .to_string(),
        },
    }]);
    mock.push_text_response("裏で走らせた⚡\nNO_REPLY");

    let capture = Arc::new(RecordingLifecycleSink::default());
    let sink: Arc<dyn opencrab_actions::SubtaskCompletionSink> = capture.clone();
    let tool_sink: Arc<dyn opencrab_actions::ToolEventSink> = Arc::new(RunToolSink {
        inner: Arc::clone(&capture.inner),
    });
    let run_req = opencrab_actions::RunRequest::new(
        &agent_id,
        "BatchRun",
        &session_id,
        "system",
        "user: 覚えて",
        "web",
        opencrab_actions::CallerIdentity::Agent,
    )
    .with_dispatch(
        Some(state.subtask_registries.registry_for(&session_id)),
        sink,
    )
    .with_tool_event_sink(tool_sink);
    opencrab_server::process::run_agent_response(&state, run_req)
        .await
        .expect("dispatch する run が失敗した");

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

    let statuses: Vec<_> = capture
        .statuses()
        .into_iter()
        .map(|(status, _)| status)
        .collect();
    assert_eq!(
        statuses,
        vec![
            opencrab_actions::SubtaskLifecycleStatus::Started,
            opencrab_actions::SubtaskLifecycleStatus::Completed,
        ],
        "{statuses:?}"
    );
    let run_key = capture.run_keys()[0].clone();
    assert_eq!(capture.run_keys()[1], run_key, "開始と終端は同じ走行");
    let batch_tools: Vec<_> = capture
        .tools()
        .into_iter()
        .filter(|(tool, _, _, _)| tool == "summarize_and_save")
        .collect();
    assert_eq!(batch_tools.len(), 2, "{batch_tools:?}");
    for (tool, status, depth, session) in &batch_tools {
        assert_eq!(*depth, 1, "{tool} {status}: 親の 1 つ下");
        assert_eq!(
            session.as_deref(),
            Some(run_key.as_str()),
            "{tool} {status}: バッチの run key を名乗る"
        );
    }
    assert_eq!(
        capture.order(),
        vec![
            "lifecycle:started".to_string(),
            "tool:summarize_and_save:started".to_string(),
            "tool:summarize_and_save:completed".to_string(),
            "lifecycle:completed".to_string(),
        ],
        "開始 → ツール → 終端の順"
    );
}
