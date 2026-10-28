// D-1096: `interrupt` stops the binding session's running turn and records how much of the
// agent's last utterance reached the other side (`delivered`). Core keeps no notion of how the
// utterance was carried; it only records the text the gateway reports.

fn interrupt_frame(id: &str, binding_id: &str, delivered: Option<&str>) -> Value {
    let mut frame = json!({"id": id, "m": "interrupt", "binding_id": binding_id});
    if let Some(delivered) = delivered {
        frame["delivered"] = json!(delivered);
    }
    frame
}

/// `(log_type, speaker_id, content)` of every row of the session, in order.
fn session_log_rows(h: &Harness, session_id: &str) -> Vec<(String, Option<String>, String)> {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
        .unwrap()
        .into_iter()
        .map(|row| (row.log_type, row.speaker_id, row.content))
        .collect()
}

fn delivery_cut_rows(h: &Harness, session_id: &str) -> Vec<Value> {
    session_log_rows(h, session_id)
        .into_iter()
        .filter(|(log_type, speaker, _)| log_type == "system" && speaker.is_none())
        .filter_map(|(_, _, content)| serde_json::from_str::<Value>(&content).ok())
        .filter(|value| value["type"] == "delivery_cut")
        .collect()
}

/// Sends a said whose turn holds (the runtime waits on `hold_rx`) and returns once the turn is
/// inside the engine.
async fn start_held_turn(h: &Harness, s: &mut UnixStream, binding_id: &str, id: &str) {
    let (_release_tx, release_rx) = oneshot::channel::<()>();
    // The sender is leaked on purpose: the held turn only ends by being interrupted.
    std::mem::forget(_release_tx);
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    let entered = h.runtime.turn_entered.notified();
    tokio::pin!(entered);
    entered.as_mut().enable();
    write_frame(s, &said_frame_with_profile(id, binding_id, &format!("o-{id}"), Some("realtime")))
        .await;
    assert_eq!(read_said_response(s, id).await["m"], "ok");
    tokio::time::timeout(Duration::from_secs(2), entered)
        .await
        .expect("turn entered the engine");
}

/// I1: interrupting the running turn answers `ok`, the turn emits no say or partial after it,
/// its activity ends, and the next queued said's turn starts at once.
#[tokio::test]
async fn interrupt_stops_the_running_turn_and_the_next_turn_starts() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    start_held_turn(&h, &mut s, &binding_id, "v1").await;
    // A said queued behind the held turn.
    write_frame(&mut s, &said_frame_with_profile("v2", &binding_id, "o-v2", Some("realtime")))
        .await;
    assert_eq!(read_said_response(&mut s, "v2").await["m"], "ok");
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0, "v2 waits behind v1");

    write_frame(&mut s, &interrupt_frame("i1", &binding_id, None)).await;
    let mut after_ok = Vec::new();
    let mut ok = false;
    for _ in 0..40 {
        let Some(frame) = read_frame_opt(&mut s).await else {
            if ok && h.runtime.turns.load(Ordering::SeqCst) >= 1 {
                break;
            }
            continue;
        };
        if frame["id"] == "i1" {
            assert_eq!(frame["m"], "ok", "{frame}");
            ok = true;
            continue;
        }
        if frame["m"] == "say" {
            write_frame(&mut s, &json!({"id": frame["id"], "m": "ok"})).await;
        }
        if ok {
            after_ok.push(frame);
        }
    }
    assert!(ok, "interrupt was not acknowledged");
    // The held v1 run never returned; v2's turn ran instead.
    wait_turns(&h, 1).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
    let conversation = h.runtime.conversations.lock().unwrap().last().cloned().unwrap();
    assert!(conversation.ends_with("speech:v2"), "{conversation}");
    // The only say after the interrupt is v2's.
    let says: Vec<&Value> = after_ok.iter().filter(|f| f["m"] == "say").collect();
    assert_eq!(says.len(), 1, "{after_ok:?}");
    assert_eq!(says[0]["payload"]["text"], "hello from agent");
}

/// I2: a tool still running when the turn is interrupted is logged as `stopped`. A subtask the
/// turn had started keeps running.
#[tokio::test]
async fn interrupt_logs_a_running_tool_as_stopped_and_keeps_subtasks() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    let subtask = tokio::spawn(std::future::pending::<()>());
    h.runtime
        .registries
        .registry_for(&session_id)
        .insert("sub-1".into(), test_spawned_subtask(subtask.abort_handle(), &session_id));
    h.runtime.activity_script.set(vec![ActivityStep::RunningTool {
        tool: "execute_shell",
        call_id: "call-1",
    }]);
    start_held_turn(&h, &mut s, &binding_id, "v1").await;

    write_frame(&mut s, &interrupt_frame("i1", &binding_id, None)).await;
    assert_eq!(read_said_response(&mut s, "i1").await["m"], "ok");

    let logs = {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::list_tool_logs(&conn, "agent-1", 10).unwrap()
    };
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert_eq!(logs[0].tool_name, "execute_shell");
    assert_eq!(logs[0].outcome, "stopped");
    assert_eq!(logs[0].session_id.as_deref(), Some(session_id.as_str()));
    assert!(!subtask.is_finished(), "the subtask is not stopped");
    assert!(!h.runtime.registries.registry_for(&session_id).is_empty());
    subtask.abort();
}

