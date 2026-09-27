//! One-shot V3 pre-hello generic provisioning client.

use anyhow::{bail, Context as _, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::BufReader;
use tokio::net::UnixStream;

use crate::wire::{read_frame, write_json};

#[derive(Clone)]
pub struct ProvisionClient {
    socket: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProvisionDesired<'a> {
    pub instance_id: &'a str,
    pub kind_id: &'a str,
    pub subject_id: i64,
    pub subject_grant: Option<&'a str>,
    pub adopt_existing: bool,
    pub enabled: bool,
    pub config_b64: &'a str,
    pub addresses: &'a [String],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioned {
    pub revision: u64,
    pub config_digest: String,
    pub enabled: bool,
    pub bindings: Vec<ProvisionedBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedBinding {
    pub binding_id: String,
    pub address: String,
}

impl ProvisionClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub async fn provision(&self, desired: ProvisionDesired<'_>) -> Result<Provisioned> {
        let namespace =
            uuid::Uuid::parse_str(desired.instance_id).context("instance_id must be UUID")?;
        let bindings = desired.addresses.iter().map(|address| {
            json!({"binding_id": uuid::Uuid::new_v5(&namespace, address.as_bytes()).to_string(), "address": address})
        }).collect::<Vec<_>>();
        let id = uuid::Uuid::new_v4().to_string();
        let mut frame = json!({
            "m":"provision", "id":id, "instance_id":desired.instance_id, "kind_id":desired.kind_id,
            "subject_id":desired.subject_id, "adopt_existing":desired.adopt_existing,
            "enabled":desired.enabled, "config_b64":desired.config_b64, "bindings":bindings,
        });
        if let Some(grant) = desired.subject_grant {
            frame["subject_grant"] = json!(grant);
        }
        let stream = UnixStream::connect(&self.socket).await?;
        let (read, mut write) = stream.into_split();
        write_json(&mut write, &frame)
            .await
            .map_err(|error| anyhow::anyhow!("provision write failed: {error:?}"))?;
        drop(write);
        let mut reader = BufReader::new(read);
        let bytes = read_frame(&mut reader)
            .await
            .map_err(|error| anyhow::anyhow!("provision read failed: {error:?}"))?;
        let value: Value = serde_json::from_slice(bytes.strip_suffix(b"\n").unwrap_or(&bytes))?;
        if value.get("m").and_then(Value::as_str) == Some("err") {
            bail!(
                "provision rejected: {}",
                value
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            );
        }
        anyhow::ensure!(value.get("m").and_then(Value::as_str) == Some("provisioned"));
        anyhow::ensure!(value.get("id").and_then(Value::as_str) == Some(id.as_str()));
        let revision = value
            .get("revision")
            .and_then(Value::as_u64)
            .context("missing provision revision")?;
        let config_digest = value
            .get("config_digest")
            .and_then(Value::as_str)
            .context("missing provision digest")?
            .to_string();
        let enabled = value
            .get("enabled")
            .and_then(Value::as_bool)
            .context("missing provision enabled")?;
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
            revision,
            config_digest,
            enabled,
            bindings,
        })
    }
}
