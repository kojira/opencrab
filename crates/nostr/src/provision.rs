//! instance canonical config（秘密を含めない）。

use anyhow::Context;
use opencrab_db::queries::SessionWatchRow;
use serde_json::{json, Value};

use crate::config::NostrConfig;

/// relays / filter / self pubkey / agents.name / watch 行 / `delivery_mode=tool_driven`。
/// watch 行の `filter_json` が object でなければ失敗する（空に置き換えない）。
/// `name` が空なら失敗する（メンション車線の keyword に載せる名前が必須）。
pub fn instance_config_value(
    self_pubkey: &str,
    name: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
) -> anyhow::Result<Value> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("agents.name が空（メンション車線の keyword にできない）");
    }
    let mut watch_values = Vec::with_capacity(watches.len());
    for w in watches {
        let filter_json: Value = serde_json::from_str(&w.filter_json).with_context(|| {
            format!(
                "session_watches.id={} の filter_json が JSON ではない",
                w.id
            )
        })?;
        if !filter_json.is_object() {
            anyhow::bail!(
                "session_watches.id={} の filter_json は JSON object が必須",
                w.id
            );
        }
        watch_values.push(json!({
            "filter_json": filter_json,
            "id": w.id,
            "interval_secs": w.interval_secs,
            "session_id": w.session_id,
        }));
    }
    Ok(json!({
        "delivery_mode": "tool_driven",
        "filter": config.filter,
        "name": name,
        "relays": config.effective_relays(),
        "self_pubkey": self_pubkey,
        "watches": watch_values,
    }))
}

pub fn instance_config_bytes(
    self_pubkey: &str,
    name: &str,
    config: &NostrConfig,
    watches: &[SessionWatchRow],
) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&instance_config_value(
        self_pubkey,
        name,
        config,
        watches,
    )?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NostrFilter;
    use opencrab_db::queries::SessionWatchRow;

    #[test]
    fn config_is_tool_driven_and_has_no_secret() {
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: NostrFilter::default(),
        };
        let raw = instance_config_bytes("aa".repeat(32).as_str(), "くらぶ", &cfg, &[]).unwrap();
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("tool_driven"));
        assert!(text.contains("self_pubkey"));
        assert!(text.contains("くらぶ"));
        assert!(!text.contains("nsec"));
        assert!(!text.contains("secret"));
    }

    #[test]
    fn watch_row_keeps_filter_json() {
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: NostrFilter::default(),
        };
        let watches = [SessionWatchRow {
            id: 17,
            session_id: "nostr-a1".into(),
            agent_id: "a1".into(),
            interval_secs: 30,
            filter_json: r#"{"authors":["npub1watched"],"keywords":["opencrab"],"kinds":[1,7]}"#
                .into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }];
        let value =
            instance_config_value("aa".repeat(32).as_str(), "くらぶ", &cfg, &watches).unwrap();
        assert_eq!(value["name"], "くらぶ");
        let filter = &value["watches"][0]["filter_json"];
        assert_eq!(filter["authors"][0], "npub1watched");
        assert_eq!(filter["keywords"][0], "opencrab");
        assert_eq!(filter["kinds"][1], 7);
    }

    #[test]
    fn broken_filter_json_is_fail_loud() {
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: NostrFilter::default(),
        };
        let watches = [SessionWatchRow {
            id: 1,
            session_id: "nostr-a1".into(),
            agent_id: "a1".into(),
            interval_secs: 30,
            filter_json: "[]".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }];
        let err =
            instance_config_value("aa".repeat(32).as_str(), "くらぶ", &cfg, &watches).unwrap_err();
        assert!(err.to_string().contains("filter_json"));
    }

    #[test]
    fn empty_name_is_fail_loud() {
        let cfg = NostrConfig {
            relays: vec!["wss://yabu.me".into()],
            filter: NostrFilter::default(),
        };
        let err = instance_config_value("aa".repeat(32).as_str(), "   ", &cfg, &[]).unwrap_err();
        assert!(err.to_string().contains("agents.name"));
    }
}
