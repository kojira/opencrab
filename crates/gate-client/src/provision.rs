//! D-1098: one `provision` frame before `hello` over the ordinary gate socket. A gateway declares
//! its own generic instance and the complete set of its bindings; no credential is involved.

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::wire::{read_frame, write_json};

/// What a gateway wants core to hold for one instance.
#[derive(Debug, Clone)]
pub struct ProvisionDesired<'a> {
    pub instance_id: &'a str,
    pub kind_id: &'a str,
    pub agent_id: &'a str,
    pub enabled: bool,
    pub config_b64: &'a str,
    /// The complete set. Each binding id is UUIDv5(instance_id, address).
    pub addresses: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedBinding {
    pub binding_id: String,
    pub address: String,
}

/// Core's answer: what the following `hello` must carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioned {
    pub revision: u64,
    pub config_digest: String,
    pub enabled: bool,
    pub bindings: Vec<ProvisionedBinding>,
}

/// The binding id `provision` uses for an address of an instance.
pub fn binding_id_for(instance_id: &str, address: &str) -> Result<String> {
    let namespace = uuid::Uuid::parse_str(instance_id).context("instance_id must be a UUID")?;
    Ok(uuid::Uuid::new_v5(&namespace, address.as_bytes()).to_string())
}

/// Builds the `provision` frame.
pub fn provision_frame(id: &str, desired: &ProvisionDesired<'_>) -> Result<Value> {
    let bindings = desired
        .addresses
        .iter()
        .map(|address| {
            Ok(json!({
                "binding_id": binding_id_for(desired.instance_id, address)?,
                "address": address,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "m": "provision",
        "id": id,
        "instance_id": desired.instance_id,
        "kind_id": desired.kind_id,
        "agent_id": desired.agent_id,
        "enabled": desired.enabled,
        "config_b64": desired.config_b64,
        "bindings": bindings,
    }))
}

#[derive(Clone)]
pub struct ProvisionClient {
    socket: PathBuf,
}

impl ProvisionClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Opens a connection, sends one `provision`, reads the answer. Core closes the connection
    /// after answering; the gateway then connects again and says `hello`.
    pub async fn provision(&self, desired: &ProvisionDesired<'_>) -> Result<Provisioned> {
        let id = uuid::Uuid::new_v4().to_string();
        let frame = provision_frame(&id, desired)?;
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect gate socket {}", self.socket.display()))?;
        let (read, mut write) = stream.into_split();
        write_json(&mut write, &frame)
            .await
            .map_err(|error| anyhow::anyhow!("provision write failed: {error:?}"))?;
        let mut reader = BufReader::new(read);
        let bytes = read_frame(&mut reader)
            .await
            .map_err(|error| anyhow::anyhow!("provision read failed: {error:?}"))?;
        let value: Value = serde_json::from_slice(bytes.strip_suffix(b"\n").unwrap_or(&bytes))?;
        parse_provisioned(&value, &id)
    }
}

fn parse_provisioned(value: &Value, id: &str) -> Result<Provisioned> {
    if value.get("m").and_then(Value::as_str) == Some("err") {
        bail!(
            "provision rejected: {}",
            value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    anyhow::ensure!(
        value.get("m").and_then(Value::as_str) == Some("provisioned"),
        "unexpected provision answer"
    );
    anyhow::ensure!(
        value.get("id").and_then(Value::as_str) == Some(id),
        "provision answer id mismatch"
    );
    let bindings = value
        .get("bindings")
        .and_then(Value::as_array)
        .context("missing provision bindings")?
        .iter()
        .map(|binding| {
            Ok(ProvisionedBinding {
                binding_id: binding
                    .get("binding_id")
                    .and_then(Value::as_str)
                    .context("missing binding id")?
                    .to_string(),
                address: binding
                    .get("address")
                    .and_then(Value::as_str)
                    .context("missing binding address")?
                    .to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Provisioned {
        revision: value
            .get("revision")
            .and_then(Value::as_u64)
            .context("missing provision revision")?,
        config_digest: value
            .get("config_digest")
            .and_then(Value::as_str)
            .context("missing provision digest")?
            .to_string(),
        enabled: value
            .get("enabled")
            .and_then(Value::as_bool)
            .context("missing provision enabled")?,
        bindings,
    })
}
