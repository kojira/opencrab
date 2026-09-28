use super::*;

use std::sync::{Arc, Mutex};

use opencrab_actions::{TimedFireRequest, TimedFireSink};

struct CollectingTimedFireSink {
    destinations: Arc<Mutex<Vec<(String, String)>>>,
    delivered: Arc<tokio::sync::Notify>,
}

impl TimedFireSink for CollectingTimedFireSink {
    fn fire_timed_turn(&self, request: TimedFireRequest) {
        self.destinations
            .lock()
            .unwrap()
            .push((request.binding_id, request.session_id));
        self.delivered.notify_one();
    }
}

struct CollectingS4Sink {
    requests: Arc<Mutex<Vec<TimedFireRequest>>>,
    delivered: Arc<tokio::sync::Notify>,
}

impl TimedFireSink for CollectingS4Sink {
    fn fire_timed_turn(&self, request: TimedFireRequest) {
        self.requests.lock().unwrap().push(request);
        self.delivered.notify_one();
    }
}

#[tokio::test]
async fn s4_scheduler_emits_generic_binding_session_with_row_message_and_advances_last_fired() {
    let mock = Arc::new(crate::bin_test_support::FixedTextMock::new("NO_REPLY"));
    let state = crate::bin_test_support::app_state_with_agent(mock, AGENT_UUID);
    let session_id = "opaque-s4-session";
    let (binding_id, schedule_id) = {
        let mut conn = state.db.lock().unwrap();
        let (_, binding_id) = seed_generic_alias_binding(&mut conn, AGENT_UUID, session_id);
        let schedule_id = opencrab_db::queries::insert_agent_schedule(
            &conn,
            &every_row(
                AGENT_UUID,
                session_id,
                "generic S4 instruction",
                Some((Utc::now() - Duration::hours(1)).to_rfc3339()),
            ),
        )
        .unwrap();
        (binding_id, schedule_id)
    };

    let requests = Arc::new(Mutex::new(Vec::new()));
    let delivered = Arc::new(tokio::sync::Notify::new());
    state
        .timed_fire_router
        .register_sink(Arc::new(CollectingS4Sink {
            requests: Arc::clone(&requests),
            delivered: Arc::clone(&delivered),
        }));
    let scheduler = tokio::spawn(run_scheduler(state.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(3), delivered.notified())
        .await
        .expect("generic schedule did not fire");
    scheduler.abort();
    let _ = scheduler.await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].binding_id, binding_id);
    assert_eq!(requests[0].session_id, session_id);
    assert!(requests[0].prompt.contains("generic S4 instruction"));
    drop(requests);

    let conn = state.db.lock().unwrap();
    let row = opencrab_db::queries::get_agent_schedule(&conn, schedule_id)
        .unwrap()
        .unwrap();
    assert!(
        row.last_fired_at.is_some(),
        "successful live fire advances last_fired_at"
    );
    let rebuilt = rebuild_entries(&test_router(), &conn, &HashMap::new());
    assert!(rebuilt[0].next_fire_at.unwrap() > Utc::now());
}

fn every_row(
    agent_id: &str,
    session_id: &str,
    message: &str,
    anchor_at: Option<String>,
) -> AgentScheduleRow {
    AgentScheduleRow {
        id: None,
        agent_id: agent_id.into(),
        session_id: session_id.into(),
        cron_expr: "@every 10m".into(),
        timezone: "UTC".into(),
        message: message.into(),
        enabled: true,
        anchor_at,
        last_fired_at: None,
    }
}

