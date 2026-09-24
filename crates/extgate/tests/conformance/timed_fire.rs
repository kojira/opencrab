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

async fn wait_registry_absent(h: &Harness, instance_id: &str) {
    for _ in 0..80 {
        if h
            .state
            .lock_registry()
            .unwrap()
            .get(instance_id)
            .is_none()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("live instance remained registered");
}

async fn wait_binding_acknowledged(h: &Harness, instance_id: &str, binding_id: &str) {
    for _ in 0..80 {
        if h
            .state
            .lock_registry()
            .unwrap()
            .get(instance_id)
            .is_some_and(|entry| entry.acknowledged.contains(binding_id))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("binding acknowledgement was not registered");
}

#[tokio::test]
async fn s3_automatic_hello_snapshot_survives_legacy_config_mutation_for_real_continuation() {
    let h = Harness::start().await;
    let session_id = "opaque-automatic-continuation";
    let instance_id = uuid();
    let binding_id = uuid();
    insert_named_session(&h, session_id);
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, session_id).await;

    let mut stream = h.connect().await;
    hello_ok(&mut stream, &instance_id, 1).await;
    assert_eq!(ack_bind(&mut stream).await, binding_id);
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
    assert_eq!(say["binding_id"], binding_id);
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

#[tokio::test]
async fn s3_timed_continuation_routes_generic_binding_session_once_after_reconnect_ack() {
    let h = Harness::start().await;
    let alias = "opaque-timed-fire-session";
    let instance_id = uuid();
    let binding_id = uuid();
    insert_named_session(&h, alias);
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, alias).await;
    let sink = ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone());

    let mut first = h.connect().await;
    hello_ok(&mut first, &instance_id, 1).await;
    let bind = read_frame(&mut first).await;
    assert_eq!(bind["m"], "bind");
    assert_eq!(bind["binding_id"], binding_id);

    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "agent-1"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
    assert!(read_frame_opt(&mut first).await.is_none());

    drop(first);
    wait_registry_absent(&h, &instance_id).await;
    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "agent-1"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);

    let mut reconnected = h.connect().await;
    hello_ok(&mut reconnected, &instance_id, 1).await;
    assert_eq!(ack_bind(&mut reconnected).await, binding_id);
    wait_binding_acknowledged(&h, &instance_id, &binding_id).await;

    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "other-agent"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
    assert!(read_frame_opt(&mut reconnected).await.is_none());

    h.state
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE gate_bindings SET closed_at = ?1 WHERE binding_id = ?2",
            rusqlite::params![now_nanos(), binding_id],
        )
        .unwrap();
    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "agent-1"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
    assert!(read_frame_opt(&mut reconnected).await.is_none());

    h.state
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE gate_bindings SET closed_at = NULL WHERE binding_id = ?1",
            [&binding_id],
        )
        .unwrap();
    sink.fire_timed_turn(timed_fire_request(&binding_id, alias, "agent-1"));
    let say = read_until(&mut reconnected, |frame| frame["m"] == "say").await;
    assert_eq!(say["binding_id"], binding_id);
    assert_eq!(say["payload"]["text"], "hello from agent");
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
    write_frame(
        &mut reconnected,
        &json!({"id": say["id"], "m": "ok"}),
    )
    .await;

    let mut say_count = 1;
    for _ in 0..4 {
        let Some(frame) = read_frame_opt(&mut reconnected).await else {
            break;
        };
        if frame["m"] == "say" {
            say_count += 1;
        }
    }
    assert_eq!(say_count, 1, "one accepted fire must deliver exactly once");
}
