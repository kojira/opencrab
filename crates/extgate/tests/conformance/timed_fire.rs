use opencrab_actions::{TimedFireRequest, TimedFireSink, TransportFire};
use opencrab_extgate::{ExtgateFire, ExtgateTimedFireSink};

fn timed_fire_request(session_id: &str, agent_id: &str) -> TimedFireRequest {
    TimedFireRequest {
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        channel_id: String::new(),
        guild_id: String::new(),
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
async fn timed_fire_sink_revalidates_lifecycle_and_delivers_once_after_reconnect_ack() {
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

    sink.fire_timed_turn(timed_fire_request(alias, "agent-1"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
    assert!(read_frame_opt(&mut first).await.is_none());

    drop(first);
    wait_registry_absent(&h, &instance_id).await;
    sink.fire_timed_turn(timed_fire_request(alias, "agent-1"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);

    let mut reconnected = h.connect().await;
    hello_ok(&mut reconnected, &instance_id, 1).await;
    assert_eq!(ack_bind(&mut reconnected).await, binding_id);
    wait_binding_acknowledged(&h, &instance_id, &binding_id).await;

    sink.fire_timed_turn(timed_fire_request(alias, "other-agent"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 0);
    assert!(read_frame_opt(&mut reconnected).await.is_none());

    let resolved = {
        let conn = h.state.db.lock().unwrap();
        ExtgateFire
            .resolve_persisted(&conn, alias, "agent-1")
            .expect("alias resolves before close")
    };
    h.state
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE gate_bindings SET closed_at = ?1 WHERE binding_id = ?2",
            rusqlite::params![now_nanos(), binding_id],
        )
        .unwrap();
    sink.fire_timed_turn(timed_fire_request(&resolved.route, "agent-1"));
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
    sink.fire_timed_turn(timed_fire_request(alias, "agent-1"));
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
