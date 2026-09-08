//! Binding 永続化の唯一の入口。V3 Binding PUT と Web 会話作成が同じ TX 部品を使う。
//!
//! V3.5: `address` が既存 session id と byte 一致なら新 session を insert せず再利用する。

use std::cell::Cell;

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection, Transaction};

use super::{
    get_session, insert_agent_session_in_tx, insert_session_in_tx, list_session_participants,
};

/// `create_gate_binding_in_tx` の失敗。membership / 占有の不一致は Conflict。
#[derive(Debug)]
pub enum CreateGateBindingError {
    Conflict,
    Store(anyhow::Error),
}

impl From<anyhow::Error> for CreateGateBindingError {
    fn from(e: anyhow::Error) -> Self {
        Self::Store(e)
    }
}

impl From<rusqlite::Error> for CreateGateBindingError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.into())
    }
}

thread_local! {
    static BINDING_TX_FAIL: Cell<u8> = const { Cell::new(0) };
}

pub const FAIL_NONE: u8 = 0;
pub const FAIL_SESSION: u8 = 1;
pub const FAIL_MEMBERSHIP: u8 = 2;
pub const FAIL_BINDING: u8 = 3;
pub const FAIL_NAME: u8 = 4;
pub const FAIL_COMMIT: u8 = 5;

/// テスト専用。現在スレッドの次の `create_gate_binding_in_tx` だけに効く。
pub fn set_binding_tx_fail(step: u8) {
    BINDING_TX_FAIL.with(|c| c.set(step));
}

fn fail_step(step: u8) -> Result<()> {
    if BINDING_TX_FAIL.with(|c| c.get()) == step {
        anyhow::bail!("injected gate binding failure at step {step}");
    }
    Ok(())
}

/// 呼び出し側が commit 直前に見る。注入時は commit せず rollback する。
pub fn injected_commit_failure() -> bool {
    BINDING_TX_FAIL.with(|c| c.get()) == FAIL_COMMIT
}

fn rfc3339_from_nanos(now: i64) -> Result<String> {
    let dt = chrono::DateTime::<Utc>::from_timestamp_nanos(now);
    Ok(dt.to_rfc3339())
}

fn agent_id_for_instance(tx: &Transaction<'_>, instance_id: &str) -> Result<String> {
    let mut stmt = tx.prepare(
        "SELECT a.agent_id
         FROM gate_instances i
         JOIN agents a ON a.subject_id = i.subject_id
         WHERE i.instance_id = ?1",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![instance_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    match ids.as_slice() {
        [id] => Ok(id.clone()),
        _ => anyhow::bail!("instance {instance_id} has no unique agent"),
    }
}

fn insert_binding_row(
    tx: &Transaction<'_>,
    binding_id: &str,
    instance_id: &str,
    address: &str,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at, closed_at)
         VALUES (?1, ?2, ?3, ?4, NULL)",
        params![binding_id, instance_id, address, now],
    )?;
    Ok(())
}

/// session / membership / binding を同一 TX で書く。commit は呼び出し側。
///
/// `session_theme` は Binding PUT では address、Web 作成では normalized name
/// （未指定時は address）。address が既存 session id と byte 一致ならその session を
/// 再利用し `extgate-{binding_id}` は作らない（V3.5）。無い場合だけ従来の新設。
pub fn create_gate_binding_in_tx(
    tx: &Transaction<'_>,
    binding_id: &str,
    instance_id: &str,
    address: &str,
    session_theme: &str,
    now: i64,
) -> std::result::Result<(), CreateGateBindingError> {
    let agent_id = agent_id_for_instance(tx, instance_id)?;
    if get_session(tx, address)?.is_some() {
        return reuse_existing_session(tx, binding_id, instance_id, address, &agent_id, now);
    }

    let session_id = format!("extgate-{binding_id}");
    let now_rfc = rfc3339_from_nanos(now)?;

    fail_step(FAIL_SESSION)?;
    if session_theme != address {
        fail_step(FAIL_NAME)?;
    }
    insert_session_in_tx(tx, &session_id, session_theme, &now_rfc)?;
    fail_step(FAIL_MEMBERSHIP)?;
    insert_agent_session_in_tx(tx, &agent_id, &session_id)?;
    fail_step(FAIL_BINDING)?;
    insert_binding_row(tx, binding_id, instance_id, address, now)?;
    Ok(())
}

