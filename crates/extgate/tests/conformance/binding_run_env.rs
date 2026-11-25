// D-1091: `hello` carries the environment the gateway declares for its own instance.
//
// Core keeps the declaration on the live entry so a turn can hand it to `execute_shell`
// unchanged (see `crates/server/src/process/tests/binding_run_env.rs` for the child-process
// proof). Here the wire is pinned: well-formed declarations land, malformed ones are refused
// instead of silently dropped, and an absent declaration is the previous behaviour.

/// A hello with `run_env` keeps the declaration on the live entry.
#[tokio::test]
async fn hello_keeps_the_declared_run_env_on_the_live_entry() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 3,
            "operation_protocol": 1,
            "final_delivery": "automatic",
            "operations": [],
            "instance_id": instance_id,
            "agent_id": "agent-1",
            "run_env": {"OPENCRAB_TEST_BINDING_ACCOUNT": "binding-account-1"},
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["m"], "ok");
    let run_env = h
        .state
        .lock_registry()
        .unwrap()
        .get(&instance_id)
        .map(|live| live.run_env.clone())
        .unwrap_or_default();
    assert_eq!(
        run_env.get("OPENCRAB_TEST_BINDING_ACCOUNT").map(String::as_str),
        Some("binding-account-1"),
        "the declaration must be kept for this instance's turns"
    );
}

/// A hello without `run_env` declares nothing (the previous frame is unchanged).
#[tokio::test]
async fn hello_without_run_env_declares_nothing() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id).await;
    let run_env = h
        .state
        .lock_registry()
        .unwrap()
        .get(&instance_id)
        .map(|live| live.run_env.clone())
        .unwrap_or_default();
    assert!(run_env.is_empty(), "no declaration means no environment");
}

/// A malformed declaration is refused, never silently dropped: the gateway must learn that its
/// declaration did not take effect.
#[tokio::test]
async fn a_malformed_run_env_is_refused() {
    for bad in [
        json!({"OPENCRAB_TEST_BINDING_ACCOUNT": 1}),
        json!({"": "value"}),
        json!(["NAME=value"]),
        json!({"OPENCRAB_TEST_BINDING_ACCOUNT": "x".repeat(9000)}),
        // A control character (NUL, newline, ...) makes the child spawn itself fail, so the
        // declaration must be refused here instead of erroring on every `execute_shell`.
        json!({"OPENCRAB_TEST_BINDING_ACCOUNT": "account\u{0}name"}),
        json!({"OPENCRAB_TEST_BINDING_ACCOUNT": "account\nname"}),
    ] {
        let h = Harness::start().await;
        let instance_id = uuid();
        put_instance(&h, &instance_id, true).await;
        let mut s = h.connect().await;
        write_frame(
            &mut s,
            &json!({
                "id": "h1",
                "m": "hello",
                "protocol": 3,
                "operation_protocol": 1,
                "final_delivery": "automatic",
                "operations": [],
                "instance_id": instance_id,
                "agent_id": "agent-1",
                "run_env": bad,
            }),
        )
        .await;
        let frame = read_frame(&mut s).await;
        assert_eq!(
            frame["m"], "err",
            "a malformed run_env must be refused, got {frame}"
        );
        assert_eq!(frame["code"], "bad_request", "got {frame}");
        assert!(
            h.state
                .lock_registry()
                .unwrap()
                .get(&instance_id)
                .is_none(),
            "a refused hello must not register the instance"
        );
    }
}

/// A declared environment is a secret (the account key). Formatting a `Hello` must never print
/// the values, however the type is logged later; the names stay visible because operators need to
/// know what was declared.
///
/// This assertion lives outside the `Debug` implementation on purpose: if someone replaces the
/// masking impl with `#[derive(Debug)]`, this test still compiles and goes red.
#[test]
fn formatting_a_hello_never_prints_a_declared_value() {
    let mut hello = opencrab_extgate::protocol::Hello {
        id: "h1".to_string(),
        protocol: 3,
        instance_id: "instance-1".to_string(),
        agent_id: "agent-1".to_string(),
        operation_protocol: 1,
        final_delivery: "automatic".to_string(),
        operations: json!([]),
        run_env: Default::default(),
    };
    hello.run_env.insert(
        "OPENCRAB_TEST_BINDING_ACCOUNT".to_string(),
        "binding-account-1".to_string(),
    );
    for rendered in [format!("{hello:?}"), format!("{hello:#?}")] {
        assert!(
            rendered.contains("OPENCRAB_TEST_BINDING_ACCOUNT"),
            "the declared name should stay visible: {rendered}"
        );
        assert!(
            !rendered.contains("binding-account-1"),
            "a declared value must never be formatted: {rendered}"
        );
    }
}

