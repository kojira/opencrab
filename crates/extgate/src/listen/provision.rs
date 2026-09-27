use std::sync::Arc;

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::json;

use crate::error::{ErrorCode, GateError};
use crate::ids::{config_digest_from_b64, now_nanos, session_id_for_binding};
use crate::protocol::{err_frame, write_json, Provision};
use crate::registry::ExtgateState;

/// Applies the complete declarative inventory in the one pre-hello transaction.
pub async fn handle_provision(
    state: &Arc<ExtgateState>,
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    request: Provision,
) -> Result<(), ()> {
    let result = persist(state, &request);
    let frame = match result {
        Ok(snapshot) => snapshot,
        Err(error) => err_frame(&request.id, error.code, None),
    };
    write_json(writer, &frame).await.map_err(|_| ())
}

pub(crate) fn persist(
    state: &ExtgateState,
    request: &Provision,
) -> Result<serde_json::Value, GateError> {
    // Hold the same registry lock used by hello through the immediate transaction. This makes a
    // live-instance change fail before any generic state can be written.
    let registry = state.lock_registry()?;
    if registry.is_live(&request.instance_id) {
        return Err(GateError::new(ErrorCode::InstanceActive));
    }
    let mut conn = state.db.lock().map_err(|_| GateError::store())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| GateError::store())?;
    let agent_id: Option<String> = tx
        .query_row(
            "SELECT agent_id FROM agents WHERE subject_id=?1",
            [request.subject_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| GateError::store())?;
    let Some(agent_id) = agent_id else {
        return Err(GateError::new(ErrorCode::SubjectUnknown));
    };
    let digest = config_digest_from_b64(&request.config_b64)?;
    let existing: Option<(String, i64, i64, String, i64, String, Option<i64>)> = tx
        .query_row(
            "SELECT kind_id, subject_id, revision, config_b64, enabled, binding_authority, deleted_at
             FROM gate_instances WHERE instance_id=?1",
            [&request.instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .optional()
        .map_err(|_| GateError::store())?;
    let now = now_nanos();
    match existing {
        None => {
            let grant = request
                .subject_grant
                .as_deref()
                .ok_or_else(|| GateError::new(ErrorCode::InstanceConflict))?;
            tx.execute(
                "INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at,binding_authority)
                 VALUES(?1,?2,?3,1,?4,?5,?6,?7,?7,'declarative')",
                params![request.instance_id, request.kind_id, request.subject_id, i64::from(request.enabled), request.config_b64, digest, now],
            ).map_err(|_| GateError::store())?;
            opencrab_db::queries::consume_subject_association_grant_in_tx(
                &tx,
                grant,
                &agent_id,
                request.subject_id,
                &request.instance_id,
                now,
            )
            .map_err(|_| GateError::new(ErrorCode::InstanceConflict))?;
        }
        Some((kind, subject, revision, config_b64, enabled, authority, deleted_at)) => {
            if deleted_at.is_some() || kind != request.kind_id || subject != request.subject_id {
                return Err(GateError::new(ErrorCode::InstanceConflict));
            }
            if authority == "runtime" {
                if !request.adopt_existing {
                    return Err(GateError::new(ErrorCode::InstanceConflict));
                }
                tx.execute(
                    "UPDATE gate_instances SET binding_authority='declarative' WHERE instance_id=?1",
                    [&request.instance_id],
                ).map_err(|_| GateError::store())?;
            }
            if config_b64 != request.config_b64 || enabled != i64::from(request.enabled) {
                let next = revision.checked_add(1).ok_or_else(GateError::store)?;
                tx.execute(
                    "UPDATE gate_instances SET revision=?2,enabled=?3,config_b64=?4,config_digest=?5,updated_at=?6,operation_declaration_digest=NULL WHERE instance_id=?1",
                    params![request.instance_id, next, i64::from(request.enabled), request.config_b64, digest, now],
                ).map_err(|_| GateError::store())?;
            }
        }
    }

    for binding in &request.bindings {
        let address_owner: Option<String> = tx
            .query_row(
                "SELECT instance_id FROM gate_bindings WHERE address=?1 AND closed_at IS NULL",
                [&binding.address],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| GateError::store())?;
        if address_owner
            .as_deref()
            .is_some_and(|owner| owner != request.instance_id)
        {
            return Err(GateError::new(ErrorCode::AddressInUse));
        }
        match opencrab_db::queries::CoreBindingService::create_in_tx(
            &tx,
            &opencrab_db::queries::CoreBindingRequest {
                binding_id: &binding.binding_id,
                instance_id: &request.instance_id,
                address: &binding.address,
                session_id: &session_id_for_binding(&binding.binding_id),
                session_title: "gateway session",
                now,
            },
        ) {
            Ok(_) => {}
            Err(opencrab_db::queries::CreateGateBindingError::AddressInUse) => {
                return Err(GateError::new(ErrorCode::AddressInUse))
            }
            Err(opencrab_db::queries::CreateGateBindingError::Unknown) => {
                return Err(GateError::new(ErrorCode::InstanceConflict))
            }
            Err(
                opencrab_db::queries::CreateGateBindingError::Conflict
                | opencrab_db::queries::CreateGateBindingError::Closed,
            ) => return Err(GateError::new(ErrorCode::BindingConflict)),
            Err(opencrab_db::queries::CreateGateBindingError::Store(_)) => {
                return Err(GateError::store())
            }
        }
    }
    if request.bindings.is_empty() {
        tx.execute(
            "UPDATE gate_bindings SET closed_at=?2 WHERE instance_id=?1 AND closed_at IS NULL",
            params![request.instance_id, now],
        )
        .map_err(|_| GateError::store())?;
    } else {
        let ids = request
            .bindings
            .iter()
            .map(|binding| format!("'{}'", binding.binding_id))
            .collect::<Vec<_>>()
            .join(",");
        tx.execute_batch(&format!("UPDATE gate_bindings SET closed_at={now} WHERE instance_id='{}' AND closed_at IS NULL AND binding_id NOT IN ({ids});", request.instance_id)).map_err(|_| GateError::store())?;
    }
    let (revision, config_digest, enabled): (i64, String, i64) = tx
        .query_row(
            "SELECT revision,config_digest,enabled FROM gate_instances WHERE instance_id=?1",
            [&request.instance_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| GateError::store())?;
    tx.commit().map_err(|_| GateError::store())?;
    drop(registry);
    Ok(
        json!({"m":"provisioned","id":request.id,"instance_id":request.instance_id,"revision":revision,"config_digest":config_digest,"enabled":enabled == 1,"bindings":request.bindings.iter().map(|binding| json!({"binding_id":binding.binding_id,"address":binding.address})).collect::<Vec<_>>() }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{issue_subject_association_grant, upsert_agent, AgentRow};

    fn request(grant: Option<String>) -> Provision {
        Provision {
            id: "p1".into(),
            instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
            kind_id: "opaque-kind".into(),
            subject_id: 1,
            subject_grant: grant,
            adopt_existing: false,
            enabled: true,
            config_b64: "eyJkZWxpdmVyeV9tb2RlIjoidG9vbF9kcml2ZW4ifQ==".into(),
            bindings: vec![crate::protocol::ProvisionBinding {
                binding_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into(),
                address: "opaque-address".into(),
            }],
        }
    }

    #[test]
    fn declarative_provision_is_atomic_idempotent_and_owns_bindings() {
        let db = opencrab_db::Db::memory().unwrap();
        let mut conn = db.lock().unwrap();
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "agent".into(),
                name: "agent".into(),
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
                "SELECT subject_id FROM agents WHERE agent_id='agent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let grant =
            issue_subject_association_grant(&mut conn, "agent", subject, i64::MAX, now_nanos())
                .unwrap();
        drop(conn);
        let state = ExtgateState::new_protected(db.clone());
        let first = persist(&state, &request(Some(grant))).unwrap();
        let second = persist(&state, &request(None)).unwrap();
        assert_eq!(first["revision"], second["revision"]);
        let conn = db.lock().unwrap();
        assert_eq!(
            conn.query_row("SELECT binding_authority FROM gate_instances", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "declarative"
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM gate_bindings WHERE closed_at IS NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(conn.query_row("SELECT count(*) FROM sessions WHERE id='extgate-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    }
}
