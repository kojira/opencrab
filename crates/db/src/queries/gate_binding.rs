//! Binding 永続化の唯一の入口。V3 Binding PUT と Web 会話作成が同じ TX 部品を使う。

use std::cell::Cell;

use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Transaction};

use super::{insert_agent_session_in_tx, insert_session_in_tx};

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
/// （未指定時は address）。physical session id は `extgate-{binding_id}`。
pub fn create_gate_binding_in_tx(
    tx: &Transaction<'_>,
    binding_id: &str,
    instance_id: &str,
    address: &str,
    session_theme: &str,
    now: i64,
) -> Result<()> {
    let session_id = format!("extgate-{binding_id}");
    let now_rfc = rfc3339_from_nanos(now)?;
    let agent_id = agent_id_for_instance(tx, instance_id)?;

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

#[cfg(test)]
mod tests {
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
        create_gate_binding_in_tx(&tx, binding, &instance, address, "My Name", 1_700_000_000_000_000_000)
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
}
