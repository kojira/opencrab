// D-1081: the said that starts a turn decides its model profile; said-less resumes inherit it.

fn said_frame_with_profile(id: &str, binding_id: &str, origin: &str, profile: Option<&str>) -> Value {
    let mut frame = json!({
        "id": id,
        "m": "said",
        "binding_id": binding_id,
        "origin": origin,
        "author_id": "u1",
        "text": id,
        "attachments": []
    });
    if let Some(profile) = profile {
        frame["model_profile"] = json!(profile);
    }
    frame
}

fn turn_profiles(h: &Harness) -> Vec<TurnModelProfile> {
    h.runtime.turn_profiles.lock().unwrap().clone()
}

#[tokio::test]
async fn realtime_said_starts_a_realtime_turn_and_typed_said_a_default_turn() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;

    write_frame(&mut s, &said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime"))).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    wait_turns(&h, 1).await;
    write_frame(&mut s, &said_frame_with_profile("t1", &binding_id, "o-t1", None)).await;
    assert_eq!(read_said_response(&mut s, "t1").await["m"], "ok");
    wait_turns(&h, 2).await;

    assert_eq!(
        turn_profiles(&h),
        [TurnModelProfile::Realtime, TurnModelProfile::Default]
    );
}

/// D-1085: a realtime said runs a lightweight turn; §13: on the same agent context as a typed
/// said (plus the gateway context).
#[tokio::test]
async fn realtime_said_runs_a_lightweight_turn_on_the_agent_context() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;

    let mut voice = said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime"));
    voice["system_context"] = json!("GATEWAY-CONTEXT");
    write_frame(&mut s, &voice).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    wait_turns(&h, 1).await;
    write_frame(&mut s, &said_frame_with_profile("t1", &binding_id, "o-t1", None)).await;
    assert_eq!(read_said_response(&mut s, "t1").await["m"], "ok");
    wait_turns(&h, 2).await;

    let systems = h.runtime.system_prompts.lock().unwrap().clone();
    assert_eq!(
        systems,
        [
            format!("sys{}GATEWAY-CONTEXT", opencrab_core::SYSTEM_SEGMENT_BREAK),
            "sys".to_string(),
        ]
    );
    assert_eq!(*h.runtime.lightweight_runs.lock().unwrap(), [true, false]);
}

/// A realtime said queued behind a running typed turn starts its own realtime turn; one folded
/// into the running turn starts none, so the running turn's model is unchanged.
#[tokio::test]
async fn queued_realtime_said_starts_its_own_turn_and_a_folded_one_does_not() {
    for folded in [false, true] {
        let h = Harness::start().await;
        let (mut s, _, binding_id) = ready_pair(&h).await;
        let session_id = session_id_for_binding(&binding_id);
        let (release_tx, release_rx) = oneshot::channel();
        *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
        let entered = h.runtime.turn_entered.notified();

        write_frame(&mut s, &said_frame_with_profile("t1", &binding_id, "o-t1", None)).await;
        assert_eq!(read_said_response(&mut s, "t1").await["m"], "ok");
        entered.await;
        write_frame(&mut s, &said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime"))).await;
        let ok = read_said_response(&mut s, "v1").await;
        assert_eq!(ok["m"], "ok");
        if folded {
            // What the engine's read hook records when it folds this said into the running turn.
            h.state.mark_folded_seq(&session_id, ok["seq"].as_i64().unwrap());
        }
        release_tx.send(()).unwrap();
        wait_turns(&h, 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let expected: &[TurnModelProfile] = if folded {
            &[TurnModelProfile::Default]
        } else {
            &[TurnModelProfile::Default, TurnModelProfile::Realtime]
        };
        assert_eq!(turn_profiles(&h), expected, "folded={folded}");
    }
}

/// The completion sink of a realtime turn resumes on the realtime profile; a timed fire is a
/// default-profile turn.
#[tokio::test]
async fn subtask_resume_inherits_the_realtime_profile_and_timed_fire_uses_default() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let session_id = session_id_for_binding(&binding_id);

    write_frame(&mut s, &said_frame_with_profile("v1", &binding_id, "o-v1", Some("realtime"))).await;
    assert_eq!(read_said_response(&mut s, "v1").await["m"], "ok");
    wait_turns(&h, 1).await;
    let sink = h.runtime.last_sink.lock().unwrap().clone().expect("turn sink");
    settle_completed(
        &h.runtime.subtask_registry_for(&session_id),
        &h.state.db,
        sink.as_ref(),
        SettleContext {
            parent_session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            subtask_id: "st-voice".into(),
            sub_session_id: String::new(),
            exit_reason: "completed".into(),
            lifecycle: SubtaskLifecycle::new(),
        },
        r#"{"ok":true}"#,
    );
    wait_turns(&h, 2).await;
    assert_eq!(
        turn_profiles(&h),
        [TurnModelProfile::Realtime, TurnModelProfile::Realtime]
    );

    opencrab_extgate::ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone())
        .fire_timed_turn(opencrab_actions::TimedFireRequest {
            binding_id: binding_id.clone(),
            session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            prompt: "timed fire".into(),
            caller: CallerIdentity::Owner,
        });
    wait_turns(&h, 3).await;
    assert_eq!(turn_profiles(&h).last(), Some(&TurnModelProfile::Default));
}
