// D-1098: `provision` and `create_binding` respect the bindings an instance already has for an
// address. An open binding is kept as it is (its id, session and title), even when its id is not
// the id the gateway would derive today (bindings migrated before provision existed). A closed
// binding for the same instance and address is reopened, so the conversation continues.

/// An open binding in the shape migration v54 left behind: an id the gateway does not derive,
/// session = address, a title that is not the provision title.
fn seed_migrated_binding(h: &Harness, instance_id: &str, binding_id: &str, address: &str) {
    let outcome =
        create_binding_row(h, binding_id, instance_id, address, address, "migrated title").unwrap();
    assert_eq!(outcome, opencrab_db::queries::CoreBindingOutcome::Created);
}

fn binding_row(h: &Harness, binding_id: &str) -> (String, String, Option<i64>) {
    let conn = h.state.db.lock().unwrap();
    conn.query_row(
        "SELECT address, session_id, closed_at FROM gate_bindings WHERE binding_id=?1",
        [binding_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

fn session_theme(h: &Harness, session_id: &str) -> String {
    let conn = h.state.db.lock().unwrap();
    opencrab_db::queries::get_session(&conn, session_id)
        .unwrap()
        .unwrap()
        .theme
}

fn binding_count(h: &Harness, instance_id: &str) -> i64 {
    let conn = h.state.db.lock().unwrap();
    conn.query_row(
        "SELECT count(*) FROM gate_bindings WHERE instance_id=?1",
        [instance_id],
        |row| row.get(0),
    )
    .unwrap()
}

#[tokio::test]
async fn d1098_provision_keeps_an_existing_migrated_open_binding_and_hello_binds_it() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance_kind(&h, &instance_id, true, "opaque-kind").await;
    let migrated = uuid();
    seed_migrated_binding(&h, &instance_id, &migrated, "chan-migrated");

    let requested = uuid();
    let answer = provision(
        &h,
        &provision_frame("p1", &instance_id, true, &[(&requested, "chan-migrated")]),
    )
    .await;
    assert_eq!(answer["m"], "provisioned", "{answer}");
    assert_eq!(
        answer["bindings"],
        json!([{"binding_id": migrated, "address": "chan-migrated"}]),
        "the existing binding is reported, not the requested id"
    );
    assert_eq!(open_bindings_of(&h, &instance_id), vec![migrated.clone()]);
    let (_, session, closed) = binding_row(&h, &migrated);
    assert_eq!(session, "chan-migrated", "session kept as it was");
    assert!(closed.is_none());
    assert_eq!(session_theme(&h, "chan-migrated"), "migrated title");
    assert_eq!(binding_count(&h, &instance_id), 1, "no second binding row");

    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    assert_eq!(ack_bind(&mut s).await, migrated, "bind carries the existing id");
}

#[tokio::test]
async fn d1098_provision_reopens_a_removed_address_with_the_same_binding_and_session() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let a = uuid();
    let b = uuid();
    provision(
        &h,
        &provision_frame("p1", &instance_id, true, &[(&a, "chan-a"), (&b, "chan-b")]),
    )
    .await;
    let (_, b_session, _) = binding_row(&h, &b);

    let removed = provision(&h, &provision_frame("p2", &instance_id, true, &[(&a, "chan-a")])).await;
    assert_eq!(removed["m"], "provisioned", "{removed}");
    assert!(binding_row(&h, &b).2.is_some(), "a missing address is closed");

    let readded = provision(
        &h,
        &provision_frame("p3", &instance_id, true, &[(&a, "chan-a"), (&b, "chan-b")]),
    )
    .await;
    assert_eq!(readded["m"], "provisioned", "{readded}");
    let (_, session_after, closed_after) = binding_row(&h, &b);
    assert!(closed_after.is_none(), "the same binding is reopened");
    assert_eq!(session_after, b_session, "the conversation continues");
    let mut open = open_bindings_of(&h, &instance_id);
    open.sort();
    let mut expected = vec![a.clone(), b.clone()];
    expected.sort();
    assert_eq!(open, expected);
    assert_eq!(binding_count(&h, &instance_id), 2, "no new binding row");
}

#[tokio::test]
async fn d1098_create_binding_reopens_a_closed_binding_for_the_same_address() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    provision(
        &h,
        &provision_frame("p1", &instance_id, true, &[(&binding_id, "chan-x")]),
    )
    .await;
    provision(&h, &provision_frame("p2", &instance_id, true, &[])).await;
    let (_, session_before, closed) = binding_row(&h, &binding_id);
    assert!(closed.is_some());

    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    write_frame(
        &mut s,
        &json!({
            "id": "c1",
            "m": "create_binding",
            "binding_id": binding_id,
            "address": "chan-x",
            "session_theme": "gateway session"
        }),
    )
    .await;
    let response = read_until(&mut s, |value| value["id"] == "c1").await;
    assert_eq!(response["m"], "ok", "{response}");
    let (_, session_after, closed_after) = binding_row(&h, &binding_id);
    assert!(closed_after.is_none(), "create_binding reopens the closed binding");
    assert_eq!(session_after, session_before);
    assert_eq!(binding_count(&h, &instance_id), 1);
}

#[tokio::test]
async fn d1098_provision_still_refuses_an_address_open_on_another_instance() {
    let h = Harness::start().await;
    let owner = uuid();
    provision(
        &h,
        &provision_frame("p1", &owner, true, &[(&uuid(), "chan-shared")]),
    )
    .await;
    let other = provision(
        &h,
        &provision_frame("p2", &uuid(), true, &[(&uuid(), "chan-shared")]),
    )
    .await;
    assert_eq!(other["code"], "address_in_use", "{other}");
}
