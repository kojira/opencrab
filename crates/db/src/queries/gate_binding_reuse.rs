//! D-1098: an instance's binding for an address is reused before a new one is created.
//!
//! Bindings can predate the ids a gateway derives today (migration v54 kept their ids and set
//! `session_id = address`). Creating a second binding for the same instance and address is refused
//! by the open-address index, and a closed binding cannot be created again under its id. So both
//! the declarative `provision` and the runtime `create_binding` first look for the instance's own
//! binding for that address:
//! - an open one is kept exactly as it is (id, session, title);
//! - a closed one is reopened (`closed_at` cleared), keeping its id and session, so the
//!   conversation continues.
//!
//! Session rows are never modified here.

use anyhow::Result;
use rusqlite::{params, OptionalExtension, Transaction};

/// What [`reuse_instance_binding_in_tx`] did for the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceBindingReuse {
    /// The instance has no binding for this address; the caller creates one.
    None,
    /// The instance's open binding for this address, kept as it is.
    Open { binding_id: String },
    /// The instance's most recent closed binding for this address, reopened.
    Reopened { binding_id: String },
}

impl InstanceBindingReuse {
    pub fn binding_id(&self) -> Option<&str> {
        match self {
            InstanceBindingReuse::None => None,
            InstanceBindingReuse::Open { binding_id }
            | InstanceBindingReuse::Reopened { binding_id } => Some(binding_id),
        }
    }
}

/// Returns the instance's binding for `address`, reopening the most recent closed one when no
/// open one exists. Commit is the caller's.
pub fn reuse_instance_binding_in_tx(
    tx: &Transaction<'_>,
    instance_id: &str,
    address: &str,
) -> Result<InstanceBindingReuse> {
    let open: Option<String> = tx
        .query_row(
            "SELECT binding_id FROM gate_bindings
             WHERE instance_id=?1 AND address=?2 AND closed_at IS NULL",
            params![instance_id, address],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(binding_id) = open {
        return Ok(InstanceBindingReuse::Open { binding_id });
    }
    let closed: Option<String> = tx
        .query_row(
            "SELECT binding_id FROM gate_bindings
             WHERE instance_id=?1 AND address=?2 AND closed_at IS NOT NULL
             ORDER BY closed_at DESC, created_at DESC LIMIT 1",
            params![instance_id, address],
            |row| row.get(0),
        )
        .optional()?;
    let Some(binding_id) = closed else {
        return Ok(InstanceBindingReuse::None);
    };
    tx.execute(
        "UPDATE gate_bindings SET closed_at=NULL WHERE binding_id=?1",
        [&binding_id],
    )?;
    Ok(InstanceBindingReuse::Reopened { binding_id })
}
