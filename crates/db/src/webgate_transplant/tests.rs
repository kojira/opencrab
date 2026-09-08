use super::*;
use crate::init_memory;
use crate::queries::{insert_session, insert_session_log, SessionLogRow, SessionRow};

fn agent(conn: &Connection, id: &str) {
    crate::queries::upsert_agent(
        conn,
        &crate::queries::AgentRow {
            agent_id: id.into(),
            name: id.into(),
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
}

fn put_web_binding(conn: &Connection, agent_id: &str, logical: &str, binding_id: &str) {
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )
        .unwrap();
    let instance = format!("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa{subject:02}");
    let now = 1i64;
    conn.execute(
        "INSERT OR IGNORE INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES (?1, 'web', ?2, 1, 1, 'e30=', '0000000000000000000000000000000000000000000000000000000000000000', ?3, ?3)",
        params![instance, subject, now],
    )
    .unwrap();
    let physical = session_id_for_binding(binding_id);
    let ts = chrono::Utc::now().to_rfc3339();
    crate::queries::insert_session_in_tx(
        &conn.unchecked_transaction().unwrap(),
        &physical,
        logical,
        &ts,
    )
    .ok();
    conn.execute(
        "INSERT OR IGNORE INTO sessions (id, theme, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
        params![physical, logical, ts],
    )
    .unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO agent_sessions (agent_id, session_id) VALUES (?1, ?2)",
        params![agent_id, physical],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![binding_id, instance, logical, now],
    )
    .unwrap();
}

