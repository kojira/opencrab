use super::*;
use crate::queries::{list_session_participants, upsert_agent, AgentRow};

const INSTANCE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

fn seed(conn: &Connection) {
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
        .query_row("SELECT subject_id FROM agents WHERE agent_id='a1'", [], |r| r.get(0))
        .unwrap();
    conn.execute(
        "INSERT INTO gate_instances
         (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
         VALUES (?1, 'generic', ?2, 1, 1, 'e30=', ?3, 1, 1)",
        params![INSTANCE, subject, "0".repeat(64)],
    )
    .unwrap();
}

fn resolve(conn: &mut Connection, key: &str) -> GateSession {
    let tx = conn.transaction().unwrap();
    let found = resolve_gate_session_in_tx(&tx, INSTANCE, key, None).unwrap();
    tx.commit().unwrap();
    found
}

fn row_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_new_key_creates_the_session_named_by_the_key() {
    let mut conn = crate::init_memory().unwrap();
    seed(&conn);
    let found = resolve(&mut conn, "room-1");
    assert_eq!(found.session_id, "room-1");
    assert_eq!(found.agent_id, "a1");
    assert!(get_session(&conn, "room-1").unwrap().is_some());
    assert_eq!(list_session_participants(&conn, "room-1").unwrap(), ["a1"]);
    let again = resolve(&mut conn, "room-1");
    assert_eq!(again, found, "the same key resolves to the same record");
    assert_eq!(row_count(&conn), 1);
}

#[test]
fn a_migrated_row_keeps_its_session() {
    let mut conn = crate::init_memory().unwrap();
    seed(&conn);
    conn.execute(
        "INSERT INTO sessions (id, theme, created_at, updated_at) VALUES ('extgate-old', 'old', 'x', 'x')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, session_key, created_at, closed_at, session_id)
         VALUES ('11111111-1111-4111-8111-111111111111', ?1, 'room-1', 1, NULL, 'extgate-old')",
        [INSTANCE],
    )
    .unwrap();
    let found = resolve(&mut conn, "room-1");
    assert_eq!(found.session_id, "extgate-old");
    assert_eq!(found.row_id, "11111111-1111-4111-8111-111111111111");
    assert_eq!(row_count(&conn), 1, "no second record is created");
}

#[test]
fn a_closed_row_still_leads_to_its_session() {
    let mut conn = crate::init_memory().unwrap();
    seed(&conn);
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, session_key, created_at, closed_at, session_id)
         VALUES ('22222222-2222-4222-8222-222222222222', ?1, 'room-2', 1, 5, 'room-2')",
        [INSTANCE],
    )
    .unwrap();
    let found = resolve(&mut conn, "room-2");
    assert_eq!(found.row_id, "22222222-2222-4222-8222-222222222222");
    assert_eq!(found.session_id, "room-2");
}

#[test]
fn an_unknown_instance_is_refused() {
    let mut conn = crate::init_memory().unwrap();
    let tx = conn.transaction().unwrap();
    assert!(matches!(
        resolve_gate_session_in_tx(&tx, INSTANCE, "k", None),
        Err(GateSessionError::UnknownInstance)
    ));
}

#[test]
fn a_session_id_leads_back_to_its_key() {
    let mut conn = crate::init_memory().unwrap();
    seed(&conn);
    let created = resolve(&mut conn, "room-3");
    match gate_session_for_session_id(&conn, "room-3").unwrap() {
        GateSessionLookup::Match(found) => assert_eq!(found, created),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        gate_session_for_session_id(&conn, "missing").unwrap(),
        GateSessionLookup::NotFound
    );
}
