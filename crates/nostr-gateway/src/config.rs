//! operator と gateway が共有する配置。HTTP bind は無い。秘密は載せない。

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Placement {
    pub core_socket: String,
    pub nostaro_bin: String,
    pub instances: Vec<InstancePlacement>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstancePlacement {
    pub instance_id: String,
    pub revision: u64,
    pub address: String,
    pub config_b64: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstanceConfig {
    pub relays: Vec<String>,
    #[serde(default)]
    pub filter: WatchFilter,
    pub self_pubkey: String,
    #[serde(default)]
    pub watches: Vec<WatchPlacement>,
    #[serde(default)]
    pub delivery_mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WatchFilter {
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchPlacement {
    pub id: i64,
    pub interval_secs: i64,
    #[serde(default)]
    pub filter: WatchFilter,
}

const DM_KINDS: &[u32] = &[4, 1059];

impl Placement {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let place: Placement = serde_json::from_str(&text)?;
        place.validate()?;
        Ok(place)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.core_socket.is_empty() || !self.core_socket.starts_with('/') {
            anyhow::bail!("core_socket must be an absolute path");
        }
        if self.nostaro_bin.is_empty() {
            anyhow::bail!("nostaro_bin must be nonempty");
        }
        if self.instances.is_empty() {
            anyhow::bail!("instances must be nonempty");
        }
        let mut seen = std::collections::BTreeSet::new();
        for inst in &self.instances {
            if inst.revision == 0 {
                anyhow::bail!("revision must be positive");
            }
            if inst.address.is_empty() {
                anyhow::bail!("address must be nonempty");
            }
            parse_uuid(&inst.instance_id)?;
            if !seen.insert(inst.instance_id.clone()) {
                anyhow::bail!("duplicate instance_id is a double live; refuse startup");
            }
            let bytes = decode_config_b64(&inst.config_b64)?;
            let cfg = parse_instance_config(&bytes)?;
            validate_instance_config(&cfg)?;
        }
        Ok(())
    }
}

pub fn decode_config_b64(config_b64: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(config_b64)
        .map_err(|_| anyhow::anyhow!("config_b64 must be standard padded base64"))
}

pub fn config_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_lower(&Sha256::digest(bytes))
}

pub fn parse_instance_config(bytes: &[u8]) -> anyhow::Result<InstanceConfig> {
    let cfg: InstanceConfig = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("instance config is not valid JSON object: {e}"))?;
    validate_instance_config(&cfg)?;
    Ok(cfg)
}

fn validate_instance_config(cfg: &InstanceConfig) -> anyhow::Result<()> {
    if cfg.relays.is_empty() {
        anyhow::bail!("relays must be nonempty");
    }
    if !is_hex_pubkey(&cfg.self_pubkey) {
        anyhow::bail!("self_pubkey must be 64 lowercase hex");
    }
    match cfg.delivery_mode.as_deref() {
        None | Some("say") | Some("tool_driven") => {}
        Some(_) => anyhow::bail!("delivery_mode must be say or tool_driven"),
    }
    for watch in &cfg.watches {
        if watch.interval_secs <= 0 {
            anyhow::bail!("watch {} interval_secs must be a positive integer", watch.id);
        }
    }
    Ok(())
}

pub fn effective_kinds(filter: &WatchFilter) -> Vec<u32> {
    let filtered: Vec<u32> = filter
        .kinds
        .iter()
        .copied()
        .filter(|k| !DM_KINDS.contains(k))
        .collect();
    if filtered.is_empty() {
        vec![1]
    } else {
        filtered
    }
}

pub fn watches_beyond_self(filter: &WatchFilter) -> bool {
    !filter.authors.is_empty() || !filter.keywords.is_empty()
}

pub fn parse_uuid(raw: &str) -> anyhow::Result<String> {
    let parsed = uuid::Uuid::parse_str(raw)?;
    let canonical = parsed.to_string();
    if canonical != raw {
        anyhow::bail!("instance_id must be canonical lowercase UUID");
    }
    Ok(canonical)
}

fn is_hex_pubkey(raw: &str) -> bool {
    raw.len() == 64
        && raw
            .bytes()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> InstanceConfig {
        InstanceConfig {
            relays: vec!["wss://example.invalid".into()],
            filter: WatchFilter::default(),
            self_pubkey: "aa".repeat(32),
            watches: vec![],
            delivery_mode: Some("tool_driven".into()),
        }
    }

    #[test]
    fn rejects_http_listen_fields_by_absence() {
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            nostaro_bin: "nostaro".into(),
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                address: "nostr-a1".into(),
                config_b64: {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD
                        .encode(serde_json::to_vec(&serde_json::json!({
                            "relays": ["wss://example.invalid"],
                            "self_pubkey": "aa".repeat(32),
                        })).unwrap())
                },
            }],
        };
        p.validate().unwrap();
    }

    #[test]
    fn empty_relays_fail_loud() {
        let mut cfg = sample_config();
        cfg.relays.clear();
        assert!(validate_instance_config(&cfg).is_err());
    }

    #[test]
    fn empty_kinds_mean_kind_1_and_dm_kinds_are_stripped() {
        assert_eq!(effective_kinds(&WatchFilter::default()), vec![1]);
        let f = WatchFilter {
            kinds: vec![1, 4, 1059, 7],
            ..WatchFilter::default()
        };
        assert_eq!(effective_kinds(&f), vec![1, 7]);
    }
}