fn reuse_existing_session(
    tx: &Transaction<'_>,
    binding_id: &str,
    instance_id: &str,
    address: &str,
    agent_id: &str,
    now: i64,
) -> std::result::Result<(), CreateGateBindingError> {
    let members = list_session_participants(tx, address)?;
    match members.as_slice() {
        [sole] if sole == agent_id => {}
        _ => return Err(CreateGateBindingError::Conflict),
    }
    if session_occupied_by_other_open_binding(tx, address, binding_id)? {
        return Err(CreateGateBindingError::Conflict);
    }
    fail_step(FAIL_BINDING)?;
    insert_binding_row(tx, binding_id, instance_id, address, now)?;
    Ok(())
}

fn session_occupied_by_other_open_binding(
    tx: &Transaction<'_>,
    session_id: &str,
    current_binding_id: &str,
) -> Result<bool> {
    let by_address: i64 = tx.query_row(
        "SELECT COUNT(*) FROM gate_bindings
         WHERE address = ?1 AND closed_at IS NULL AND binding_id != ?2",
        params![session_id, current_binding_id],
        |r| r.get(0),
    )?;
    if by_address > 0 {
        return Ok(true);
    }
    if let Some(other_id) = session_id.strip_prefix("extgate-") {
        let by_physical: i64 = tx.query_row(
            "SELECT COUNT(*) FROM gate_bindings
             WHERE binding_id = ?1 AND closed_at IS NULL AND binding_id != ?2",
            params![other_id, current_binding_id],
            |r| r.get(0),
        )?;
        if by_physical > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Binding の正本 session。physical `extgate-{binding_id}` があればそれを、
/// 無ければ address と id が一致する再利用 session を返す。どちらも無ければ None。
pub fn canonical_session_id(
    conn: &Connection,
    binding_id: &str,
    address: &str,
) -> Result<Option<String>> {
    let physical = format!("extgate-{binding_id}");
    if get_session(conn, &physical)?.is_some() {
        return Ok(Some(physical));
    }
    if get_session(conn, address)?.is_some() {
        return Ok(Some(address.to_string()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::{get_session, insert_session, upsert_agent, AgentRow, SessionRow};
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
                |r| r.get(0),
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
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        let members: i64 = conn
            .query_row("SELECT COUNT(*) FROM agent_sessions", [], |r| r.get(0))
            .unwrap();
        let bindings: i64 = conn
            .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
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
        let addr: String = conn
            .query_row(
                "SELECT address FROM gate_bindings WHERE binding_id = ?1",
                [binding],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(addr, address);
    }

    fn assert_fail_rolls_back(step: u8) {
        set_binding_tx_fail(step);
        let mut conn = crate::init_memory().unwrap();
        let (instance, _) = seed_agent_and_instance(&conn);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let err = create_gate_binding_in_tx(
            &tx,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &instance,
            "web-a1-c1",
            "Named",
            1,
        );
        assert!(err.is_err(), "step {step}");
        let _ = tx.rollback();
        assert_eq!(counts(&conn), (0, 0, 0), "step {step} left writes");
        set_binding_tx_fail(FAIL_NONE);
    }

    #[test]
    fn fail_session_write_zero() {
        assert_fail_rolls_back(FAIL_SESSION);
    }

    #[test]
    fn fail_membership_write_zero() {
        assert_fail_rolls_back(FAIL_MEMBERSHIP);
    }

    #[test]
    fn fail_binding_write_zero() {
        assert_fail_rolls_back(FAIL_BINDING);
    }

    #[test]
    fn fail_name_write_zero() {
        assert_fail_rolls_back(FAIL_NAME);
    }

    #[test]
    fn fail_commit_write_zero() {
        set_binding_tx_fail(FAIL_COMMIT);
        let mut conn = crate::init_memory().unwrap();
        let (instance, _) = seed_agent_and_instance(&conn);
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        create_gate_binding_in_tx(
            &tx,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &instance,
            "web-a1-c1",
            "web-a1-c1",
            1,
        )
        .unwrap();
        assert!(injected_commit_failure());
        let _ = tx.rollback();
        assert_eq!(counts(&conn), (0, 0, 0));
        set_binding_tx_fail(FAIL_NONE);
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

    #[test]
    fn reuse_existing_session_does_not_insert_second() {
        set_binding_tx_fail(FAIL_NONE);
        let mut conn = crate::init_memory().unwrap();
        let (instance, _) = seed_agent_and_instance(&conn);
        insert_named_session(&conn, "nostr-a1", "a1");
        let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        create_gate_binding_in_tx(&tx, binding, &instance, "nostr-a1", "nostr-a1", 1).unwrap();
        tx.commit().unwrap();
        assert_eq!(counts(&conn), (1, 1, 1));
        assert!(get_session(&conn, "nostr-a1").unwrap().is_some());
        assert!(get_session(&conn, &format!("extgate-{binding}"))
            .unwrap()
            .is_none());
        assert_eq!(
            canonical_session_id(&conn, binding, "nostr-a1")
                .unwrap()
                .as_deref(),
            Some("nostr-a1")
        );
    }

    #[test]
    fn reuse_membership_mismatch_is_conflict() {
        set_binding_tx_fail(FAIL_NONE);
        let mut conn = crate::init_memory().unwrap();
        let (instance, _) = seed_agent_and_instance(&conn);
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "a2".into(),
                name: "a2".into(),
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
        insert_named_session(&conn, "nostr-other", "a2");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let err = create_gate_binding_in_tx(
            &tx,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            &instance,
            "nostr-other",
            "nostr-other",
            1,
        );
        assert!(matches!(err, Err(CreateGateBindingError::Conflict)));
        let _ = tx.rollback();
        assert_eq!(counts(&conn), (1, 1, 0));
    }

    #[test]
    fn reuse_occupation_is_conflict() {
        set_binding_tx_fail(FAIL_NONE);
        let mut conn = crate::init_memory().unwrap();
        let (instance, subject) = seed_agent_and_instance(&conn);
        insert_named_session(&conn, "nostr-a1", "a1");
        let first = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        create_gate_binding_in_tx(&tx, first, &instance, "nostr-a1", "nostr-a1", 1).unwrap();
        tx.commit().unwrap();

        let other = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
        conn.execute(
            "INSERT INTO gate_instances
             (instance_id, kind_id, subject_id, revision, enabled, config_b64, config_digest, created_at, updated_at)
             VALUES (?1, 'nostr', ?2, 1, 1, 'e30=', '0000000000000000000000000000000000000000000000000000000000000000', 1, 1)",
            rusqlite::params![other, subject],
        )
        .unwrap();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let err = create_gate_binding_in_tx(
            &tx,
            "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
            other,
            "nostr-a1",
            "nostr-a1",
            2,
        );
        assert!(matches!(err, Err(CreateGateBindingError::Conflict)));
        let _ = tx.rollback();
        let bindings: i64 = conn
            .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bindings, 1);
    }

    #[test]
    fn traditional_create_still_uses_physical_session() {
        set_binding_tx_fail(FAIL_NONE);
        let mut conn = crate::init_memory().unwrap();
        let (instance, _) = seed_agent_and_instance(&conn);
        let binding = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        create_gate_binding_in_tx(&tx, binding, &instance, "chan-1", "chan-1", 1).unwrap();
        tx.commit().unwrap();
        let physical = format!("extgate-{binding}");
        assert!(get_session(&conn, &physical).unwrap().is_some());
        assert_eq!(
            canonical_session_id(&conn, binding, "chan-1")
                .unwrap()
                .as_deref(),
            Some(physical.as_str())
        );
    }
}
