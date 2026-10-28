// D-1087: a said turn writes the run's activity (tool executions, subtask lifecycles) as
// id-less `tool_activity` / `subtask_lifecycle` frames, in occurrence order, before `ended`.
//
// The conformance runtime stands in for the engine: it plays the steps below through the
// seams the production turn installs on the `RunRequest` (the executor's `ToolEventSink` and
// the completion sink's lifecycle slot).

/// One step of the faked engine's activity.
#[derive(Clone, Debug)]
enum ActivityStep {
    /// A tool execution: `started` then its terminal state (the executor calls the sink twice).
    Tool {
        tool: &'static str,
        call_id: &'static str,
        terminal: opencrab_actions::ToolEventStatus,
    },
    /// D-1096: a tool that has started and is still running when the turn is stopped (the
    /// executor wrote only `started`).
    RunningTool {
        tool: &'static str,
        call_id: &'static str,
    },
    /// One subtask lifecycle event.
    Lifecycle {
        run_id: &'static str,
        status: opencrab_actions::SubtaskLifecycleStatus,
        label: &'static str,
    },
}

#[derive(Clone, Default)]
struct ActivityScript {
    steps: Arc<Mutex<Vec<ActivityStep>>>,
    /// `RunRequest::tool_event_sink.is_some()` of every run, in run order.
    sink_seen: Arc<Mutex<Vec<bool>>>,
}

impl ActivityScript {
    fn set(&self, steps: Vec<ActivityStep>) {
        *self.steps.lock().unwrap() = steps;
    }

    fn play(
        &self,
        sink: &Option<Arc<dyn opencrab_actions::ToolEventSink>>,
        completion: &Option<Arc<dyn opencrab_actions::SubtaskCompletionSink>>,
    ) {
        self.sink_seen.lock().unwrap().push(sink.is_some());
        let steps = std::mem::take(&mut *self.steps.lock().unwrap());
        if steps.is_empty() {
            return;
        }
        let args = json!({"cmd": "ls -la"});
        let result = json!({"ok": true});
        for step in steps {
            match step {
                ActivityStep::Tool {
                    tool,
                    call_id,
                    terminal,
                } => {
                    let Some(sink) = sink else { continue };
                    let started = opencrab_actions::ToolEventStatus::Started;
                    for status in [started, terminal] {
                        let is_terminal = status != started;
                        sink.on_event(&opencrab_actions::ToolEvent {
                            tool_name: tool,
                            tool_call_id: call_id,
                            agent_id: "agent-1",
                            session_id: Some("sess"),
                            depth: 0,
                            status,
                            started_at: "2026-10-20T00:00:00Z",
                            duration_ms: is_terminal.then_some(7),
                            args: &args,
                            result: if is_terminal { Some(&result) } else { None },
                            error: matches!(
                                status,
                                opencrab_actions::ToolEventStatus::Failed
                                    | opencrab_actions::ToolEventStatus::Rejected
                            )
                            .then_some("boom"),
                        });
                    }
                }
                ActivityStep::RunningTool { tool, call_id } => {
                    let Some(sink) = sink else { continue };
                    sink.on_event(&opencrab_actions::ToolEvent {
                        tool_name: tool,
                        tool_call_id: call_id,
                        agent_id: "agent-1",
                        session_id: Some("sess"),
                        depth: 0,
                        status: opencrab_actions::ToolEventStatus::Started,
                        started_at: "2026-10-20T00:00:00Z",
                        duration_ms: None,
                        args: &args,
                        result: None,
                        error: None,
                    });
                }
                ActivityStep::Lifecycle {
                    run_id,
                    status,
                    label,
                } => {
                    let Some(completion) = completion else {
                        continue;
                    };
                    completion.on_subtask_lifecycle(&opencrab_actions::SubtaskLifecycleEvent {
                        subtask_id: run_id,
                        sub_session_id: "subtask-11111111-2222-3333-4444-555555555555",
                        parent_session_id: "sess",
                        agent_id: "agent-1",
                        status,
                        label,
                        task: if matches!(status, opencrab_actions::SubtaskLifecycleStatus::Started)
                        {
                            "do the thing"
                        } else {
                            ""
                        },
                        detail: matches!(status, opencrab_actions::SubtaskLifecycleStatus::Progress)
                            .then_some("half way"),
                        duration_ms: matches!(
                            status,
                            opencrab_actions::SubtaskLifecycleStatus::Completed
                                | opencrab_actions::SubtaskLifecycleStatus::Failed
                                | opencrab_actions::SubtaskLifecycleStatus::TimedOut
                                | opencrab_actions::SubtaskLifecycleStatus::Aborted
                        )
                        .then_some(42),
                        result_text: matches!(
                            status,
                            opencrab_actions::SubtaskLifecycleStatus::Completed
                        )
                        .then_some("the result"),
                    });
                }
            }
        }
    }
}

