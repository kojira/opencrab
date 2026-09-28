use super::*;
use crate::queries::{get_session, upsert_agent, AgentRow};
use rusqlite::TransactionBehavior;

fn seed_agent_and_instance(conn: &rusqlite::Connection) -> (String, i64) {
    upsert_agent(
        conn,
        &AgentRow {
            agent_id: "a1".into(),
            name: "a1".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = 'a1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let instance = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES (?1, 'web', ?2, 1, 1, 'e30=', '0000000000000000000000000000000000000000000000000000000000000000', 1, 1)",
        rusqlite::params![instance, subject],
    )
    .unwrap();
    (instance.to_string(), subject)
}

fn counts(conn: &rusqlite::Connection) -> (i64, i64, i64) {
    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    let members: i64 = conn
        .query_row("SELECT COUNT(*) FROM agent_sessions", [], |row| row.get(0))
        .unwrap();
    let bindings: i64 = conn
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |row| row.get(0))
        .unwrap();
    (sessions, members, bindings)
}

#[test]
fn create_writes_session_membership_binding_with_theme() {
    set_binding_tx_fail(FAIL_NONE);
    let mut conn = crate::init_memory().unwrap();
    let (instance, _) = seed_agent_and_instance(&conn);
    let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let address = "web-a1-c1";
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    create_gate_binding_in_tx(
        &tx,
        binding,
        &instance,
        address,
        "My Name",
        1_700_000_000_000_000_000,
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(counts(&conn), (1, 1, 1));
    let row = get_session(&conn, &format!("extgate-{binding}"))
        .unwrap()
        .unwrap();
    assert_eq!(row.theme, "My Name");
    let stored_address: String = conn
        .query_row(
            "SELECT address FROM gate_bindings WHERE binding_id = ?1",
            [binding],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_address, address);
}

#[test]
fn s2_binding_creation_is_byte_idempotent_through_the_generic_authority() {
    set_binding_tx_fail(FAIL_NONE);
    let mut conn = crate::init_memory().unwrap();
    let (instance, _) = seed_agent_and_instance(&conn);
    let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    for _ in 0..2 {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        create_gate_binding_in_tx(
            &tx,
            binding,
            &instance,
            "opaque-address",
            "Stable title",
            1_700_000_000_000_000_000,
        )
        .expect("byte-identical binding creation must be idempotent");
        tx.commit().unwrap();
    }
    assert_eq!(counts(&conn), (1, 1, 1));
}

#[test]
fn s2_concurrent_byte_identical_binding_creation_converges_to_one_row() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("binding-race.sqlite");
    let db = crate::Db::open(path.to_str().unwrap()).unwrap();
    let instance = {
        let conn = db.lock().unwrap();
        seed_agent_and_instance(&conn).0
    };
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for _ in 0..2 {
        let db = db.clone();
        let instance = instance.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut conn = db.lock().unwrap();
            barrier.wait();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let outcome = CoreBindingService::create_in_tx(
                &tx,
                &CoreBindingRequest {
                    binding_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                    instance_id: &instance,
                    address: "race-address",
                    session_id: "race-session",
                    session_title: "Race title",
                    now: 1_700_000_000_000_000_000,
                },
            )
            .unwrap();
            tx.commit().unwrap();
            outcome
        }));
    }
    barrier.wait();
    let mut outcomes = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    outcomes.sort_by_key(|outcome| match outcome {
        CoreBindingOutcome::Created => 0,
        CoreBindingOutcome::Existing => 1,
    });
    assert_eq!(
        outcomes,
        vec![CoreBindingOutcome::Created, CoreBindingOutcome::Existing]
    );
    let conn = db.lock().unwrap();
    assert_eq!(counts(&conn), (1, 1, 1));
}

#[test]
fn s2_deleted_instance_cannot_create_a_binding() {
    let mut conn = crate::init_memory().unwrap();
    let (instance, _) = seed_agent_and_instance(&conn);
    conn.execute(
        "UPDATE gate_instances SET deleted_at=2 WHERE instance_id=?1",
        [&instance],
    )
    .unwrap();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let outcome = CoreBindingService::create_in_tx(
        &tx,
        &CoreBindingRequest {
            binding_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            instance_id: &instance,
            address: "opaque-address",
            session_id: "opaque-session",
            session_title: "Opaque title",
            now: 3,
        },
    );
    assert!(outcome.is_err(), "deleted instance accepted a new binding");
    tx.rollback().unwrap();
    assert_eq!(counts(&conn), (0, 0, 0));
}
