use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub core_socket: String,
    pub instances: Vec<InstancePlacement>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstancePlacement {
    pub instance_id: String,
    pub revision: u64,
    pub agent_id: String,
    pub author_id: String,
}

impl Placement {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).context("read placement")?;
        let placement: Self = serde_json::from_slice(&bytes).context("parse placement")?;
        placement.validate()?;
        Ok(placement)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !PathBuf::from(&self.core_socket).is_absolute() {
            anyhow::bail!("core_socket must be an absolute path");
        }
        if self.instances.is_empty() {
            anyhow::bail!("instances must be nonempty");
        }
        let mut instances = BTreeSet::new();
        let mut agents = BTreeSet::new();
        for instance in &self.instances {
            canonical_uuid(&instance.instance_id)
                .map_err(|_| anyhow::anyhow!("instance_id must be a canonical lowercase UUID"))?;
            if instance.revision == 0 {
                anyhow::bail!("revision must be positive");
            }
            if instance.agent_id.is_empty() {
                anyhow::bail!("agent_id must be nonempty");
            }
            if instance.author_id.is_empty() {
                anyhow::bail!("author_id must be nonempty");
            }
            if !instances.insert(instance.instance_id.as_str()) {
                anyhow::bail!("duplicate instance_id");
            }
            if !agents.insert(instance.agent_id.as_str()) {
                anyhow::bail!("duplicate agent_id");
            }
        }
        Ok(())
    }

    pub fn select_agent(&self, exact_id: &str) -> anyhow::Result<&InstancePlacement> {
        let mut matches = self
            .instances
            .iter()
            .filter(|item| item.agent_id == exact_id);
        let selected = matches
            .next()
            .ok_or_else(|| anyhow::anyhow!("selected agent is absent"))?;
        if matches.next().is_some() {
            anyhow::bail!("selected agent is ambiguous");
        }
        Ok(selected)
    }
}

pub fn canonical_uuid(raw: &str) -> Result<String, ()> {
    let parsed = uuid::Uuid::parse_str(raw).map_err(|_| ())?;
    let canonical = parsed.to_string();
    if canonical == raw {
        Ok(canonical)
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Placement {
        Placement {
            core_socket: "/tmp/gate.sock".into(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                agent_id: "agent-a".into(),
                author_id: "local-operator".into(),
            }],
        }
    }

    #[test]
    fn validates_and_selects_exact_agent() {
        let placement = valid();
        placement.validate().unwrap();
        assert_eq!(placement.select_agent("agent-a").unwrap().revision, 1);
        assert!(placement.select_agent("Agent A").is_err());
    }

    #[test]
    fn rejects_invalid_fields_and_duplicates() {
        let mut placement = valid();
        placement.instances[0].instance_id = "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".into();
        assert!(placement.validate().is_err());
        let mut placement = valid();
        placement.instances.push(placement.instances[0].clone());
        assert!(placement.validate().is_err());
    }

    #[test]
    fn load_rejects_unknown_members() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("placement.json");
        std::fs::write(
            &path,
            r#"{"core_socket":"/tmp/gate.sock","instances":[],"token":"no"}"#,
        )
        .unwrap();
        assert!(Placement::load(&path).is_err());
    }
}
