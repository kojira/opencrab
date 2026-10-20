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
            "revision": 1,
            "config_digest": config_digest(),
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
    hello_ok(&mut s, &instance_id, 1).await;
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
                "revision": 1,
                "config_digest": config_digest(),
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
        revision: 1,
        config_digest: config_digest(),
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
