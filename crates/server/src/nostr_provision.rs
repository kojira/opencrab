//! Nostr instance / binding の core 側敷設。address は既存 session_id（V3.5 reuse）。

use anyhow::{bail, Context, Result};
use opencrab_db::queries::{
    create_gate_binding_in_tx, get_session, CreateGateBindingError, SessionWatchRow,
};
use opencrab_nostr::{
    instance_config_bytes, nostr_instance_id, plan_session_bindings, NostrConfig, SessionBindingPlan,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

/// session ごと 1 binding。session 不在・membership 不一致は fail-loud。
pub fn provision_nostr_gate(
    conn: &mut Connection,
    agent_id: &str,
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
    now: i64,
) -> Result<Vec<SessionBindingPlan>> {
    let plans = plan_session_bindings(agent_id, watches)?;
    let instance_id = nostr_instance_id(agent_id);
    let config_bytes = instance_config_bytes(self_pubkey, config, watches)?;
    let config_b64 = opencrab_extgate::encode_config_b64(&config_bytes);
    let digest = opencrab_extgate::config_digest(&config_bytes);

    let subject_id: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id = ?1",
            params![agent_id],
            |r| r.get(0),
        )
        .with_context(|| format!("agent {agent_id} の subject_id が無い"))?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for plan in &plans {
        if get_session(&tx, &plan.address)?.is_none() {
            bail!(
                "session {} が無い（V3 binding は既存 session を再利用する）",
                plan.address
            );
        }
    }

    let existing = tx
        .query_row(
            "SELECT kind_id, subject_id, deleted_at FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    match existing {
        Some((_, _, Some(_))) => bail!("nostr instance {instance_id} は削除済み"),
        Some((kind, subject, None)) if kind != "nostr" || subject != subject_id => {
            bail!("nostr instance {instance_id} が別 kind/subject で存在する")
        }
        Some(_) => {
            tx.execute(
                "UPDATE gate_instances
                 SET config_b64 = ?2, config_digest = ?3, updated_at = ?4
                 WHERE instance_id = ?1",
                params![instance_id, config_b64, digest, now],
            )?;
        }
        None => {
            tx.execute(
                "INSERT INTO gate_instances (
                    instance_id, kind_id, subject_id, revision, enabled,
                    config_b64, config_digest, created_at, updated_at, deleted_at
                 ) VALUES (?1, 'nostr', ?2, 1, 1, ?3, ?4, ?5, ?5, NULL)",
                params![instance_id, subject_id, config_b64, digest, now],
            )?;
        }
    }

    for plan in &plans {
        let already: Option<String> = tx
            .query_row(
                "SELECT address FROM gate_bindings WHERE binding_id = ?1",
                params![plan.binding_id],
                |r| r.get(0),
            )
            .optional()?;
        match already {
            Some(addr) if addr == plan.address => {}
            Some(addr) => bail!(
                "binding {} は別 address {} で存在する",
                plan.binding_id,
                addr
            ),
            None => match create_gate_binding_in_tx(
                &tx,
                &plan.binding_id,
                &instance_id,
                &plan.address,
                &plan.address,
                now,
            ) {
                Ok(()) => {}
                Err(CreateGateBindingError::Conflict) => {
                    bail!(
                        "binding address {} の membership / 占有が一致しない",
                        plan.address
                    )
                }
                Err(CreateGateBindingError::Store(e)) => return Err(e),
            },
        }
    }
    tx.commit()?;
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{
        insert_agent_session_in_tx, insert_session_in_tx, upsert_agent, AgentRow,
    };
    use opencrab_nostr::{nostr_binding_id, nostr_session_id};

    fn seed_agent(conn: &Connection) {
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
    }

    #[test]
    fn provision_reuses_existing_session() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let sid = nostr_session_id("a1");
        let tx = conn.transaction().unwrap();
        insert_session_in_tx(&tx, &sid, &sid, "2026-01-01T00:00:00Z").unwrap();
        insert_agent_session_in_tx(&tx, "a1", &sid).unwrap();
        tx.commit().unwrap();

        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: opencrab_nostr::NostrFilter::default(),
        };
        let plans = provision_nostr_gate(&mut conn, "a1", &"aa".repeat(32), &cfg, &[], 1).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].address, sid);
        assert_eq!(plans[0].binding_id, nostr_binding_id("a1", &sid));
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        let kind: String = conn
            .query_row(
                "SELECT kind_id FROM gate_instances WHERE instance_id = ?1",
                params![nostr_instance_id("a1")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "nostr");
    }

    #[test]
    fn missing_session_is_fail_loud() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: opencrab_nostr::NostrFilter::default(),
        };
        let err = provision_nostr_gate(&mut conn, "a1", &"aa".repeat(32), &cfg, &[], 1).unwrap_err();
        assert!(err.to_string().contains("session"));
    }
}
