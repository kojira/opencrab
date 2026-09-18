use super::*;
use crate::queries::{insert_session, upsert_agent, AgentRow, SessionRow};

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
            heartbeat_instructions: String::new(),
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
         VALUES (?1, 'generic', ?2, 1, 1, 'e30=', ?3, 1, 1)",
        rusqlite::params![instance, subject, "0".repeat(64)],
    )
    .unwrap();
    (instance.to_string(), subject)
}

fn counts(conn: &rusqlite::Connection) -> (i64, i64, i64) {
    let sessions = conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
        .unwrap();
    let members = conn
        .query_row("SELECT COUNT(*) FROM agent_sessions", [], |row| row.get(0))
        .unwrap();
    let bindings = conn
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |row| row.get(0))
        .unwrap();
    (sessions, members, bindings)
}

fn insert_named_session(conn: &rusqlite::Connection, id: &str, agent_id: &str) {
    insert_session(
        conn,
        &SessionRow {
            id: id.into(),
            mode: "solo".into(),
            theme: id.into(),
            phase: "convergent".into(),
            turn_number: 0,
            status: "active".into(),
            participant_ids_json: format!(r#"["{agent_id}"]"#),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
}

fn seed_reused_address(conn: &mut Connection, address: &str) -> (String, String) {
    let (instance, _) = seed_agent_and_instance(conn);
    insert_named_session(conn, address, "a1");
    let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".to_string();
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    create_gate_binding_in_tx(&tx, &binding, &instance, address, address, 1).unwrap();
    tx.commit().unwrap();
    (binding, instance)
}

#[test]
fn lookup_canonical_gate_binding_resolves_reused_exact_address_without_writes() {
    let mut conn = crate::init_memory().unwrap();
    let address = "opaque-existing-session";
    let (binding, _) = seed_reused_address(&mut conn, address);
    let changes_before = conn.total_changes();

    for _ in 0..20 {
        assert_eq!(
            lookup_canonical_gate_binding(&conn, address).unwrap(),
            CanonicalGateBindingLookup::Match(CanonicalGateBinding {
                binding_id: binding.clone(),
                agent_id: "a1".into(),
            })
        );
    }
    assert_eq!(conn.total_changes(), changes_before);
    assert_eq!(counts(&conn), (1, 1, 1));
}

#[test]
fn lookup_canonical_gate_binding_preserves_physical_and_rejects_noncanonical_address() {
    let mut conn = crate::init_memory().unwrap();
    let (instance, _) = seed_agent_and_instance(&conn);
    let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let tx = conn.transaction().unwrap();
    create_gate_binding_in_tx(&tx, binding, &instance, "opaque-address", "opaque", 1).unwrap();
    tx.commit().unwrap();

    assert_eq!(
        lookup_canonical_gate_binding(&conn, &format!("extgate-{binding}")).unwrap(),
        CanonicalGateBindingLookup::Match(CanonicalGateBinding {
            binding_id: binding.into(),
            agent_id: "a1".into(),
        })
    );
    assert_eq!(
        lookup_canonical_gate_binding(&conn, "opaque-address").unwrap(),
        CanonicalGateBindingLookup::NotFound
    );
}

#[test]
fn lookup_canonical_gate_binding_rejects_closed_deleted_and_reports_ambiguity() {
    let mut conn = crate::init_memory().unwrap();
    let address = "opaque-shared-session";
    let (binding, instance) = seed_reused_address(&mut conn, address);

    conn.execute(
        "UPDATE gate_bindings SET closed_at = 2 WHERE binding_id = ?1",
        [&binding],
    )
    .unwrap();
    assert_eq!(
        lookup_canonical_gate_binding(&conn, address).unwrap(),
        CanonicalGateBindingLookup::NotFound
    );
    conn.execute(
        "UPDATE gate_bindings SET closed_at = NULL WHERE binding_id = ?1",
        [&binding],
    )
    .unwrap();
    conn.execute(
        "UPDATE gate_instances SET deleted_at = 3 WHERE instance_id = ?1",
        [&instance],
    )
    .unwrap();
    assert_eq!(
        lookup_canonical_gate_binding(&conn, address).unwrap(),
        CanonicalGateBindingLookup::NotFound
    );
    conn.execute(
        "UPDATE gate_instances SET deleted_at = NULL WHERE instance_id = ?1",
        [&instance],
    )
    .unwrap();

    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = 'a1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES ('other-instance', 'generic', ?1, 1, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject, "0".repeat(64)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
         VALUES ('cccccccc-cccc-4ccc-8ccc-cccccccccccc', 'other-instance', ?1, 4)",
        [address],
    )
    .unwrap();
    assert_eq!(
        lookup_canonical_gate_binding(&conn, address).unwrap(),
        CanonicalGateBindingLookup::Ambiguous
    );
}

#[test]
fn canonical_address_query_uses_address_first_partial_index() {
    let mut conn = crate::init_memory().unwrap();
    let (instance, _) = seed_agent_and_instance(&conn);
    let tx = conn.transaction().unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO gate_bindings
                 (binding_id, instance_id, address, created_at, closed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .unwrap();
        for n in 0..10_000i64 {
            insert
                .execute(rusqlite::params![
                    format!("binding-{n:05}"),
                    instance,
                    format!("address-{n:05}"),
                    n,
                    (n % 2 == 0).then_some(n + 1),
                ])
                .unwrap();
        }
    }
    tx.commit().unwrap();
    let detail: Vec<String> = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {CANONICAL_ADDRESS_CANDIDATES_SQL}"
        ))
        .unwrap()
        .query_map(["address-09999"], |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        detail
            .iter()
            .any(|line| line.contains("idx_gate_bindings_open_address_lookup")),
        "address lookup did not select the address-first partial index: {detail:?}"
    );
    assert!(
        detail.iter().all(|line| !line.contains("SCAN b")),
        "address lookup scanned gate_bindings: {detail:?}"
    );
}

#[test]
#[ignore = "100k-row scale probe; run explicitly with --ignored --nocapture"]
fn lookup_canonical_gate_binding_scale() {
    let mut conn = crate::init_memory().unwrap();
    let (instance, _) = seed_agent_and_instance(&conn);
    insert_named_session(&conn, "scale-target", "a1");
    let tx = conn.transaction().unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO gate_bindings
                 (binding_id, instance_id, address, created_at, closed_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
            )
            .unwrap();
        for n in 0..100_000i64 {
            insert
                .execute(rusqlite::params![
                    format!("scale-binding-{n:06}"),
                    instance,
                    format!("scale-address-{n:06}"),
                    n,
                ])
                .unwrap();
        }
        insert
            .execute(rusqlite::params![
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                instance,
                "scale-target",
                100_001i64,
            ])
            .unwrap();
    }
    tx.commit().unwrap();

    let started = std::time::Instant::now();
    for _ in 0..1_000 {
        assert!(matches!(
            lookup_canonical_gate_binding(&conn, "scale-target").unwrap(),
            CanonicalGateBindingLookup::Match(_)
        ));
        assert_eq!(
            lookup_canonical_gate_binding(&conn, "scale-missing").unwrap(),
            CanonicalGateBindingLookup::NotFound
        );
    }
    eprintln!(
        "100k gate bindings, 1000 hit+miss lookups: {:?}",
        started.elapsed()
    );
}
