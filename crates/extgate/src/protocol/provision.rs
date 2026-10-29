//! D-1098: `provision` — a gateway declares its own generic instance and the complete set of its
//! bindings over the ordinary gate socket, before `hello`. No credential: access to the socket
//! file is the only gate, as for every other frame.

use std::collections::BTreeSet;

use serde_json::Value;

use super::{nonempty_str, require_str};
use crate::error::{ErrorCode, GateError};
use crate::ids::{decode_config_b64, parse_request_id, parse_uuid};

const FIELDS: [&str; 8] = [
    "id",
    "m",
    "instance_id",
    "kind_id",
    "agent_id",
    "enabled",
    "config_b64",
    "bindings",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionBinding {
    pub binding_id: String,
    pub address: String,
}

/// `provision`: answered with `provisioned` (or `err`). The connection stays before `hello`.
#[derive(Debug, Clone)]
pub struct Provision {
    pub id: String,
    pub instance_id: String,
    pub kind_id: String,
    pub agent_id: String,
    pub enabled: bool,
    pub config_b64: String,
    /// The complete set: open bindings of the instance not listed here are closed.
    pub bindings: Vec<ProvisionBinding>,
}

pub(super) fn parse_provision(obj: &Value) -> Result<Provision, GateError> {
    let bad = || GateError::new(ErrorCode::BadRequest);
    let map = obj.as_object().ok_or_else(bad)?;
    if map.keys().any(|key| !FIELDS.contains(&key.as_str())) {
        return Err(bad());
    }
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let instance_id = parse_uuid(&require_str(obj, "instance_id")?)?;
    let kind_id = nonempty_str(obj, "kind_id")?;
    let agent_id = nonempty_str(obj, "agent_id")?;
    let enabled = obj
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(bad)?;
    let config_b64 = require_str(obj, "config_b64")?;
    decode_config_b64(&config_b64)?;
    let mut bindings = Vec::new();
    let mut ids = BTreeSet::new();
    let mut addresses = BTreeSet::new();
    for item in obj
        .get("bindings")
        .and_then(Value::as_array)
        .ok_or_else(bad)?
    {
        let fields = item.as_object().ok_or_else(bad)?;
        if fields.len() != 2 {
            return Err(bad());
        }
        let binding_id = parse_uuid(&require_str(item, "binding_id")?)?;
        let address = nonempty_str(item, "address")?;
        if !ids.insert(binding_id.clone()) || !addresses.insert(address.clone()) {
            return Err(bad());
        }
        bindings.push(ProvisionBinding {
            binding_id,
            address,
        });
    }
    Ok(Provision {
        id,
        instance_id,
        kind_id,
        agent_id,
        enabled,
        config_b64,
        bindings,
    })
}
