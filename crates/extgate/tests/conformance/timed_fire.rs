use opencrab_actions::{TimedFireRequest, TimedFireRouter, TimedFireSink};
use opencrab_extgate::ExtgateTimedFireSink;

fn timed_fire_request(binding_id: &str, session_id: &str, agent_id: &str) -> TimedFireRequest {
    TimedFireRequest {
        binding_id: binding_id.to_string(),
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        prompt: "timed fire".into(),
        caller: CallerIdentity::Owner,
    }
}


/// D-1099: nothing to acknowledge; a live instance is enough.
async fn wait_binding_acknowledged(_h: &Harness, _instance_id: &str, _binding_id: &str) {}

#[tokio::test]
async fn s3_automatic_hello_snapshot_survives_legacy_config_mutation_for_real_continuation() {
    let h = Harness::start().await;
    let session_id = "opaque-automatic-continuation";
    let instance_id = uuid();
    insert_named_session(&h, session_id);
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = (session_id).to_string();

    let mut stream = h.connect().await;
    hello_ok(&mut stream, &instance_id).await;
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_binding_acknowledged(&h, &instance_id, &binding_id).await;

    // The accepted hello snapshot is automatic. Mutating the legacy config after acceptance must
    // not change completion behavior for this live connection.
    let legacy_tool_driven = "eyJkZWxpdmVyeV9tb2RlIjoidG9vbF9kcml2ZW4ifQ==";
    let legacy_digest =
        opencrab_extgate::ids::config_digest_from_b64(legacy_tool_driven).unwrap();
    h.state
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE gate_instances SET config_b64 = ?1, config_digest = ?2 WHERE instance_id = ?3",
            rusqlite::params![legacy_tool_driven, legacy_digest, instance_id],
        )
        .unwrap();

    let canonical = {
        let conn = h.state.db.lock().unwrap();
        TimedFireRouter::new()
            .resolve_persisted_target(&conn, session_id, "agent-1")
            .expect("canonical generic route")
    };
    assert_eq!(canonical.binding_id, binding_id);
    assert_eq!(canonical.session_id, session_id);

    let sink = ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone());
    sink.fire_timed_turn(timed_fire_request(
        &canonical.binding_id,
        &canonical.session_id,
        "agent-1",
    ));
    let say = read_until(&mut stream, |frame| frame["m"] == "say").await;
    assert_eq!(say["session"], session_id);
    assert_eq!(say["payload"]["text"], "hello from agent");
    write_frame(&mut stream, &json!({"id": say["id"], "m": "ok"})).await;

    let mut say_count = 1;
    for _ in 0..4 {
        let Some(frame) = read_frame_opt(&mut stream).await else {
            break;
        };
        if frame["m"] == "say" {
            say_count += 1;
        }
    }
    assert_eq!(say_count, 1, "automatic continuation emitted duplicate say frames");
}

/// D-1099: a timed fire reaches the session's gateway once its instance is live, under the key;
/// nothing is sent while it is disconnected, and a fire for another agent is refused.
#[tokio::test]
async fn s3_timed_continuation_routes_the_session_once_when_its_instance_is_live() {
    let h = Harness::start().await;
    let alias = "opaque-timed-fire-session";
    let instance_id = uuid();
    insert_named_session(&h, alias);
    put_instance(&h, &instance_id, true).await;
    let binding_id = session_row(&h, &instance_id, alias).row_id;
    let sink = ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone());

    // Not connected: nothing runs.
    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "agent-1"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);

    let mut connected = h.connect().await;
    hello_ok(&mut connected, &instance_id).await;

    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "other-agent"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
    assert!(read_frame_opt(&mut connected).await.is_none());

    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "agent-1"));
    let say = read_until(&mut connected, |frame| frame["m"] == "say").await;
    assert_eq!(say["session"], alias);
    assert_eq!(say["payload"]["text"], "hello from agent");
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
    write_frame(&mut connected, &json!({"id": say["id"], "m": "ok"})).await;

    let mut say_count = 1;
    for _ in 0..4 {
        let Some(frame) = read_frame_opt(&mut connected).await else {
            break;
        };
        if frame["m"] == "say" {
            say_count += 1;
        }
    }
    assert_eq!(say_count, 1, "one accepted fire must deliver exactly once");
}

/// #612 §8.2 / RED-4: 同じセッションへ同時に 2 本の時刻トリガーが届いたら、捨てず・束ねず、
/// 共有 `SessionLocks` の下で別々のターンとして直列に走る。各ターンは自分の message を持つ。
#[tokio::test]
async fn same_session_timed_fires_run_as_separate_serialized_turns() {
    let h = Harness::start().await;
    *h.runtime.reply.lock().unwrap() = "NO_REPLY".into();
    let session_id = "opaque-concurrent-triggers";
    let instance_id = uuid();
    insert_named_session(&h, session_id);
    put_instance(&h, &instance_id, true).await;
    let binding_id_key: String = (session_id).to_string();
    let mut stream = h.connect().await;
    hello_ok(&mut stream, &instance_id).await;
    let binding_id = session_row(&h, &instance_id, &binding_id_key).row_id;
    wait_binding_acknowledged(&h, &instance_id, &binding_id).await;

    let (release_tx, release_rx) = oneshot::channel();
    *h.runtime.hold_rx.lock().unwrap() = Some(release_rx);
    let entered = h.runtime.turn_entered.notified();
    tokio::pin!(entered);

    let sink = ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone());
    let mut interval = timed_fire_request(&binding_id, session_id, "agent-1");
    interval.prompt = "interval trigger message".into();
    let mut daily = timed_fire_request(&binding_id, session_id, "agent-1");
    daily.prompt = "daily trigger message".into();
    sink.fire_timed_turn(interval);
    sink.fire_timed_turn(daily);

    tokio::time::timeout(Duration::from_secs(3), &mut entered)
        .await
        .expect("first timed turn did not start");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        h.runtime.system_prompts.lock().unwrap().len(),
        1,
        "second trigger waits for the session lock (turns must not overlap)"
    );
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);

    release_tx.send(()).unwrap();
    for _ in 0..200 {
        if h.runtime.turns.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 2, "both triggers ran");
    let prompts = h.runtime.system_prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 2);
    let interval_turns = prompts
        .iter()
        .filter(|p| p.contains("interval trigger message") && !p.contains("daily trigger message"))
        .count();
    let daily_turns = prompts
        .iter()
        .filter(|p| p.contains("daily trigger message") && !p.contains("interval trigger message"))
        .count();
    assert_eq!(interval_turns, 1, "interval turn carries only its own message");
    assert_eq!(daily_turns, 1, "daily turn carries only its own message");
}
