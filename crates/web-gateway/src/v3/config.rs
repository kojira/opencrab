//! operator と gateway が共有する配置ファイル。Bearer は載せない。

use std::net::SocketAddr;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Placement {
    pub http_bind: String,
    pub core_socket: String,
    pub instances: Vec<InstancePlacement>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstancePlacement {
    pub instance_id: String,
    pub revision: u64,
    pub author_id: String,
}

impl Placement {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let place: Placement = serde_json::from_str(&text)?;
        place.validate()?;
        Ok(place)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let addr: SocketAddr = self
            .http_bind
            .parse()
            .map_err(|_| anyhow::anyhow!("http_bind must be a socket address"))?;
        if !addr.ip().is_loopback() {
            anyhow::bail!("http_bind must be loopback");
        }
        if self.core_socket.is_empty() || !self.core_socket.starts_with('/') {
            anyhow::bail!("core_socket must be an absolute path");
        }
        if self.instances.is_empty() {
            anyhow::bail!("instances must be nonempty");
        }
        let mut seen = std::collections::BTreeSet::new();
        for inst in &self.instances {
            if inst.revision == 0 {
                anyhow::bail!("revision must be positive");
            }
            if inst.author_id.is_empty() {
                anyhow::bail!("author_id must be nonempty");
            }
            crate::v3::wire::parse_uuid(&inst.instance_id)
                .map_err(|_| anyhow::anyhow!("instance_id must be canonical lowercase UUID"))?;
            if !seen.insert(inst.instance_id.clone()) {
                anyhow::bail!("duplicate instance_id is a double live; refuse startup");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback() {
        let p = Placement {
            http_bind: "0.0.0.0:80".into(),
            core_socket: "/tmp/g.sock".into(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                author_id: "owner".into(),
            }],
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn accepts_loopback() {
        let p = Placement {
            http_bind: "127.0.0.1:18700".into(),
            core_socket: "/tmp/g.sock".into(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                author_id: "owner".into(),
            }],
        };
        p.validate().unwrap();
    }
}
