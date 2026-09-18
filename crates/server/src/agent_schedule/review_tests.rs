use super::*;
use serde_json::json;

fn context(session_id: &str) -> GatewayCallContext {
    GatewayCallContext::new(GatewayCaller::TrustedUser, "agent-x")
        .with_session_id(session_id.to_string())
}

fn state_with_alias(session_id: &str) -> AppState {
    let state = crate::test_app_state();
    let mut conn = state.db.lock().unwrap();
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: "agent-x".into(),
            name: "agent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "persona".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = 'agent-x'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES ('aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa', 'generic', ?1, 1, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject_id, "0".repeat(64)],
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    opencrab_db::queries::insert_session_in_tx(&tx, session_id, "alias", "2026-01-01T00:00:00Z")
        .unwrap();
    opencrab_db::queries::insert_agent_session_in_tx(&tx, "agent-x", session_id).unwrap();
    opencrab_db::queries::create_gate_binding_in_tx(
        &tx,
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        session_id,
        session_id,
        1,
    )
    .unwrap();
    tx.commit().unwrap();
    drop(conn);
    state
}

fn poison_db(db: &opencrab_db::Db) {
    let db = db.clone();
    assert!(std::thread::spawn(move || {
        let _guard = db.lock().unwrap();
        panic!("poison test DB");
    })
    .join()
    .is_err());
}

#[test]
fn set_and_update_accept_owned_alias() {
    let session_id = "opaque-schedule-session";
    let state = state_with_alias(session_id);
    let context = context(session_id);
    let created = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "first"}),
        &context,
    );
    assert!(created.success, "alias create: {:?}", created.error);
    let id = created.data.unwrap()["id"].as_i64().unwrap();
    let updated = update_my_schedule(&state, &json!({"id": id, "message": "updated"}), &context);
    assert!(updated.success, "alias update: {:?}", updated.error);
    assert_eq!(updated.data.unwrap()["message"], "updated");
}

#[test]
fn persisted_resolution_db_lock_failure_is_fail_closed() {
    let session_id = "opaque-schedule-session";
    let state = state_with_alias(session_id);
    poison_db(&state.db);
    let result = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "first"}),
        &context(session_id),
    );
    assert!(!result.success);
    assert!(result.error.unwrap().contains("再試行"));
}
