//! instance canonical config（秘密を含めない）。

use opencrab_db::queries::SessionWatchRow;
use serde_json::{json, Value};

use crate::config::NostrConfig;

/// relays / filter / self pubkey / watch 行 / `delivery_mode=tool_driven`。
pub fn instance_config_value(
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
) -> Value {
    json!({
        "delivery_mode": "tool_driven",
        "filter": config.filter,
        "relays": config.effective_relays(),
        "self_pubkey": self_pubkey,
        "watches": watches.iter().map(|w| {
            json!({
                "id": w.id,
                "interval_secs": w.interval_secs,
                "session_id": w.session_id,
            })
        }).collect::<Vec<_>>(),
    })
}

pub fn instance_config_bytes(
    self_pubkey: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&instance_config_value(
        self_pubkey,
        config,
        watches,
    ))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NostrFilter;

    #[test]
    fn config_is_tool_driven_and_has_no_secret() {
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: NostrFilter::default(),
        };
        let raw = instance_config_bytes("aa".repeat(32).as_str(), &cfg, &[]).unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("tool_driven"));
        assert!(text.contains("self_pubkey"));
        assert!(!text.contains("nsec"));
        assert!(!text.contains("secret"));
    }
}
