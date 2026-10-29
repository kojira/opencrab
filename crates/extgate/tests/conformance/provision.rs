// D-1098: a gateway declares its own generic instance and the complete set of its bindings with
// one `provision` frame before `hello`. There is no credential: being able to open the socket is
// the only gate.

fn provision_frame(id: &str, instance_id: &str, enabled: bool, bindings: &[(&str, &str)]) -> Value {
    json!({
        "id": id,
        "m": "provision",
        "instance_id": instance_id,
        "kind_id": "opaque-kind",
        "agent_id": "agent-1",
        "enabled": enabled,
        "config_b64": config_b64(),
        "bindings": bindings.iter().map(|(binding_id, address)| json!({
            "binding_id": binding_id,
            "address": address,
        })).collect::<Vec<_>>(),
    })
}

async fn provision(h: &Harness, frame: &Value) -> Value {
    let mut s = h.connect().await;
    write_frame(&mut s, frame).await;
    let answer = read_frame(&mut s).await;
    assert!(
        read_frame_opt(&mut s).await.is_none(),
        "the connection closes after one provision answer"
    );
    answer
}

fn open_bindings_of(h: &Harness, instance_id: &str) -> Vec<String> {
    let conn = h.state.db.lock().unwrap();
    let mut statement = conn
        .prepare(
            "SELECT binding_id FROM gate_bindings
             WHERE instance_id=?1 AND closed_at IS NULL ORDER BY binding_id",
        )
        .unwrap();
    statement
        .query_map([instance_id], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[tokio::test]
async fn d1098_provision_creates_an_instance_and_binding_without_credentials_then_hello_binds() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();

    let answer = provision(
        &h,
        &provision_frame("p1", &instance_id, true, &[(&binding_id, "chan-1")]),
    )
    .await;
    assert_eq!(answer["m"], "provisioned", "{answer}");
    assert_eq!(answer["id"], "p1");
    assert_eq!(answer["instance_id"], instance_id);
    assert_eq!(answer["revision"], 1);
    assert_eq!(answer["config_digest"], config_digest());
    assert_eq!(answer["enabled"], true);
    assert_eq!(
        answer["bindings"],
        json!([{"binding_id": binding_id, "address": "chan-1"}])
    );

    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    assert_eq!(ack_bind(&mut s).await, binding_id);
    let conn = h.state.db.lock().unwrap();
    let session: String = conn
        .query_row(
            "SELECT session_id FROM gate_bindings WHERE binding_id=?1",
            [&binding_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session, session_id_for_binding(&binding_id));
}

#[tokio::test]
async fn d1098_provision_retry_is_a_no_op_and_changes_revise_and_close() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let first = uuid();
    let second = uuid();
    let frame = provision_frame("p1", &instance_id, true, &[(&first, "chan-1")]);
    assert_eq!(provision(&h, &frame).await["revision"], 1);
    let retry = provision(&h, &frame).await;
    assert_eq!(retry["revision"], 1, "identical retry changes nothing");
    assert_eq!(open_bindings_of(&h, &instance_id), vec![first.clone()]);

    let mut disabled = provision_frame("p2", &instance_id, false, &[(&second, "chan-2")]);
    let answer = provision(&h, &disabled).await;
    assert_eq!(answer["revision"], 2, "{answer}");
    assert_eq!(answer["enabled"], false);
    assert_eq!(
        open_bindings_of(&h, &instance_id),
        vec![second.clone()],
        "a binding missing from the declaration is closed"
    );

    disabled["bindings"] = json!([]);
    provision(&h, &disabled).await;
    assert!(open_bindings_of(&h, &instance_id).is_empty());
}

#[tokio::test]
async fn d1098_provision_refuses_a_live_instance_an_unknown_agent_and_a_taken_address() {
    let h = Harness::start().await;
    let (_live, live_instance, _) = ready_pair(&h).await;
    let changed = provision(
        &h,
        &provision_frame("p1", &live_instance, false, &[]),
    )
    .await;
    assert_eq!(changed["m"], "err");
    assert_eq!(changed["code"], "instance_active");

    let mut unknown = provision_frame("p2", &uuid(), true, &[]);
    unknown["agent_id"] = json!("no-such-agent");
    assert_eq!(provision(&h, &unknown).await["code"], "subject_unknown");

    let taken = provision(
        &h,
        &provision_frame("p3", &uuid(), true, &[(&uuid(), "chan-1")]),
    )
    .await;
    assert_eq!(taken["code"], "address_in_use", "{taken}");

    let mut extra = provision_frame("p4", &uuid(), true, &[]);
    extra["subject_grant"] = json!("anything");
    assert_eq!(provision(&h, &extra).await["code"], "bad_request");
}

#[tokio::test]
async fn d1098_provision_after_hello_is_protocol_order() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(&mut s, &provision_frame("p9", &instance_id, true, &[])).await;
    let answer = read_until(&mut s, |v| v["m"] == "err").await;
    assert_eq!(answer["code"], "protocol_order");
}

#[tokio::test]
async fn d1098_gate_client_provision_then_instance_client_hello_binds() {
    use opencrab_gate_client::provision::{binding_id_for, ProvisionClient, ProvisionDesired};
    let h = Harness::start().await;
    let instance_id = uuid();
    let addresses = vec!["chan-a".to_string(), "chan-b".to_string()];
    let provisioned = ProvisionClient::new(h.sock.clone())
        .provision(&ProvisionDesired {
            instance_id: &instance_id,
            kind_id: "opaque-kind",
            agent_id: "agent-1",
            enabled: true,
            config_b64: config_b64(),
            addresses: &addresses,
        })
        .await
        .unwrap();
    assert_eq!(provisioned.revision, 1);
    assert_eq!(provisioned.config_digest, config_digest());
    let expected: Vec<String> = addresses
        .iter()
        .map(|address| binding_id_for(&instance_id, address).unwrap())
        .collect();
    assert_eq!(
        provisioned
            .bindings
            .iter()
            .map(|b| b.binding_id.clone())
            .collect::<Vec<_>>(),
        expected
    );

    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, provisioned.revision).await;
    let mut bound = vec![ack_bind(&mut s).await, ack_bind(&mut s).await];
    bound.sort();
    let mut want = expected.clone();
    want.sort();
    assert_eq!(bound, want);
}