#[tokio::test]
async fn s3_scheduler_fans_authentic_exact_and_global_sources_to_canonical_destinations() {
    const EXACT_BINDING: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const GLOBAL_BINDING: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const EXACT_ADDRESS: &str = "opaque-exact-address";
    const GLOBAL_ADDRESS: &str = "opaque-global-address";

    let mock = Arc::new(crate::bin_test_support::FixedTextMock::new("NO_REPLY"));
    let state = crate::bin_test_support::app_state_with_agent(mock, AGENT_UUID);
    let global_session = format!("extgate-{GLOBAL_BINDING}");
    {
        let mut conn = state.db.lock().unwrap();
        let (instance_id, exact_binding) =
            seed_generic_alias_binding(&mut conn, AGENT_UUID, EXACT_ADDRESS);
        assert_eq!(exact_binding, EXACT_BINDING);

        // The exact source reuses an existing address-named session. The global-address source has
        // no exact session and therefore uses the binding's canonical physical session.
        let tx = conn.transaction().unwrap();
        opencrab_db::queries::create_gate_binding_in_tx(
            &tx,
            GLOBAL_BINDING,
            &instance_id,
            GLOBAL_ADDRESS,
            "global fallback",
            2,
        )
        .unwrap();
        tx.commit().unwrap();

        let exact_session: String = conn
            .query_row(
                "SELECT session_id FROM gate_bindings WHERE binding_id = ?1",
                [EXACT_BINDING],
                |row| row.get(0),
            )
            .unwrap();
        let global_fallback_session: String = conn
            .query_row(
                "SELECT session_id FROM gate_bindings WHERE binding_id = ?1",
                [GLOBAL_BINDING],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exact_session, EXACT_ADDRESS);
        assert_eq!(global_fallback_session, global_session);

        let old_anchor = (Utc::now() - Duration::hours(1)).to_rfc3339();
        for session_id in [EXACT_ADDRESS, global_session.as_str()] {
            opencrab_db::queries::insert_agent_schedule(
                &conn,
                &every_row(AGENT_UUID, session_id, "run", Some(old_anchor.clone())),
            )
            .unwrap();
        }
    }

    let destinations = Arc::new(Mutex::new(Vec::new()));
    let delivered = Arc::new(tokio::sync::Notify::new());
    state
        .timed_fire_router
        .register_sink(Arc::new(CollectingTimedFireSink {
            destinations: Arc::clone(&destinations),
            delivered: Arc::clone(&delivered),
        }));
    let scheduler = tokio::spawn(run_scheduler(state.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if destinations.lock().unwrap().len() >= 2 {
                break;
            }
            delivered.notified().await;
        }
    })
    .await
    .expect("production scheduler did not route both sources");
    scheduler.abort();
    let _ = scheduler.await;

    let mut actual = destinations.lock().unwrap().clone();
    actual.sort();
    let mut expected = vec![
        (EXACT_BINDING.to_string(), EXACT_ADDRESS.to_string()),
        (GLOBAL_BINDING.to_string(), global_session),
    ];
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 2);
}

#[test]
fn persisted_physical_session_still_admits_schedule() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let (instance_id, _) =
        seed_generic_alias_binding(&mut conn, AGENT_UUID, "opaque-existing-session");
    let binding_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let session_id = format!("extgate-{binding_id}");
    let tx = conn.transaction().unwrap();
    opencrab_db::queries::create_gate_binding_in_tx(
        &tx,
        binding_id,
        &instance_id,
        "unclaimed-address",
        "physical",
        2,
    )
    .unwrap();
    tx.commit().unwrap();
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &every_row(AGENT_UUID, &session_id, "run", None),
    )
    .unwrap();

    let entries = rebuild_entries(&test_router(), &conn, &HashMap::new());
    assert_eq!(entries.len(), 1);
}

fn seed_generic_alias_scheduler_rows(
    conn: &mut rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
) -> (String, String) {
    let ids = seed_generic_alias_binding(conn, agent_id, session_id);
    opencrab_db::queries::insert_agent_schedule(
        conn,
        &every_row(
            agent_id,
            session_id,
            "run",
            Some((Utc::now() - Duration::hours(1)).to_rfc3339()),
        ),
    )
    .unwrap();
    ids
}

#[test]
fn rebuild_requires_persisted_alias_for_schedule_without_writes() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let session_id = "opaque-existing-session";
    let (instance_id, binding_id) =
        seed_generic_alias_scheduler_rows(&mut conn, AGENT_UUID, session_id);
    let router = test_router();
    let changes = conn.total_changes();

    let entries = rebuild_entries(&router, &conn, &HashMap::new());
    assert_eq!(entries.len(), 1, "valid alias must admit the schedule");
    let repeated = rebuild_entries(&router, &conn, &HashMap::new());
    assert_eq!(
        repeated.len(),
        entries.len(),
        "rebuild result must be stable"
    );
    assert_eq!(conn.total_changes(), changes, "rebuild must be read-only");

    conn.execute(
        "UPDATE gate_bindings SET closed_at = 2 WHERE binding_id = ?1",
        [&binding_id],
    )
    .unwrap();
    assert!(rebuild_entries(&router, &conn, &HashMap::new()).is_empty());
    conn.execute(
        "UPDATE gate_bindings SET closed_at = NULL WHERE binding_id = ?1",
        [&binding_id],
    )
    .unwrap();
    conn.execute(
        "UPDATE gate_instances SET deleted_at = 3 WHERE instance_id = ?1",
        [&instance_id],
    )
    .unwrap();
    assert!(rebuild_entries(&router, &conn, &HashMap::new()).is_empty());
    conn.execute(
        "UPDATE gate_instances SET deleted_at = NULL WHERE instance_id = ?1",
        [&instance_id],
    )
    .unwrap();
    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            [AGENT_UUID],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES ('cccccccc-cccc-4ccc-8ccc-cccccccccccc', 'generic', ?1, 1, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject_id, "0".repeat(64)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
         VALUES ('dddddddd-dddd-4ddd-8ddd-dddddddddddd',
                 'cccccccc-cccc-4ccc-8ccc-cccccccccccc', ?1, 4)",
        [session_id],
    )
    .unwrap();
    assert!(
        rebuild_entries(&router, &conn, &HashMap::new()).is_empty(),
        "ambiguous aliases must not admit the schedule"
    );
}

