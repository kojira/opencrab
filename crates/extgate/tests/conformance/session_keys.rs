// D-1099: core keeps sessions; a gateway names them with opaque keys. No pre-registration, no
// bind, no list.

/// `hello` with an instance id core has never seen creates the instance for the agent.
#[tokio::test]
async fn d1099_hello_with_no_prior_instance_is_accepted() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id).await;
    assert!(h.state.lock_registry().unwrap().is_live(&instance_id));
    let subject: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT subject_id FROM gate_instances WHERE instance_id=?1",
            [&instance_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(subject, h.subject_id);
}

/// `hello` for an agent core does not know is refused; no instance is created.
#[tokio::test]
async fn d1099_hello_for_an_unknown_agent_is_subject_unknown() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1", "m": "hello", "protocol": 3, "operation_protocol": 1,
            "final_delivery": "automatic", "operations": [],
            "instance_id": instance_id, "agent_id": "nobody",
        }),
    )
    .await;
    let err = read_frame(&mut s).await;
    assert_eq!(err["code"], "subject_unknown");
    let rows: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM gate_instances", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

/// A said with a key core has never seen creates the session (named by the key) and the `ok`
/// carries that session id.
#[tokio::test]
async fn d1099_said_with_a_new_key_creates_its_session_and_returns_it() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1", "m": "said", "session": "room-new", "origin": "o1",
            "author_id": "u1", "text": "hello", "attachments": []
        }),
    )
    .await;
    let ok = read_said_response(&mut s, "s1").await;
    assert_eq!(ok["seq"], 1);
    assert_eq!(ok["session_id"], "room-new");
    let conn = h.state.db.lock().unwrap();
    let recorded: String = conn
        .query_row(
            "SELECT session_id FROM memory_sessions WHERE content='hello'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recorded, "room-new");
    let (instance, key): (String, String) = conn
        .query_row(
            "SELECT instance_id, address FROM gate_bindings WHERE session_id='room-new'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((instance.as_str(), key.as_str()), (instance_id.as_str(), "room-new"));
}

/// A key that maps to a migrated record (v4 row id, session = the key, or an `extgate-` session)
/// continues that session; no second record is created.
#[tokio::test]
async fn d1099_said_on_a_migrated_record_continues_its_session() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let migrated_row = uuid();
    legacy_record(
        &h,
        &migrated_row,
        &instance_id,
        "discord-agent-1-g-c",
        "discord-agent-1-g-c",
        "#general",
    );
    let older_row = uuid();
    let physical = format!("extgate-{older_row}");
    legacy_record(&h, &older_row, &instance_id, "web-key", &physical, "web chat");
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id).await;
    for (id, key, expected) in [
        ("s1", "discord-agent-1-g-c", "discord-agent-1-g-c".to_string()),
        ("s2", "web-key", physical.clone()),
    ] {
        write_frame(
            &mut s,
            &json!({
                "id": id, "m": "said", "session": key, "origin": format!("o-{id}"),
                "author_id": "u1", "text": format!("text-{id}"), "attachments": []
            }),
        )
        .await;
        let ok = read_said_response(&mut s, id).await;
        assert_eq!(ok["seq"], 1);
        assert_eq!(ok["session_id"], expected.as_str());
    }
    let rows: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2, "migrated records are reused, not duplicated");
}

/// A reply comes back addressed by the key the said used, never by an internal id.
#[tokio::test]
async fn d1099_say_is_addressed_by_the_session_key() {
    let h = Harness::start().await;
    let (mut s, _, _) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1", "m": "said", "session": "room-say", "origin": "o1",
            "author_id": "u1", "text": "hi", "attachments": []
        }),
    )
    .await;
    read_said_response(&mut s, "s1").await;
    let say = read_until(&mut s, |v| v["m"] == "say").await;
    assert_eq!(say["session"], "room-say");
    assert!(say.get("binding_id").is_none());
    write_frame(&mut s, &json!({"id": say["id"], "m": "ok"})).await;
}

/// A timed fire of a session reaches the gateway under the session's key (the instance only has
/// to be connected).
#[tokio::test]
async fn d1099_timed_fire_reaches_the_gateway_by_key() {
    use opencrab_actions::{TimedFireRequest, TimedFireRouter, TimedFireSink};
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    let row = session_row(&h, &instance_id, "room-fire");
    let router = TimedFireRouter::new();
    let target = {
        let conn = h.state.db.lock().unwrap();
        router
            .resolve_persisted_target(&conn, "room-fire", "agent-1")
            .expect("fire target")
    };
    assert_eq!(target.binding_id, row.row_id);
    opencrab_extgate::ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone())
        .fire_timed_turn(TimedFireRequest {
            binding_id: target.binding_id,
            session_id: target.session_id,
            agent_id: "agent-1".into(),
            prompt: "timed".into(),
            caller: CallerIdentity::Owner,
        });
    let say = read_until(&mut s, |v| v["m"] == "say").await;
    assert_eq!(say["session"], "room-fire");
    write_frame(&mut s, &json!({"id": say["id"], "m": "ok"})).await;
}

/// `open` finds or creates a key's session, and is idempotent.
#[tokio::test]
async fn d1099_open_is_idempotent() {
    let h = Harness::start().await;
    let (mut s, _, _) = ready_pair(&h).await;
    for id in ["o1", "o2"] {
        write_frame(
            &mut s,
            &json!({"id": id, "m": "open", "session": "room-open", "title": "New chat"}),
        )
        .await;
        let ok = read_said_response(&mut s, id).await;
        assert_eq!(ok["m"], "ok");
        assert_eq!(ok["session_id"], "room-open");
    }
    let conn = h.state.db.lock().unwrap();
    let (rows, theme): (i64, String) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM gate_bindings WHERE address='room-open'),
                    (SELECT theme FROM sessions WHERE id='room-open')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((rows, theme.as_str()), (1, "New chat"));
}

/// The removed frames are unknown messages now: core never registers or binds anything.
#[tokio::test]
async fn d1099_provision_create_binding_and_bind_frames_do_not_exist() {
    let h = Harness::start().await;
    let (mut s, _, _) = ready_pair(&h).await;
    for (id, m) in [("p", "provision"), ("c", "create_binding"), ("b", "bind")] {
        write_frame(&mut s, &json!({"id": id, "m": m})).await;
        let err = read_said_response(&mut s, id).await;
        assert_eq!(err["code"], "unknown_message", "{m}");
    }
    // Nothing core sends after hello is a `bind`.
    assert!(read_frame_opt(&mut s).await.is_none());
}