/// I3 (C3, nothing recorded yet): the turn is stopped before its say is recorded, so the
/// delivered part is recorded as the agent's speech, followed by the delivery_cut note.
#[tokio::test]
async fn interrupt_before_the_say_records_the_delivered_part_and_the_note() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    start_held_turn(&h, &mut s, &binding_id, "v1").await;

    write_frame(&mut s, &interrupt_frame("i1", &binding_id, Some("一文目。"))).await;
    assert_eq!(read_said_response(&mut s, "i1").await["m"], "ok");

    let rows = session_log_rows(&h, &session_id);
    assert_eq!(
        rows[rows.len() - 2..],
        [
            ("speech".to_string(), Some("agent-1".to_string()), "一文目。".to_string()),
            (
                "system".to_string(),
                None,
                json!({"type": "delivery_cut", "delivered": "一文目。"}).to_string()
            ),
        ],
        "{rows:?}"
    );
}

/// I4 (C3, say already recorded): an intermediate say of the running turn was recorded, so the
/// row is left as is and only the delivery_cut note is added.
#[tokio::test]
async fn interrupt_after_a_recorded_say_keeps_it_and_adds_the_note() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    h.runtime
        .partial_script
        .set(vec![PartialStep::Say("一文目。二文目。")]);
    let (_release_tx, release_rx) = oneshot::channel::<()>();
    std::mem::forget(_release_tx);
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    write_frame(&mut s, &said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime")))
        .await;
    let say = read_until(&mut s, |f| f["m"] == "say").await;
    write_frame(&mut s, &json!({"id": say["id"], "m": "ok"})).await;

    write_frame(&mut s, &interrupt_frame("i1", &binding_id, Some("一文目。"))).await;
    assert_eq!(read_said_response(&mut s, "i1").await["m"], "ok");

    let rows = session_log_rows(&h, &session_id);
    let agent_speech: Vec<_> = rows
        .iter()
        .filter(|(t, speaker, _)| t == "speech" && speaker.as_deref() == Some("agent-1"))
        .collect();
    assert_eq!(agent_speech.len(), 1, "{rows:?}");
    assert_eq!(agent_speech[0].2, "一文目。二文目。", "the recorded say is unchanged");
    assert_eq!(
        delivery_cut_rows(&h, &session_id),
        [json!({"type": "delivery_cut", "delivered": "一文目。"})]
    );
    assert_eq!(rows.last().unwrap().0, "system", "the note follows the say");
}

/// I5 (C3, `delivered` empty): nothing reached the other side; the note says so.
#[tokio::test]
async fn interrupt_with_empty_delivered_records_that_nothing_was_delivered() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    start_held_turn(&h, &mut s, &binding_id, "v1").await;

    write_frame(&mut s, &interrupt_frame("i1", &binding_id, Some(""))).await;
    assert_eq!(read_said_response(&mut s, "i1").await["m"], "ok");

    let rows = session_log_rows(&h, &session_id);
    assert!(
        !rows
            .iter()
            .any(|(t, speaker, _)| t == "speech" && speaker.as_deref() == Some("agent-1")),
        "{rows:?}"
    );
    assert_eq!(
        delivery_cut_rows(&h, &session_id),
        [json!({"type": "delivery_cut", "delivered": ""})]
    );
}

/// I6: no turn is running and `delivered` is present (the reply was already recorded; the other
/// side was still receiving it). Nothing is aborted, the say row is unchanged, the note is added.
#[tokio::test]
async fn interrupt_without_a_running_turn_records_the_note_after_the_say() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    *h.runtime.reply.lock().unwrap() = "一文目。二文目。".into();
    write_frame(&mut s, &said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime")))
        .await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    let say = read_until(&mut s, |f| f["m"] == "say").await;
    write_frame(&mut s, &json!({"id": say["id"], "m": "ok"})).await;
    read_until(&mut s, |f| f["m"] == "activity" && f["state"] == "ended").await;
    let before = session_log_rows(&h, &session_id);

    write_frame(&mut s, &interrupt_frame("i1", &binding_id, Some("一文目。"))).await;
    assert_eq!(read_said_response(&mut s, "i1").await["m"], "ok");

    let after = session_log_rows(&h, &session_id);
    assert_eq!(after[..before.len()], before[..], "earlier rows are unchanged");
    assert_eq!(
        after[before.len()..],
        [(
            "system".to_string(),
            None,
            json!({"type": "delivery_cut", "delivered": "一文目。"}).to_string()
        )]
    );
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
}

/// I7: no turn is running and no `delivered`: `ok`, and nothing changes.
#[tokio::test]
async fn interrupt_without_a_running_turn_or_delivered_is_a_no_op() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);
    let before = session_log_rows(&h, &session_id);

    write_frame(&mut s, &interrupt_frame("i1", &binding_id, None)).await;
    assert_eq!(read_said_response(&mut s, "i1").await["m"], "ok");

    assert_eq!(session_log_rows(&h, &session_id), before);
    assert!(read_frame_opt(&mut s).await.is_none());
}

/// I8: the frame shape is checked like the other requests.
#[tokio::test]
async fn interrupt_with_a_malformed_field_is_bad_request() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let mut frame = interrupt_frame("i1", &binding_id, None);
    frame["delivered"] = json!(42);
    write_frame(&mut s, &frame).await;
    assert_eq!(read_said_response(&mut s, "i1").await["code"], "bad_request");
    write_frame(&mut s, &json!({"id": "i2", "m": "interrupt", "binding_id": "not-a-uuid"})).await;
    assert_eq!(read_said_response(&mut s, "i2").await["code"], "bad_request");
}
