use crate::canonical;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fs, path::Path};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub version: u64,
    pub operation_id: String,
    pub created_at: String,
    pub core_user_version: u64,
    pub source_core_sha256: String,
    pub destinations: Vec<Destination>,
    pub identity_dispositions: Vec<IdentityDisposition>,
    pub channel_edges: Vec<ChannelEdge>,
    pub watch_edges: Vec<WatchEdge>,
    pub credential_sources: Vec<CredentialSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub kind_id: String,
    pub path_id: String,
    pub schema: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityDisposition {
    pub source_fingerprint: String,
    pub edges: Vec<IdentityEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum IdentityEdge {
    ApiPrincipal,
    Gateway {
        kind_id: String,
        instance_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelEdge {
    pub source_fingerprint: String,
    pub instance_id: String,
    pub binding_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchEdge {
    pub source_fingerprint: String,
    pub instance_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSource {
    pub instance_id: String,
    pub source: String,
}

impl Approval {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read(path).context("read approval")?;
        let mut de = serde_json::Deserializer::from_slice(&raw);
        let approval = Approval::deserialize(&mut de).context("strict approval JSON")?;
        de.end().context("trailing approval JSON")?;
        approval.validate()?;
        ensure!(
            raw == canonical::bytes(&approval)?,
            "approval is not RFC-8785 canonical JSON"
        );
        Ok(approval)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "approval version must be 1");
        ensure!(self.core_user_version == 56, "core_user_version must be 56");
        let operation = Uuid::parse_str(&self.operation_id).context("operation_id")?;
        ensure!(
            operation.get_version_num() == 4,
            "operation_id must be UUIDv4"
        );
        ensure!(
            valid_timestamp(&self.created_at),
            "created_at must be UTC seconds"
        );
        ensure!(
            canonical::is_sha256(&self.source_core_sha256),
            "invalid source hash"
        );
        ensure_sorted_unique(&self.destinations, "destinations")?;
        ensure_sorted_by(
            &self.identity_dispositions,
            |value| value.source_fingerprint.as_bytes(),
            "identity_dispositions",
        )?;
        for disposition in &self.identity_dispositions {
            ensure!(
                canonical::is_sha256(&disposition.source_fingerprint),
                "invalid source fingerprint"
            );
            ensure!(
                !disposition.edges.is_empty(),
                "identity disposition has zero edges"
            );
            for pair in disposition.edges.windows(2) {
                ensure!(
                    canonical::bytes(&pair[0])? < canonical::bytes(&pair[1])?,
                    "identity edges must be strictly sorted by canonical JSON"
                );
            }
        }
        ensure_sorted_unique(&self.channel_edges, "channel_edges")?;
        ensure_sorted_unique(&self.watch_edges, "watch_edges")?;
        ensure_sorted_unique(&self.credential_sources, "credential_sources")?;
        Ok(())
    }

    pub fn sha256(&self) -> Result<String> {
        canonical::hash(self)
    }

    pub fn destination(&self, kind_id: &str) -> Result<&Destination> {
        let mut matches = self
            .destinations
            .iter()
            .filter(|item| item.kind_id == kind_id);
        let first = matches.next().context("destination is not approved")?;
        ensure!(
            matches.next().is_none(),
            "multiple destinations for kind require an explicit path selection"
        );
        Ok(first)
    }
}

fn valid_timestamp(value: &str) -> bool {
    if value.len() != 20 || !value.ends_with('Z') {
        return false;
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.to_rfc3339_opts(chrono::SecondsFormat::Secs, true) == value)
        .unwrap_or(false)
}

fn ensure_sorted_unique<T: Ord>(values: &[T], label: &str) -> Result<()> {
    for pair in values.windows(2) {
        if pair[0] >= pair[1] {
            bail!("{label} must be strictly sorted");
        }
    }
    Ok(())
}

fn ensure_sorted_by<'a, T, F>(values: &'a [T], key: F, label: &str) -> Result<()>
where
    F: Fn(&'a T) -> &'a [u8],
{
    let mut seen = BTreeSet::new();
    let mut previous: Option<&[u8]> = None;
    for value in values {
        let current = key(value);
        if previous.is_some_and(|old| old >= current) || !seen.insert(current) {
            bail!("{label} must be strictly sorted");
        }
        previous = Some(current);
    }
    Ok(())
}