#[test]
fn rebuild_rejects_wrong_owner_for_schedule() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let session_id = "opaque-owned-session";
    seed_generic_alias_scheduler_rows(&mut conn, AGENT_UUID, session_id);
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: "other-agent".into(),
            name: "other".into(),
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
    conn.execute("DELETE FROM agent_schedules", []).unwrap();
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &every_row("other-agent", session_id, "run", None),
    )
    .unwrap();
    assert!(rebuild_entries(&test_router(), &conn, &HashMap::new()).is_empty());
}

/// #612 §8.2 / RED-4: 同じセッションで `@every` と cron の 2 行が同時に due になったら、
/// 両方とも TimedFire sink に届き、各プロンプトは自分の message を含む。
#[tokio::test]
async fn same_session_every_and_cron_rows_both_reach_timed_fire_sink_with_own_message() {
    let mock = Arc::new(crate::bin_test_support::FixedTextMock::new("NO_REPLY"));
    let state = crate::bin_test_support::app_state_with_agent(mock, AGENT_UUID);
    let session_id = "opaque-concurrent-session";
    let long_ago = (Utc::now() - Duration::days(2)).to_rfc3339();
    let binding_id = {
        let mut conn = state.db.lock().unwrap();
        let (_, binding_id) = seed_generic_alias_binding(&mut conn, AGENT_UUID, session_id);
        for (cron_expr, message) in [
            ("@every 10m", "interval trigger message"),
            ("0 7 * * *", "daily trigger message"),
        ] {
            opencrab_db::queries::insert_agent_schedule(
                &conn,
                &AgentScheduleRow {
                    id: None,
                    agent_id: AGENT_UUID.into(),
                    session_id: session_id.into(),
                    cron_expr: cron_expr.into(),
                    timezone: "Asia/Tokyo".into(),
                    message: message.into(),
                    enabled: true,
                    anchor_at: Some(long_ago.clone()),
                    last_fired_at: None,
                },
            )
            .unwrap();
        }
        binding_id
    };

    let requests = Arc::new(Mutex::new(Vec::new()));
    let delivered = Arc::new(tokio::sync::Notify::new());
    state
        .timed_fire_router
        .register_sink(Arc::new(CollectingS4Sink {
            requests: Arc::clone(&requests),
            delivered: Arc::clone(&delivered),
        }));
    let scheduler = tokio::spawn(run_scheduler(state.clone()));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if requests.lock().unwrap().len() >= 2 {
                break;
            }
            delivered.notified().await;
        }
    })
    .await
    .expect("both due rows must reach the timed-fire sink");
    scheduler.abort();
    let _ = scheduler.await;

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.binding_id, binding_id);
        assert_eq!(request.session_id, session_id);
    }
    let interval: Vec<_> = requests
        .iter()
        .filter(|r| r.prompt.contains("interval trigger message"))
        .collect();
    let daily: Vec<_> = requests
        .iter()
        .filter(|r| r.prompt.contains("daily trigger message"))
        .collect();
    assert_eq!(
        interval.len(),
        1,
        "interval row prompt carries its own message"
    );
    assert_eq!(daily.len(), 1, "daily row prompt carries its own message");
    assert!(!interval[0].prompt.contains("daily trigger message"));
    drop(requests);

    let conn = state.db.lock().unwrap();
    for row in opencrab_db::queries::list_agent_schedules(&conn, AGENT_UUID).unwrap() {
        assert!(
            row.last_fired_at.is_some(),
            "sink accepted → last_fired_at advances"
        );
    }
}