// D-1149: every turn this binding runs carries the declaration, not only the said turn. A timed
// fire, the resume after a dispatched `execute_shell` settles and a provisional run reach
// `execute_shell` too; without the declaration their shell ran on the host user's account.

const BINDING_ENV_NAME: &str = "OPENCRAB_TEST_BINDING_ACCOUNT";
const BINDING_ENV_VALUE: &str = "binding-account-1";

/// Connects `instance_id` with a hello that declares [`BINDING_ENV_NAME`].
async fn hello_declaring_binding_env(h: &Harness, instance_id: &str) -> UnixStream {
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 3,
            "operation_protocol": 1,
            "final_delivery": "automatic",
            "operations": [],
            "instance_id": instance_id,
            "agent_id": "agent-1",
            "run_env": {BINDING_ENV_NAME: BINDING_ENV_VALUE},
        }),
    )
    .await;
    assert_eq!(read_frame(&mut s).await["m"], "ok");
    s
}

/// The declared value seen by every run so far, in run order.
fn run_binding_envs(h: &Harness) -> Vec<Option<String>> {
    h.runtime
        .run_envs
        .lock()
        .unwrap()
        .iter()
        .map(|env| env.get(BINDING_ENV_NAME).cloned())
        .collect()
}

fn assert_every_run_declares_binding_env(h: &Harness, path: &str) {
    let seen = run_binding_envs(h);
    assert!(!seen.is_empty(), "{path}: no run happened");
    assert!(
        seen.iter().all(|v| v.as_deref() == Some(BINDING_ENV_VALUE)),
        "{path}: every run must carry the binding's declared env, got {seen:?}"
    );
}

#[tokio::test]
async fn said_turn_carries_the_binding_env() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = hello_declaring_binding_env(&h, &instance_id).await;
    write_frame(
        &mut s,
        &json!({
            "id": "c1", "m": "said", "session": "chan-1", "origin": "o-1",
            "author_id": "u1", "text": "hi", "attachments": []
        }),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "c1").await["m"], "ok");
    wait_turns(&h, 1).await;
    assert_every_run_declares_binding_env(&h, "said turn");
}

#[tokio::test]
async fn timed_fire_turn_carries_the_binding_env() {
    let h = Harness::start().await;
    let session_id = "opaque-timed-fire-env";
    let instance_id = uuid();
    insert_named_session(&h, session_id);
    put_instance(&h, &instance_id, true).await;
    let binding_id = session_row(&h, &instance_id, session_id).row_id;
    let _s = hello_declaring_binding_env(&h, &instance_id).await;
    ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone()).fire_timed_turn(
        timed_fire_request(&binding_id, session_id, "agent-1"),
    );
    wait_turns(&h, 1).await;
    assert_every_run_declares_binding_env(&h, "timed fire");
}

#[tokio::test]
async fn resume_after_a_dispatched_tool_settles_carries_the_binding_env() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let _s = hello_declaring_binding_env(&h, &instance_id).await;
    let key = "chan-1";
    let binding_id = session_row(&h, &instance_id, key).row_id;
    let session_id = key.to_string();
    let sink = ExtgateCompletionSink {
        state: Arc::clone(&h.state),
        runtime: h.runtime.clone(),
        instance_id,
        binding_id,
        agent_id: "agent-1".into(),
        session_id: session_id.clone(),
        only_speaker: false,
        speaker_id: "u1".into(),
        system_context: String::new(),
        model_profile: TurnModelProfile::Default,
        activity: None,
    };
    settle_completed(
        &h.runtime.subtask_registry_for(&session_id),
        &h.state.db,
        &sink,
        SettleContext {
            parent_session_id: session_id.clone(),
            agent_id: "agent-1".into(),
            subtask_id: "st-shell".into(),
            sub_session_id: String::new(),
            exit_reason: "completed".into(),
            lifecycle: SubtaskLifecycle::new(),
        },
        r#"{"exit_code":0}"#,
    );
    wait_turns(&h, 1).await;
    assert_every_run_declares_binding_env(&h, "resume");
}

#[tokio::test]
async fn provisional_run_carries_the_binding_env() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = hello_declaring_binding_env(&h, &instance_id).await;
    write_frame(
        &mut s,
        &provisional_frame("p1", "chan-1", PROVISIONAL_ID, "（音声）今日は"),
    )
    .await;
    assert_eq!(read_said_response(&mut s, "p1").await["m"], "ok");
    wait_turns(&h, 1).await;
    assert_every_run_declares_binding_env(&h, "provisional");
}
