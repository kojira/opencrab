//! Issue #1006 S3 generic timed-fire routing contract.
use std::sync::{Arc, Mutex};

use opencrab_actions::{CallerIdentity, TimedFireRequest, TimedFireRouter, TimedFireSink};
use opencrab_db::queries::{
    create_gate_binding_in_tx, insert_session, upsert_agent, AgentRow, SessionRow,
};

#[test]
fn generic_router_has_no_static_kind_or_descriptor_registry() {
    let router = TimedFireRouter::new();
    assert!(!router.has_live_sink());
    assert_eq!(router.fire_target_hint(), "ゲートに接続した会話");
}

#[test]
fn persisted_route_uses_only_canonical_binding_and_session_ids() {
    let conn = opencrab_db::init_memory().unwrap();
    assert!(TimedFireRouter::new()
        .resolve_persisted_target(&conn, "missing", "agent")
        .is_none());
}

fn insert_session_fixture(conn: &rusqlite::Connection, session_id: &str) {
    insert_session(
        conn,
        &SessionRow {
            id: session_id.into(),
            mode: "solo".into(),
            theme: session_id.into(),
            phase: "convergent".into(),
            turn_number: 0,
            status: "active".into(),
            participant_ids_json: r#"["agent-1"]"#.into(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

fn routing_fixture() -> rusqlite::Connection {
    let mut conn = opencrab_db::init_memory().unwrap();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "agent-1".into(),
            name: "agent-1".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "agent-1".into(),
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
            "SELECT subject_id FROM agents WHERE agent_id = 'agent-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES ('generic-instance', 'generic', ?1, 1, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject_id, "0".repeat(64)],
    )
    .unwrap();

    let fixtures = [
        ("exact-session", "11111111-1111-4111-8111-111111111111", 1),
        ("global-session", "22222222-2222-4222-8222-222222222222", 2),
    ];
    for (session_id, binding_id, created_at) in fixtures {
        insert_session_fixture(&conn, session_id);
        let tx = conn.transaction().unwrap();
        create_gate_binding_in_tx(
            &tx,
            binding_id,
            "generic-instance",
            session_id,
            session_id,
            created_at,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    conn
}

struct CollectingSink(Arc<Mutex<Vec<(String, String)>>>);

impl TimedFireSink for CollectingSink {
    fn fire_timed_turn(&self, request: TimedFireRequest) {
        self.0
            .lock()
            .unwrap()
            .push((request.binding_id, request.session_id));
    }
}

#[test]
fn s3_exact_and_global_fixtures_fan_out_through_canonical_timed_fire_routes() {
    let conn = routing_fixture();
    let router = TimedFireRouter::new();
    let delivered = Arc::new(Mutex::new(Vec::new()));
    router.register_sink(Arc::new(CollectingSink(Arc::clone(&delivered))));

    let expected = [
        ("exact-session", "11111111-1111-4111-8111-111111111111"),
        ("global-session", "22222222-2222-4222-8222-222222222222"),
    ];
    let targets: Vec<_> = expected
        .iter()
        .map(|(session_id, _)| {
            router
                .resolve_persisted_target(&conn, session_id, "agent-1")
                .expect("fixture must resolve through canonical production routing")
        })
        .collect();
    assert_eq!(targets.len(), 2);

    for target in &targets {
        router.resolve().unwrap().fire_timed_turn(TimedFireRequest {
            binding_id: target.binding_id.clone(),
            session_id: target.session_id.clone(),
            agent_id: "agent-1".into(),
            prompt: "route proof".into(),
            caller: CallerIdentity::Owner,
        });
    }

    let delivered = delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert_eq!(
        delivered,
        expected
            .iter()
            .map(|(session_id, binding_id)| (binding_id.to_string(), session_id.to_string()))
            .collect::<Vec<_>>()
    );
}
