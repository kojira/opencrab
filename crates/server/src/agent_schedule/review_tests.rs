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
        "alias",
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

/// #612 I1〜I3: update_my_schedule の式変更・再有効化は last_fired_at を保持し、次回は now より後。
#[test]
fn update_my_schedule_keeps_last_fired_and_fires_after_now() {
    let session_id = "opaque-schedule-session";
    let state = state_with_alias(session_id);
    let context = context(session_id);
    let created = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "patrol"}),
        &context,
    );
    assert!(created.success, "create: {:?}", created.error);
    let id = created.data.unwrap()["id"].as_i64().unwrap();
    let last_fired = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::set_agent_schedule_last_fired(&conn, id, &last_fired).unwrap();
    }
    let after_now = |data: &serde_json::Value| {
        let next = data["next_fire_at"].as_str().expect("next_fire_at");
        chrono::DateTime::parse_from_rfc3339(next).unwrap() > chrono::Utc::now()
    };

    let changed = update_my_schedule(
        &state,
        &json!({"id": id, "cron_expr": "0 7 * * *"}),
        &context,
    );
    assert!(changed.success, "cron change: {:?}", changed.error);
    let data = changed.data.unwrap();
    assert_eq!(data["last_fired_at"], last_fired.as_str());
    assert!(after_now(&data), "cron 変更直後に即発火しない");

    let disabled = update_my_schedule(&state, &json!({"id": id, "enabled": false}), &context);
    assert!(disabled.success, "disable: {:?}", disabled.error);
    let enabled = update_my_schedule(&state, &json!({"id": id, "enabled": true}), &context);
    assert!(enabled.success, "enable: {:?}", enabled.error);
    let data = enabled.data.unwrap();
    assert_eq!(data["last_fired_at"], last_fired.as_str());
    assert!(after_now(&data), "再有効化直後に即発火しない");
}

struct CollectingTimedFireSink {
    requests: std::sync::Arc<std::sync::Mutex<Vec<opencrab_actions::TimedFireRequest>>>,
    delivered: std::sync::Arc<tokio::sync::Notify>,
}

impl opencrab_actions::TimedFireSink for CollectingTimedFireSink {
    fn fire_timed_turn(&self, request: opencrab_actions::TimedFireRequest) {
        self.requests.lock().unwrap().push(request);
        self.delivered.notify_one();
    }
}

/// #612 D2: `run_my_schedule` は所属チェックを通った行を TimedFire で発火し、プロンプトは行の
/// message を含む。`last_fired_at` は更新しない。
#[tokio::test]
async fn run_my_schedule_fires_owned_row_message_without_touching_last_fired() {
    let session_id = "opaque-schedule-session";
    let state = state_with_alias(session_id);
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let delivered = std::sync::Arc::new(tokio::sync::Notify::new());
    state
        .timed_fire_router
        .register_sink(std::sync::Arc::new(CollectingTimedFireSink {
            requests: requests.clone(),
            delivered: delivered.clone(),
        }));
    let created = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "manual patrol message"}),
        &context(session_id),
    );
    assert!(created.success, "create: {:?}", created.error);
    let id = created.data.unwrap()["id"].as_i64().unwrap();

    let owner = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
        .with_session_id(session_id.to_string());
    let run = run_my_schedule(&state, &json!({"id": id}), &owner);
    assert!(run.success, "run: {:?}", run.error);
    tokio::time::timeout(std::time::Duration::from_secs(3), delivered.notified())
        .await
        .expect("manual fire must reach the timed-fire sink");

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session_id, session_id);
    assert!(requests[0].prompt.contains("manual patrol message"));
    drop(requests);
    let row = opencrab_db::queries::get_agent_schedule(&state.db.lock().unwrap(), id)
        .unwrap()
        .unwrap();
    assert!(
        row.last_fired_at.is_none(),
        "手動発火は last_fired_at を刻まない"
    );
}

/// #612 D2: `run_my_schedule` は owner / co_agent だけ。他人・他セッションの id は存在を明かさず拒否。
#[test]
fn run_my_schedule_is_owner_only_and_rejects_foreign_id() {
    let session_id = "opaque-schedule-session";
    let state = state_with_alias(session_id);
    let created = set_my_schedule(
        &state,
        &json!({"cron_expr": "@every 3h", "message": "patrol"}),
        &context(session_id),
    );
    let id = created.data.unwrap()["id"].as_i64().unwrap();

    for caller in [GatewayCaller::TrustedUser, GatewayCaller::Agent] {
        let ctx =
            GatewayCallContext::new(caller, "agent-x").with_session_id(session_id.to_string());
        let denied = run_my_schedule(&state, &json!({"id": id}), &ctx);
        assert!(!denied.success);
        assert!(denied.error.unwrap().contains("オーナーまたは co_agent"));
    }

    let owner = GatewayCallContext::new(GatewayCaller::Owner, "agent-x")
        .with_session_id(session_id.to_string());
    let missing = run_my_schedule(&state, &json!({"id": id + 1000}), &owner);
    assert!(!missing.success);
    assert!(missing.error.unwrap().contains("見つかりません"));

    assert!(ensure_owner_or_coagent(&GatewayCallContext::new(
        GatewayCaller::CoAgent {
            agent_id: "peer".to_string()
        },
        "agent-x"
    ))
    .is_none());
}
