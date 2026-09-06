use opencrab_db::queries::{get_agent_discord_config, get_agent_nostr_owner_pubkey};
use rusqlite::{params, Connection, Transaction};

use crate::delivery_mode::DeliveryMode;
use crate::error::{ErrorCode, GateError};
use crate::ids::decode_config_b64;
use crate::registry::ExtgateState;

use super::SaidOutcome;

#[derive(Clone)]
pub(super) struct OriginRow {
    pub(super) instance_id: String,
    pub(super) kind_id: String,
    pub(super) address: String,
    pub(super) agent_id: String,
    pub(super) owner_id: String,
    pub(super) delivery_mode: DeliveryMode,
}

pub(super) fn binding_said_error(
    state: &ExtgateState,
    instance_id: &str,
    binding_id: &str,
) -> Result<SaidOutcome, GateError> {
    let conn = state.db.lock().map_err(|_| GateError::store())?;
    match binding_status(&conn, instance_id, binding_id)? {
        BindingStatus::Absent => Err(GateError::new(ErrorCode::BindingUnknown)),
        BindingStatus::Closed => Err(GateError::new(ErrorCode::BindingClosed)),
        BindingStatus::OtherInstance | BindingStatus::Open => {
            Err(GateError::new(ErrorCode::InstanceNotReady))
        }
    }
}

enum BindingStatus {
    Absent,
    Closed,
    OtherInstance,
    Open,
}

fn binding_status(
    conn: &Connection,
    instance_id: &str,
    binding_id: &str,
) -> Result<BindingStatus, GateError> {
    let row = conn.query_row(
        "SELECT instance_id, closed_at FROM gate_bindings WHERE binding_id = ?1",
        params![binding_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
    );
    match row {
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(BindingStatus::Absent),
        Err(_) => Err(GateError::store()),
        Ok((inst, closed)) => {
            if inst != instance_id {
                Ok(BindingStatus::OtherInstance)
            } else if closed.is_some() {
                Ok(BindingStatus::Closed)
            } else {
                Ok(BindingStatus::Open)
            }
        }
    }
}

pub(super) fn load_origin_row(
    tx: &Transaction<'_>,
    instance_id: &str,
    binding_id: &str,
) -> Result<Option<OriginRow>, GateError> {
    let result = tx.query_row(
        "SELECT b.instance_id, i.kind_id, b.address, a.agent_id, b.closed_at, i.config_b64
         FROM gate_bindings b
         JOIN gate_instances i ON i.instance_id = b.instance_id
         JOIN agents a ON a.subject_id = i.subject_id
         WHERE b.binding_id = ?1 AND i.deleted_at IS NULL",
        params![binding_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, String>(5)?,
            ))
        },
    );
    match result {
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(_) => Err(GateError::store()),
        Ok((inst, kind_id, address, agent_id, closed, config_b64)) => {
            if inst != instance_id || closed.is_some() {
                return Ok(None);
            }
            let owner_id = if kind_id == "nostr" {
                get_agent_nostr_owner_pubkey(tx, &agent_id).map_err(|_| GateError::store())?
            } else {
                get_agent_discord_config(tx, &agent_id)
                    .ok()
                    .flatten()
                    .map(|c| c.owner_discord_id)
                    .unwrap_or_default()
            };
            let config_bytes = decode_config_b64(&config_b64)?;
            let delivery_mode =
                crate::delivery_mode::delivery_mode_from_config_bytes(&config_bytes)
                    .map_err(|_| GateError::new(ErrorCode::BadRequest))?;
            Ok(Some(OriginRow {
                instance_id: inst,
                kind_id,
                address,
                agent_id,
                owner_id,
                delivery_mode,
            }))
        }
    }
}

/// #925: `binding_id` から発火ターンに要る文脈を解決する（`load_origin_row` と同じ源）。
///
/// heartbeat 受け口（`crate::fire`）が owner の platform 別解決（`kind_id == "nostr"` 分岐や
/// `get_agent_*` 呼び出し）という **platform 語彙を持たずに済むよう**、その分岐をこの
/// 未分離 Nostr profile ファイル（allowlist 済み）へ集約する。live 判定（registry）は platform
/// 非依存なので呼び出し側（fire.rs）が行う。open binding・未削除 instance のみ解決する。
pub(crate) struct BindingContext {
    pub instance_id: String,
    pub kind_id: String,
    pub agent_id: String,
    pub owner_id: String,
    pub delivery_mode: DeliveryMode,
}

pub(crate) fn resolve_binding_context(
    conn: &Connection,
    binding_id: &str,
) -> Option<BindingContext> {
    let (instance_id, kind_id, agent_id, config_b64) = conn
        .query_row(
            "SELECT b.instance_id, i.kind_id, a.agent_id, i.config_b64
             FROM gate_bindings b
             JOIN gate_instances i ON i.instance_id = b.instance_id
             JOIN agents a ON a.subject_id = i.subject_id
             WHERE b.binding_id = ?1 AND b.closed_at IS NULL AND i.deleted_at IS NULL",
            params![binding_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .ok()?;
    let config_bytes = decode_config_b64(&config_b64).ok()?;
    let delivery_mode =
        crate::delivery_mode::delivery_mode_from_config_bytes(&config_bytes).ok()?;
    let owner_id = if kind_id == "nostr" {
        get_agent_nostr_owner_pubkey(conn, &agent_id).ok()?
    } else {
        get_agent_discord_config(conn, &agent_id)
            .ok()
            .flatten()
            .map(|c| c.owner_discord_id)
            .unwrap_or_default()
    };
    Some(BindingContext {
        instance_id,
        kind_id,
        agent_id,
        owner_id,
        delivery_mode,
    })
}
