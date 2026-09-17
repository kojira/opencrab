use super::*;

#[test]
fn persisted_physical_session_still_admits_heartbeat_and_schedule() {
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
    opencrab_db::queries::upsert_session_heartbeat_config(
        &conn,
        &SessionHeartbeatConfigRow {
            agent_id: AGENT_UUID.into(),
            session_id: session_id.clone(),
            enabled: true,
            interval_secs: Some(600),
            anchor_at: None,
            last_fired_at: None,
        },
    )
    .unwrap();
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &AgentScheduleRow {
            id: None,
            agent_id: AGENT_UUID.into(),
            session_id,
            cron_expr: "@every 10m".into(),
            timezone: "UTC".into(),
            message: "run".into(),
            enabled: true,
            anchor_at: None,
            last_fired_at: None,
        },
    )
    .unwrap();

    let entries = rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new());
    assert_eq!(entries.len(), 2);
}

fn seed_generic_alias_scheduler_rows(
    conn: &mut rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
) -> (String, String) {
    let ids = seed_generic_alias_binding(conn, agent_id, session_id);
    opencrab_db::queries::upsert_session_heartbeat_config(
        conn,
        &SessionHeartbeatConfigRow {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            enabled: true,
            interval_secs: Some(600),
            anchor_at: Some((Utc::now() - Duration::hours(1)).to_rfc3339()),
            last_fired_at: None,
        },
    )
    .unwrap();
    opencrab_db::queries::insert_agent_schedule(
        conn,
        &AgentScheduleRow {
            id: None,
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            cron_expr: "@every 10m".into(),
            timezone: "UTC".into(),
            message: "run".into(),
            enabled: true,
            anchor_at: Some((Utc::now() - Duration::hours(1)).to_rfc3339()),
            last_fired_at: None,
        },
    )
    .unwrap();
    ids
}

#[test]
fn rebuild_requires_persisted_alias_for_heartbeat_and_schedule_without_writes() {
    let mut conn = opencrab_db::init_memory().unwrap();
    let session_id = "opaque-existing-session";
    let (instance_id, binding_id) =
        seed_generic_alias_scheduler_rows(&mut conn, AGENT_UUID, session_id);
    let router = test_router();
    let changes = conn.total_changes();

    let entries = rebuild_entries(&router, &conn, true, 1800, 300, &HashMap::new());
    assert_eq!(
        entries.len(),
        2,
        "valid alias must admit heartbeat and schedule"
    );
    assert!(entries
        .iter()
        .any(|entry| matches!(entry.kind, FireKind::Heartbeat { .. })));
    assert!(entries
        .iter()
        .any(|entry| matches!(entry.kind, FireKind::ScheduledMessage { .. })));
    let repeated = rebuild_entries(&router, &conn, true, 1800, 300, &HashMap::new());
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
    assert!(rebuild_entries(&router, &conn, true, 1800, 300, &HashMap::new()).is_empty());
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
    assert!(rebuild_entries(&router, &conn, true, 1800, 300, &HashMap::new()).is_empty());
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
        rebuild_entries(&router, &conn, true, 1800, 300, &HashMap::new()).is_empty(),
        "ambiguous aliases must admit neither heartbeat nor schedule"
    );
}

#[test]
fn rebuild_rejects_wrong_owner_for_heartbeat_and_schedule() {
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
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    conn.execute("DELETE FROM session_heartbeat_config", [])
        .unwrap();
    conn.execute("DELETE FROM agent_schedules", []).unwrap();
    opencrab_db::queries::upsert_session_heartbeat_config(
        &conn,
        &SessionHeartbeatConfigRow {
            agent_id: "other-agent".into(),
            session_id: session_id.into(),
            enabled: true,
            interval_secs: Some(600),
            anchor_at: None,
            last_fired_at: None,
        },
    )
    .unwrap();
    opencrab_db::queries::insert_agent_schedule(
        &conn,
        &AgentScheduleRow {
            id: None,
            agent_id: "other-agent".into(),
            session_id: session_id.into(),
            cron_expr: "@every 10m".into(),
            timezone: "UTC".into(),
            message: "run".into(),
            enabled: true,
            anchor_at: None,
            last_fired_at: None,
        },
    )
    .unwrap();
    assert!(rebuild_entries(&test_router(), &conn, true, 1800, 300, &HashMap::new()).is_empty());
}