fn legacy_session(conn: &Connection, id: &str, agent_id: &str) {
    insert_session(
        conn,
        &SessionRow {
            id: id.into(),
            mode: "web".into(),
            theme: id.into(),
            phase: "divergent".into(),
            turn_number: 0,
            status: "active".into(),
            participant_ids_json: serde_json::to_string(&vec![agent_id]).unwrap(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: Some(r#"{"keep":true}"#.into()),
        },
    )
    .unwrap();
}

fn speech(conn: &Connection, agent_id: &str, session: &str, text: &str) {
    insert_session_log(
        conn,
        &SessionLogRow {
            id: None,
            agent_id: agent_id.into(),
            session_id: session.into(),
            log_type: "speech".into(),
            content: text.into(),
            speaker_id: Some(agent_id.into()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap();
}

#[test]
fn transplant_zero_one_and_rerun() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    let logical = "web-a1-c1";
    legacy_session(&conn, logical, "a1");
    put_web_binding(&conn, "a1", logical, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
    let m = list_web_mappings(&conn).unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(
        transplant_mapping(&conn, &m[0]).unwrap(),
        TransplantOutcome::Migrated
    );
    assert_eq!(
        transplant_mapping(&conn, &m[0]).unwrap(),
        TransplantOutcome::WriteZero
    );

    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    let logical = "web-a1-c1";
    legacy_session(&conn, logical, "a1");
    put_web_binding(&conn, "a1", logical, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
    speech(&conn, "a1", logical, "hello");
    let m = list_web_mappings(&conn).unwrap();
    let before = snapshot_session(&conn, logical).unwrap();
    assert_eq!(before["memory_sessions"].count, 1);
    assert_eq!(
        transplant_mapping(&conn, &m[0]).unwrap(),
        TransplantOutcome::Migrated
    );
    let after_l = snapshot_session(&conn, logical).unwrap();
    let after_p = snapshot_session(&conn, &m[0].physical).unwrap();
    assert_eq!(after_l["memory_sessions"].count, 0);
    assert_eq!(after_p["memory_sessions"].count, 1);
    assert_eq!(
        after_p["memory_sessions"].digest,
        before["memory_sessions"].digest
    );
    let fts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions_fts WHERE session_id = ?1",
            [&m[0].physical],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts, 1);
    let meta: String = conn
        .query_row(
            "SELECT metadata_json FROM sessions WHERE id = ?1",
            [logical],
            |r| r.get(0),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(v["keep"], true);
    assert_eq!(v[WEBGATE_MARKER], m[0].physical);
    assert_eq!(
        transplant_mapping(&conn, &m[0]).unwrap(),
        TransplantOutcome::WriteZero
    );
}

#[test]
fn transplant_rejects_prefix_mismatch() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    agent(&conn, "a1x");
    let logical = "web-a1x-c";
    legacy_session(&conn, logical, "a1x");
    put_web_binding(&conn, "a1", logical, "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee");
    let m = list_web_mappings(&conn).unwrap();
    assert!(transplant_mapping(&conn, &m[0]).is_err());
}

#[test]
fn transplant_rejects_multiple_participants() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    agent(&conn, "a2");
    let logical = "web-a1-c2";
    insert_session(
        &conn,
        &SessionRow {
            id: logical.into(),
            mode: "web".into(),
            theme: logical.into(),
            phase: "divergent".into(),
            turn_number: 0,
            status: "active".into(),
            participant_ids_json: r#"["a1","a2"]"#.into(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: None,
        },
    )
    .unwrap();
    put_web_binding(&conn, "a1", logical, "cccccccc-cccc-4ccc-8ccc-cccccccccccc");
    let m = list_web_mappings(&conn).unwrap();
    assert!(transplant_mapping(&conn, &m[0]).is_err());
}

#[test]
fn transplant_ten_thousand_logs_preserve_order() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    let logical = "web-a1-big";
    legacy_session(&conn, logical, "a1");
    put_web_binding(&conn, "a1", logical, "dddddddd-dddd-4ddd-8ddd-dddddddddddd");
    for i in 0..10_000 {
        speech(&conn, "a1", logical, &format!("m{i}"));
    }
    let before = snapshot_session(&conn, logical).unwrap();
    assert_eq!(before["memory_sessions"].count, 10_000);
    let m = list_web_mappings(&conn).unwrap();
    transplant_mapping(&conn, &m[0]).unwrap();
    let after = snapshot_session(&conn, &m[0].physical).unwrap();
    assert_eq!(after["memory_sessions"].count, 10_000);
    assert_eq!(
        after["memory_sessions"].digest,
        before["memory_sessions"].digest
    );
    let first: String = conn
        .query_row(
            "SELECT content FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC LIMIT 1",
            [&m[0].physical],
            |r| r.get(0),
        )
        .unwrap();
    let last: String = conn
        .query_row(
            "SELECT content FROM memory_sessions WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
            [&m[0].physical],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(first, "m0");
    assert_eq!(last, "m9999");
}

#[test]
fn transplant_zero_logs_still_validates_prefix() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    agent(&conn, "a1x");
    let logical = "web-a1x-c";
    legacy_session(&conn, logical, "a1x");
    put_web_binding(&conn, "a1", logical, "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee");
    let m = list_web_mappings(&conn).unwrap();
    assert!(transplant_mapping(&conn, &m[0]).is_err());
}

#[test]
fn transplant_rerun_fails_when_physical_digest_changes() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    let logical = "web-a1-c1";
    legacy_session(&conn, logical, "a1");
    put_web_binding(&conn, "a1", logical, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
    speech(&conn, "a1", logical, "hello");
    let m = list_web_mappings(&conn).unwrap();
    assert_eq!(
        transplant_mapping(&conn, &m[0]).unwrap(),
        TransplantOutcome::Migrated
    );
    conn.execute(
        "UPDATE memory_sessions SET content = 'tampered' WHERE session_id = ?1",
        [&m[0].physical],
    )
    .unwrap();
    let err = transplant_mapping(&conn, &m[0]).unwrap_err();
    assert!(err.to_string().contains("mismatch"), "{err}");
}

#[test]
fn transplant_mixed_legacy_and_physical_digest() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    let logical = "web-a1-mix";
    legacy_session(&conn, logical, "a1");
    put_web_binding(&conn, "a1", logical, "ffffffff-ffff-4fff-8fff-ffffffffffff");
    speech(&conn, "a1", logical, "legacy");
    speech(
        &conn,
        "a1",
        &list_web_mappings(&conn).unwrap()[0].physical,
        "phys",
    );
    let m = list_web_mappings(&conn).unwrap();
    let expected = expected_after(&conn, &m[0]).unwrap();
    assert_eq!(expected["memory_sessions"].count, 2);
    assert_eq!(
        transplant_mapping(&conn, &m[0]).unwrap(),
        TransplantOutcome::Migrated
    );
    let after = snapshot_session(&conn, &m[0].physical).unwrap();
    assert_eq!(after["memory_sessions"], expected["memory_sessions"]);
    assert_eq!(
        after["memory_sessions_fts"],
        expected["memory_sessions_fts"]
    );
    assert_eq!(after["agent_sessions"], expected["agent_sessions"]);
    assert_eq!(
        transplant_mapping(&conn, &m[0]).unwrap(),
        TransplantOutcome::WriteZero
    );
}

#[test]
fn transplant_rejects_invalid_utf8() {
    let conn = init_memory().unwrap();
    agent(&conn, "a1");
    let logical = "web-a1-bin";
    legacy_session(&conn, logical, "a1");
    put_web_binding(&conn, "a1", logical, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaa99");
    conn.execute(
        "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, created_at)
         VALUES ('a1', ?1, 'speech', x'c3', datetime('now'))",
        [logical],
    )
    .unwrap();
    let m = list_web_mappings(&conn).unwrap();
    let err = transplant_mapping(&conn, &m[0]).unwrap_err();
    assert!(err.to_string().contains("invalid utf-8"), "{err}");
}