/// Frames up to the turn's `ended`, acknowledging every say. Unlike `turn_frames`, the bound
/// is wide enough for the long runs of this file.
async fn long_turn_frames(s: &mut UnixStream) -> Vec<Value> {
    let mut frames = Vec::new();
    for _ in 0..2000 {
        let Some(frame) = read_frame_opt(s).await else {
            continue;
        };
        if frame["m"] == "say" {
            write_frame(s, &json!({"id": frame["id"], "m": "ok"})).await;
        }
        let ended = frame["m"] == "activity" && frame["state"] == "ended";
        frames.push(frame);
        if ended {
            return frames;
        }
    }
    panic!("turn did not end within 2000 frames");
}

fn activity_frames(frames: &[Value]) -> Vec<Value> {
    frames
        .iter()
        .filter(|f| f["m"] == "tool_activity" || f["m"] == "subtask_lifecycle")
        .cloned()
        .collect()
}

/// A1: the turn's activity frames reach the gateway in occurrence order, before `ended`, and
/// carry the turn's `activity` id without an `id` of their own.
#[tokio::test]
async fn said_turn_writes_tool_activity_and_subtask_lifecycle_in_order_before_ended() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.activity_script.set(vec![
        ActivityStep::Tool {
            tool: "execute_shell",
            call_id: "call-1",
            terminal: opencrab_actions::ToolEventStatus::Completed,
        },
        ActivityStep::Lifecycle {
            run_id: "run-1",
            status: opencrab_actions::SubtaskLifecycleStatus::Started,
            label: "shell job",
        },
        ActivityStep::Lifecycle {
            run_id: "run-1",
            status: opencrab_actions::SubtaskLifecycleStatus::Progress,
            label: "shell job",
        },
        ActivityStep::Lifecycle {
            run_id: "run-1",
            status: opencrab_actions::SubtaskLifecycleStatus::Completed,
            label: "shell job",
        },
    ]);

    write_frame(&mut s, &said_frame_with_profile("a1", &binding_id, "o-a1", None)).await;
    assert_eq!(read_said_response(&mut s, "a1").await["m"], "ok");
    let frames = turn_frames(&mut s).await;
    let activity = activity_frames(&frames);

    let shape: Vec<String> = activity
        .iter()
        .map(|f| {
            format!(
                "{}:{}",
                f["m"].as_str().unwrap(),
                f["status"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            "tool_activity:started",
            "tool_activity:completed",
            "subtask_lifecycle:started",
            "subtask_lifecycle:progress",
            "subtask_lifecycle:completed",
        ],
        "{activity:?}"
    );
    // `ended` is last: the turn waits for the queue before closing the activity.
    assert_eq!(frames.last().unwrap()["m"], "activity");
    assert_eq!(frames.last().unwrap()["state"], "ended");

    let turn_activity_id = frames
        .iter()
        .find(|f| f["m"] == "activity" && f["state"] == "started")
        .map(|f| f["activity_id"].as_str().unwrap().to_string())
        .expect("started activity");
    for frame in &activity {
        assert!(frame.get("id").is_none(), "fire-and-forget frame: {frame}");
        assert_eq!(frame["binding_id"], json!(binding_id), "{frame}");
    }
    for frame in activity.iter().filter(|f| f["m"] == "tool_activity") {
        assert_eq!(frame["activity_id"], json!(turn_activity_id), "{frame}");
    }

    // Field presence is part of the contract: only the terminal state carries a duration,
    // the result and (for failed/rejected only) the error.
    let (started, completed) = (&activity[0], &activity[1]);
    assert_eq!(started["call_id"], "call-1");
    assert_eq!(started["tool"], "execute_shell");
    assert_eq!(started["depth"], 0);
    assert_eq!(started["args"], json!({"cmd": "ls -la"}));
    assert!(started.get("duration_ms").is_none(), "{started}");
    assert!(started.get("result").is_none(), "{started}");
    assert!(started.get("error").is_none(), "{started}");
    assert_eq!(completed["duration_ms"], 7);
    assert_eq!(completed["result"], json!({"ok": true}));
    assert!(completed.get("error").is_none(), "{completed}");

    let (lifecycle_started, progress, lifecycle_done) = (&activity[2], &activity[3], &activity[4]);
    assert_eq!(lifecycle_started["run_id"], "run-1");
    // 走行の相関キー。旧 gateway は `sessionKey` 行にこの値を出していたので、frame が運ぶ。
    assert_eq!(
        lifecycle_started["sub_session_id"],
        "subtask-11111111-2222-3333-4444-555555555555"
    );
    assert_eq!(progress["sub_session_id"], lifecycle_started["sub_session_id"]);
    assert_eq!(lifecycle_done["sub_session_id"], lifecycle_started["sub_session_id"]);
    assert_eq!(lifecycle_started["label"], "shell job");
    assert_eq!(lifecycle_started["task"], "do the thing");
    assert!(lifecycle_started.get("duration_ms").is_none());
    assert_eq!(progress["detail"], "half way");
    assert!(progress.get("task").is_none(), "empty fields are omitted");
    assert_eq!(lifecycle_done["duration_ms"], 42);
    assert_eq!(lifecycle_done["result_text"], "the result");
}

/// A2: the queue is flushed before `ended`, so even a long run of events stays inside the
/// turn's activity instead of trailing after it.
#[tokio::test]
async fn many_activity_frames_are_all_written_before_ended() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.activity_script.set(
        std::iter::repeat_n(
            ActivityStep::Tool {
                tool: "execute_shell",
                call_id: "call-many",
                terminal: opencrab_actions::ToolEventStatus::Completed,
            },
            120,
        )
        .collect(),
    );

    write_frame(&mut s, &said_frame_with_profile("a2-many", &binding_id, "o-a2m", None)).await;
    assert_eq!(read_said_response(&mut s, "a2-many").await["m"], "ok");
    let frames = long_turn_frames(&mut s).await;

    let activity = activity_frames(&frames);
    assert_eq!(activity.len(), 240, "every started/terminal pair is written");
    assert_eq!(frames.last().unwrap()["m"], "activity");
    assert_eq!(frames.last().unwrap()["state"], "ended");
}

