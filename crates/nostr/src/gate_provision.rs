//! Nostr instance / binding の core 側敷設。address は既存 session_id（V3.5 reuse）。

use crate::{
    instance_config_bytes_with_access, nostr_instance_id, plan_session_bindings, AllowSources,
    NostrConfig, SessionBindingPlan,
};
use anyhow::{bail, Context, Result};
use opencrab_db::queries::{
    create_gate_binding_in_tx, get_session, revise_gate_instance_in_tx, CreateGateBindingError,
    ReviseGateInstanceError, SessionWatchRow,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoppedRevision {
    pub expected_revision: u64,
    pub updated_at: i64,
}

fn encode_config_b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn config_digest(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// Computes the exact core-facing instance config without mutating core storage.
pub fn desired_nostr_config_b64(
    conn: &Connection,
    agent_id: &str,
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
    access: &AllowSources,
) -> Result<String> {
    Ok(desired_nostr_config(conn, agent_id, self_pubkey, config, watches, access)?.0)
}

fn desired_nostr_config(
    conn: &Connection,
    agent_id: &str,
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
    access: &AllowSources,
) -> Result<(String, String)> {
    let name = agent_name(conn, agent_id)?;
    let bytes = instance_config_bytes_with_access(self_pubkey, &name, config, watches, access)?;
    Ok((encode_config_b64(&bytes), config_digest(&bytes)))
}

pub fn build_allow_sources(
    followees: impl IntoIterator<Item = String>,
    keys: &crate::NostrGateAllowKeys,
) -> crate::AllowSources {
    fn normalized(values: &[String]) -> std::collections::HashSet<String> {
        values
            .iter()
            .filter_map(|value| crate::normalize_pubkey(value))
            .collect()
    }
    crate::AllowSources {
        followees: followees
            .into_iter()
            .filter_map(|value| crate::normalize_pubkey(&value))
            .collect(),
        owner: normalized(&keys.owner),
        co_agents: normalized(&keys.co_agents),
        co_agent_identities: keys
            .co_agent_identities
            .iter()
            .filter_map(|(key, agent_id)| {
                crate::normalize_pubkey(key).map(|key| (key, agent_id.clone()))
            })
            .collect(),
        trusted_users: normalized(&keys.trusted_users),
    }
}

/// Provisioning が完了した enabled Nostr instance を、外部 gateway の placement へ投影する。
/// default session の open binding が無い・重複する instance は fail-loud にする。
pub fn load_nostr_placement_plan(conn: &Connection, agent_id: &str) -> Result<NostrPlacementPlan> {
    find_nostr_placement_plan(conn, agent_id)?
        .with_context(|| format!("gateway placement not found for {agent_id}"))
}

/// Loads an existing placement without turning absence into an error during reconciliation.
pub fn find_nostr_placement_plan(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<NostrPlacementPlan>> {
    let instance_id = nostr_instance_id(agent_id);
    let address = crate::nostr_session_id(agent_id);
    let row = conn
        .query_row(
            "SELECT gi.revision, gb.address, gi.config_b64
             FROM gate_instances gi
             JOIN gate_bindings gb ON gb.instance_id = gi.instance_id
             WHERE gi.instance_id = ?1 AND gi.enabled = 1 AND gi.deleted_at IS NULL
               AND gb.address = ?2 AND gb.closed_at IS NULL",
            params![instance_id, address],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|row| {
        Ok(NostrPlacementPlan {
            agent_id: agent_id.to_string(),
            instance_id,
            revision: u64::try_from(row.0).context("gateway instance revision")?,
            address: row.1,
            config_b64: row.2,
        })
    })
    .transpose()
}

/// session ごと 1 binding。session 不在・membership 不一致は fail-loud。
pub fn provision_nostr_gate(
    conn: &mut Connection,
    agent_id: &str,
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
    access: &AllowSources,
    now: i64,
) -> Result<Vec<SessionBindingPlan>> {
    let plans = plan_session_bindings(agent_id, watches)?;
    let instance_id = nostr_instance_id(agent_id);
    let (config_b64, digest) =
        desired_nostr_config(conn, agent_id, self_pubkey, config, watches, access)?;
    let subject_id = agent_subject_id(conn, agent_id)?;

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_sessions(&tx, &plans)?;
    let existing = tx
        .query_row(
            "SELECT kind_id, subject_id, deleted_at, config_b64, config_digest
             FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    match existing {
        Some((_, _, Some(_), _, _)) => bail!("nostr instance {instance_id} は削除済み"),
        Some((kind, subject, None, _, _)) if kind != "nostr" || subject != subject_id => {
            bail!("nostr instance {instance_id} が別 kind/subject で存在する")
        }
        Some((_, _, None, stored_config, stored_digest))
            if stored_config == config_b64 && stored_digest == digest => {}
        Some(_) => bail!("nostr instance {instance_id} の変更には停止後 revision が必要"),
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
    ensure_bindings(&tx, &instance_id, &plans, now)?;
    tx.commit()?;
    Ok(plans)
}

/// Applies a stopped Nostr instance change through the shared generic revision operation.
pub fn revise_nostr_gate(
    conn: &mut Connection,
    agent_id: &str,
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
    access: &AllowSources,
    revision: StoppedRevision,
) -> Result<u64> {
    let plans = plan_session_bindings(agent_id, watches)?;
    let instance_id = nostr_instance_id(agent_id);
    let (config_b64, digest) =
        desired_nostr_config(conn, agent_id, self_pubkey, config, watches, access)?;
    let subject_id = agent_subject_id(conn, agent_id)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_sessions(&tx, &plans)?;
    let existing = tx
        .query_row(
            "SELECT kind_id, subject_id, deleted_at
             FROM gate_instances WHERE instance_id = ?1",
            params![instance_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    match existing {
        Some((_, _, Some(_))) => bail!("nostr instance {instance_id} は削除済み"),
        Some((kind, subject, None)) if kind != "nostr" || subject != subject_id => {
            bail!("nostr instance {instance_id} が別 kind/subject で存在する")
        }
        Some(_) => {}
        None => bail!("nostr instance {instance_id} が無いので revision を上げられない"),
    }
    ensure_bindings(&tx, &instance_id, &plans, revision.updated_at)?;
    let revision = revise_gate_instance_in_tx(
        &tx,
        &instance_id,
        revision.expected_revision,
        true,
        &config_b64,
        &digest,
        revision.updated_at,
    )
    .map_err(|error| match error {
        ReviseGateInstanceError::Unknown => anyhow::anyhow!("nostr instance {instance_id} が無い"),
        ReviseGateInstanceError::RevisionConflict => {
            anyhow::anyhow!("nostr instance {instance_id} の revision が競合した")
        }
        ReviseGateInstanceError::Store(error) => error,
    })?;
    tx.commit()?;
    Ok(revision)
}

fn validate_sessions(tx: &rusqlite::Transaction<'_>, plans: &[SessionBindingPlan]) -> Result<()> {
    for plan in plans {
        if get_session(tx, &plan.address)?.is_none() {
            bail!(
                "session {} が無い（V3 binding は既存 session を再利用する）",
                plan.address
            );
        }
    }
    Ok(())
}

fn ensure_bindings(
    tx: &rusqlite::Transaction<'_>,
    instance_id: &str,
    plans: &[SessionBindingPlan],
    now: i64,
) -> Result<()> {
    for plan in plans {
        let already: Option<String> = tx
            .query_row(
                "SELECT address FROM gate_bindings WHERE binding_id = ?1",
                params![plan.binding_id],
                |row| row.get(0),
            )
            .optional()?;
        match already {
            Some(address) if address == plan.address => {}
            Some(address) => bail!(
                "binding {} は別 address {} で存在する",
                plan.binding_id,
                address
            ),
            None => match create_gate_binding_in_tx(
                tx,
                &plan.binding_id,
                instance_id,
                &plan.address,
                &plan.address,
                now,
            ) {
                Ok(()) => {}
                Err(CreateGateBindingError::Conflict) => bail!(
                    "binding address {} の membership / 占有が一致しない",
                    plan.address
                ),
                Err(CreateGateBindingError::Store(error)) => return Err(error),
            },
        }
    }
    Ok(())
}

fn agent_subject_id(conn: &Connection, agent_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT subject_id FROM agents WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )
    .with_context(|| format!("agent {agent_id} の subject_id が無い"))
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
    use crate::{nostr_binding_id, nostr_session_id};
    use opencrab_db::queries::{
        insert_agent_session_in_tx, insert_session_in_tx, upsert_agent, AgentRow,
    };

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
            filter: crate::NostrFilter::default(),
        };
        let plans = provision_nostr_gate(
            &mut conn,
            "a1",
            &"aa".repeat(32),
            &cfg,
            &[],
            &AllowSources::default(),
            1,
        )
        .unwrap();
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
            filter: crate::NostrFilter::default(),
        };
        let err = provision_nostr_gate(
            &mut conn,
            "a1",
            &"aa".repeat(32),
            &cfg,
            &[],
            &AllowSources::default(),
            1,
        )
        .unwrap_err();
        assert!(err.to_string().contains("session"));
    }

    #[test]
    fn revise_bumps_revision() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: crate::NostrFilter::default(),
        };
        let sid = nostr_session_id("a1");
        let tx = conn.transaction().unwrap();
        insert_session_in_tx(&tx, &sid, &sid, "2026-01-01T00:00:00Z").unwrap();
        insert_agent_session_in_tx(&tx, "a1", &sid).unwrap();
        tx.commit().unwrap();
        provision_nostr_gate(
            &mut conn,
            "a1",
            &"aa".repeat(32),
            &cfg,
            &[],
            &AllowSources::default(),
            1,
        )
        .unwrap();
        conn.execute(
            "UPDATE gate_instances SET operation_declaration_digest = 'old' WHERE instance_id = ?1",
            params![nostr_instance_id("a1")],
        )
        .unwrap();
        let rev = revise_nostr_gate(
            &mut conn,
            "a1",
            &"bb".repeat(32),
            &cfg,
            &[],
            &AllowSources::default(),
            StoppedRevision {
                expected_revision: 1,
                updated_at: 2,
            },
        )
        .unwrap();
        assert_eq!(rev, 2);
        let stored: (i64, Option<String>) = conn
            .query_row(
                "SELECT revision, operation_declaration_digest
                 FROM gate_instances WHERE instance_id = ?1",
                params![nostr_instance_id("a1")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (2, None));
    }

    #[test]
    fn instance_config_includes_agent_name() {
        let mut conn = opencrab_db::init_memory().unwrap();
        seed_agent(&conn);
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: crate::NostrFilter::default(),
        };
        let sid = nostr_session_id("a1");
        let tx = conn.transaction().unwrap();
        insert_session_in_tx(&tx, &sid, &sid, "2026-01-01T00:00:00Z").unwrap();
        insert_agent_session_in_tx(&tx, "a1", &sid).unwrap();
        tx.commit().unwrap();
        provision_nostr_gate(
            &mut conn,
            "a1",
            &"aa".repeat(32),
            &cfg,
            &[],
            &AllowSources::default(),
            1,
        )
        .unwrap();
        let config_b64: String = conn
            .query_row(
                "SELECT config_b64 FROM gate_instances WHERE instance_id = ?1",
                params![nostr_instance_id("a1")],
                |r| r.get(0),
            )
            .unwrap();
        let bytes = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(&config_b64)
                .unwrap()
        };
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
            filter: crate::NostrFilter::default(),
        };
        let err = provision_nostr_gate(
            &mut conn,
            "a1",
            &"aa".repeat(32),
            &cfg,
            &[],
            &AllowSources::default(),
            1,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("agents.name"),
            "empty name must fail-loud: {err}"
        );
    }
}
