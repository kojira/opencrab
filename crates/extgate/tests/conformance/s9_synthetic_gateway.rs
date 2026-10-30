/// A new kind of gateway just says hello (D-1099), declares an operation, and gets a timed route
/// and one-frame/one-handler delivery without a concrete branch in core/shared/server.
#[tokio::test]
async fn s9_synthetic_gateway_connects_invokes_and_delivers_a_timed_result() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let address = "synthetic-address";
    let session_id = address;

    let operations = json!([{
        "name": "synthetic.report",
        "description": "report a synthetic result",
        "input_schema": {"type": "object"},
        "output_schema": {"type": "object"},
        "callback_schema": null,
        "authorization": {"allowed_callers": ["owner"]},
        "dispatch": "background",
        "sub_engine": "not_exposed",
        "sharing": "agent_bound",
        "effect": "state_change"
    }]);
    let mut gateway = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut gateway, &instance_id, 1, &operations).await["m"],
        "ok"
    );
    let binding_id = session_row(&h, &instance_id, address).row_id;

    let projected = ExtgateOpsGatewayActions::for_binding(
        Arc::clone(&h.state), &instance_id, &binding_id, session_id, "agent-1",
    ).unwrap();
    let invoked = tokio::spawn(async move {
        projected.execute(
            "synthetic.report",
            &json!({"result": "ok"}),
            &GatewayCallContext::new(GatewayCaller::Owner, "agent-1"),
        ).await
    });
    let invoke = read_until(&mut gateway, |frame| frame["m"] == "invoke").await;
    assert_eq!(invoke["operation"], "synthetic.report");
    assert_eq!(invoke["payload"], json!({"result": "ok"}));
    write_frame(&mut gateway, &json!({
        "id": invoke["id"], "m": "ok", "result": {"accepted": true}
    })).await;
    assert_eq!(invoked.await.unwrap().data, Some(json!({"accepted": true})));

    let target = {
        let conn = h.state.db.lock().unwrap();
        TimedFireRouter::new().resolve_persisted_target(&conn, session_id, "agent-1")
            .expect("generic timed route")
    };
    assert_eq!(target.binding_id, binding_id);
    ExtgateTimedFireSink::new(Arc::clone(&h.state), h.runtime.clone()).fire_timed_turn(
        TimedFireRequest {
            binding_id: target.binding_id,
            session_id: target.session_id,
            agent_id: "agent-1".into(),
            prompt: "timed synthetic result".into(),
            caller: CallerIdentity::Owner,
        }
    );
    let say = read_until(&mut gateway, |frame| frame["m"] == "say").await;
    assert_eq!(say["session"], address);
    assert_eq!(say["payload"]["text"], "hello from agent");
    let delivery_id = say["id"].as_str().unwrap();
    write_frame(&mut gateway, &json!({"id": delivery_id, "m": "ok"})).await;
    for _ in 0..40 {
        let state: Option<String> = h.state.db.lock().unwrap().query_row(
            "SELECT state FROM deliveries WHERE delivery_id=?1",
            [delivery_id], |row| row.get(0),
        ).ok();
        if state.as_deref() == Some("delivered") { return; }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed synthetic delivery did not reach the existing terminal state");
}