/// A3: a failed tool execution carries its error; the shape stays the same otherwise.
#[tokio::test]
async fn failed_tool_activity_carries_the_error() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.runtime.activity_script.set(vec![ActivityStep::Tool {
        tool: "execute_shell",
        call_id: "call-2",
        terminal: opencrab_actions::ToolEventStatus::Failed,
    }]);

    write_frame(&mut s, &said_frame_with_profile("a2", &binding_id, "o-a2", None)).await;
    assert_eq!(read_said_response(&mut s, "a2").await["m"], "ok");
    let activity = activity_frames(&turn_frames(&mut s).await);

    assert_eq!(activity.len(), 2, "{activity:?}");
    assert_eq!(activity[1]["status"], "failed");
    assert_eq!(activity[1]["error"], "boom");
}

/// A4: with nothing to report, the turn's stream is exactly what it was before this change —
/// installing the observer does not alter the existing frames.
#[tokio::test]
async fn an_unobserved_activity_stream_does_not_change_the_turn_frames() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;

    write_frame(&mut s, &said_frame_with_profile("a3", &binding_id, "o-a3", None)).await;
    assert_eq!(read_said_response(&mut s, "a3").await["m"], "ok");
    let frames = turn_frames(&mut s).await;

    // The turn is observed (the sink is installed) …
    assert_eq!(*h.runtime.activity_script.sink_seen.lock().unwrap(), vec![true]);
    // … but with no events, no activity frame is written and the turn is unchanged.
    assert!(activity_frames(&frames).is_empty(), "{frames:?}");
    let kinds: Vec<&str> = frames
        .iter()
        .filter(|f| !(f["m"] == "activity" && f["state"] == "read"))
        .map(|f| f["m"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["activity", "say", "activity"], "{frames:?}");
}
