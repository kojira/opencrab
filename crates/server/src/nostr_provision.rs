//! Nostr instance / binding の core 側敷設。address は既存 session_id（V3.5 reuse）。

use anyhow::{bail, Context, Result};
use opencrab_db::queries::{
    create_gate_binding_in_tx, get_session, CreateGateBindingError, SessionWatchRow,
};
use opencrab_nostr::{
    instance_config_bytes, nostr_instance_id, plan_session_bindings, NostrConfig,
    SessionBindingPlan,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NostrPlacementPlan {
    pub agent_id: String,
    pub instance_id: String,
    pub revision: u64,
    pub address: String,
    pub config_b64: String,
}

/// Provisioning が完了した enabled Nostr instance を、外部 gateway の placement へ投影する。
/// default session の open binding が無い・重複する instance は fail-loud にする。
pub fn load_nostr_placement_plans(conn: &Connection) -> Result<Vec<NostrPlacementPlan>> {
    let mut stmt = conn.prepare(
        "SELECT nc.agent_id, gi.instance_id, gi.revision, gb.address, gi.config_b64
         FROM agent_nostr_config nc
         JOIN agents a ON a.agent_id = nc.agent_id
         JOIN gate_instances gi
           ON gi.subject_id = a.subject_id
          AND gi.kind_id = 'nostr'
          AND gi.enabled = 1
          AND gi.deleted_at IS NULL
         JOIN gate_bindings gb
           ON gb.instance_id = gi.instance_id
          AND gb.closed_at IS NULL
          AND gb.address = 'nostr-' || nc.agent_id
         WHERE nc.enabled = 1
         ORDER BY nc.agent_id",
    )?;
    let rows = stmt.query_map([], |row| {
        let revision = row.get::<_, i64>(2)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            revision,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;

    let mut plans = Vec::new();
    for row in rows {
        let (agent_id, instance_id, revision, address, config_b64) = row?;
        let expected_instance_id = nostr_instance_id(&agent_id);
        if instance_id != expected_instance_id {
            bail!(
                "agent {agent_id} の Nostr instance が不正: expected={expected_instance_id}, actual={instance_id}"
            );
        }
        plans.push(NostrPlacementPlan {
            agent_id,
            instance_id,
            revision: u64::try_from(revision).context("nostr instance revision")?,
            address,
            config_b64,
        });
    }
    let enabled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM agent_nostr_config WHERE enabled = 1",
        [],
        |row| row.get(0),
    )?;
    if i64::try_from(plans.len()).context("nostr placement count")? != enabled {
        bail!(
            "enabled Nostr agent {enabled} 件に対して有効な V3 placement は {} 件（binding/instance 欠落）",
            plans.len()
        );
    }
    Ok(plans)
}

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
    let name = agent_name(conn, agent_id)?;
    let config_bytes = instance_config_bytes(self_pubkey, &name, config, watches)?;
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

/// 停止後の identity 切替用。config を書き revision を +1 する。
pub fn revise_nostr_gate(
    conn: &mut Connection,
    agent_id: &str,
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
    now: i64,
) -> Result<u64> {
    update_nostr_instance(conn, agent_id, self_pubkey, config, watches, now)
}

fn update_nostr_instance(
    conn: &mut Connection,
    agent_id: &str,
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
    now: i64,
) -> Result<u64> {
    let instance_id = nostr_instance_id(agent_id);
    let name = agent_name(conn, agent_id)?;
    let config_bytes = instance_config_bytes(self_pubkey, &name, config, watches)?;
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
    let existing = tx
        .query_row(
            "SELECT kind_id, subject_id, deleted_at, revision FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let revision = match existing {
        Some((_, _, Some(_), _)) => bail!("nostr instance {instance_id} は削除済み"),
        Some((kind, subject, None, _)) if kind != "nostr" || subject != subject_id => {
            bail!("nostr instance {instance_id} が別 kind/subject で存在する")
        }
        Some((_, _, None, rev)) => {
            let new_rev = rev + 1;
            tx.execute(
                "UPDATE gate_instances
                 SET config_b64 = ?2, config_digest = ?3, revision = ?4, updated_at = ?5
                 WHERE instance_id = ?1",
                params![instance_id, config_b64, digest, new_rev, now],
            )?;
            u64::try_from(new_rev).context("revision")?
        }
        None => bail!("nostr instance {instance_id} が無いので revision を上げられない"),
    };
    tx.commit()?;
    Ok(revision)
}

fn agent_name(conn: &Connection, agent_id: &str) -> Result<String> {
    conn.query_row(
        "SELECT name FROM agents WHERE agent_id = ?1",
        params![agent_id],
        |r| r.get(0),
    )
    .with_context(|| format!("agent {agent_id} の name が無い"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{
        insert_agent_session_in_tx, insert_session_in_tx, upsert_agent, upsert_agent_nostr_config,
        AgentNostrConfigRow, AgentRow,
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
    fn enabled_agent_projects_to_external_gateway_placement() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let sid = nostr_session_id("a1");
        let tx = conn.transaction().unwrap();
        insert_session_in_tx(&tx, &sid, &sid, "2026-01-01T00:00:00Z").unwrap();
        insert_agent_session_in_tx(&tx, "a1", &sid).unwrap();
        tx.commit().unwrap();
        upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "a1".into(),
                secret_key: "encrypted-secret-placeholder".into(),
                relays_json: r#"["wss://yabu.me"]"#.into(),
                filter_json: "{}".into(),
                enabled: true,
            },
        )
        .unwrap();
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: opencrab_nostr::NostrFilter::default(),
        };
        provision_nostr_gate(&mut conn, "a1", &"aa".repeat(32), &cfg, &[], 1).unwrap();

        let placements = load_nostr_placement_plans(&conn).unwrap();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].agent_id, "a1");
        assert_eq!(placements[0].instance_id, nostr_instance_id("a1"));
        assert_eq!(placements[0].revision, 1);
        assert_eq!(placements[0].address, sid);
        assert!(!placements[0].config_b64.is_empty());
    }

    #[test]
    fn enabled_agent_without_v3_binding_is_fail_loud() {
        let conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        upsert_agent_nostr_config(
            &conn,
            &AgentNostrConfigRow {
                agent_id: "a1".into(),
                secret_key: "encrypted-secret-placeholder".into(),
                relays_json: "[]".into(),
                filter_json: "{}".into(),
                enabled: true,
            },
        )
        .unwrap();
        let error = load_nostr_placement_plans(&conn).unwrap_err();
        assert!(error.to_string().contains("placement"));
    }

    #[test]
    fn missing_session_is_fail_loud() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: opencrab_nostr::NostrFilter::default(),
        };
        let err =
            provision_nostr_gate(&mut conn, "a1", &"aa".repeat(32), &cfg, &[], 1).unwrap_err();
        assert!(err.to_string().contains("session"));
    }

    #[test]
    fn revise_bumps_revision() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: opencrab_nostr::NostrFilter::default(),
        };
        let sid = nostr_session_id("a1");
        let tx = conn.transaction().unwrap();
        insert_session_in_tx(&tx, &sid, &sid, "2026-01-01T00:00:00Z").unwrap();
        insert_agent_session_in_tx(&tx, "a1", &sid).unwrap();
        tx.commit().unwrap();
        provision_nostr_gate(&mut conn, "a1", &"aa".repeat(32), &cfg, &[], 1).unwrap();
        let rev = revise_nostr_gate(&mut conn, "a1", &"bb".repeat(32), &cfg, &[], 2).unwrap();
        assert_eq!(rev, 2);
        let stored: i64 = conn
            .query_row(
                "SELECT revision FROM gate_instances WHERE instance_id = ?1",
                params![nostr_instance_id("a1")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, 2);
    }

    #[test]
    fn instance_config_includes_agent_name() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: opencrab_nostr::NostrFilter::default(),
        };
        let sid = nostr_session_id("a1");
        let tx = conn.transaction().unwrap();
        insert_session_in_tx(&tx, &sid, &sid, "2026-01-01T00:00:00Z").unwrap();
        insert_agent_session_in_tx(&tx, "a1", &sid).unwrap();
        tx.commit().unwrap();
        provision_nostr_gate(&mut conn, "a1", &"aa".repeat(32), &cfg, &[], 1).unwrap();
        let config_b64: String = conn
            .query_row(
                "SELECT config_b64 FROM gate_instances WHERE instance_id = ?1",
                params![nostr_instance_id("a1")],
                |r| r.get(0),
            )
            .unwrap();
        let bytes = opencrab_extgate::ids::decode_config_b64(&config_b64).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["name"], "a1");
    }

    #[test]
    fn empty_agent_name_is_fail_loud() {
        let mut conn = opencrab_db::init_memory().unwrap();
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "a1".into(),
                name: "   ".into(),
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
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: opencrab_nostr::NostrFilter::default(),
        };
        let err =
            provision_nostr_gate(&mut conn, "a1", &"aa".repeat(32), &cfg, &[], 1).unwrap_err();
        assert!(
            err.to_string().contains("agents.name"),
            "empty name must fail-loud: {err}"
        );
    }
}
