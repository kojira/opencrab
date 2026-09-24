//! Pure Nostr watch subscription and inbound classification policy shared with V3 admission.

use opencrab_db::queries::SessionWatchRow;

use crate::config::{NostrConfig, NostrFilter};
use crate::event::NostrEvent;
use crate::pubkey::follow_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchForward {
    Discard,
    Immediate { label: &'static str },
    Bundle { label: &'static str },
}

pub fn parse_watch_filter(filter_json: &str) -> anyhow::Result<NostrFilter> {
    let value: serde_json::Value = serde_json::from_str(filter_json)
        .map_err(|e| anyhow::anyhow!("session_watches.filter_json が読めない: {e}"))?;
    if !value.is_object() {
        anyhow::bail!("session_watches.filter_json は JSON object が必須");
    }
    serde_json::from_value(value).map_err(|e| {
        anyhow::anyhow!("session_watches.filter_json が NostrFilter として読めない: {e}")
    })
}

pub fn watch_subscribe_config(
    watch: &SessionWatchRow,
    relays: Vec<String>,
) -> anyhow::Result<NostrConfig> {
    if watch.interval_secs <= 0 {
        anyhow::bail!(
            "session_watches.id={} の interval_secs が正の整数ではない（既定値は使わない）",
            watch.id
        );
    }
    let filter = parse_watch_filter(&watch.filter_json)?;
    Ok(NostrConfig { relays, filter })
}

fn p_tag_is_self(event: &NostrEvent, self_pubkey: &str) -> bool {
    let self_key = follow_key(self_pubkey);
    event.tags.iter().any(|tag| {
        tag.first().is_some_and(|kind| kind == "p")
            && tag.get(1).is_some_and(|key| follow_key(key) == self_key)
    })
}

fn has_e_tag(event: &NostrEvent) -> bool {
    event
        .tags
        .iter()
        .any(|tag| tag.first().is_some_and(|kind| kind == "e"))
}

fn e_tag_is_self(event: &NostrEvent, self_pubkey: &str) -> bool {
    let self_key = follow_key(self_pubkey);
    event.tags.iter().any(|tag| {
        tag.first().is_some_and(|kind| kind == "e")
            && tag
                .iter()
                .skip(1)
                .any(|value| follow_key(value) == self_key)
    })
}

fn watch_kind_label(event: &NostrEvent) -> &'static str {
    if event.is_dm() {
        "DM"
    } else if event.kind == 7 {
        "リアクション"
    } else if event.kind == 6 || event.kind == 16 {
        "リポスト"
    } else if event.kind == 30023 {
        "長文"
    } else if has_e_tag(event) {
        "リプライ"
    } else {
        "メンション"
    }
}

pub fn classify_watch_event(
    event: &NostrEvent,
    self_pubkey: &str,
    watches_beyond_self_mentions: bool,
) -> WatchForward {
    if event.is_dm() {
        return WatchForward::Discard;
    }
    if event.kind == 7 {
        return WatchForward::Immediate {
            label: "リアクション",
        };
    }
    if event.kind == 6 || event.kind == 16 {
        return WatchForward::Immediate {
            label: "リポスト"
        };
    }
    let to_self = p_tag_is_self(event, self_pubkey);
    if event.kind == 30023 {
        return if to_self || e_tag_is_self(event, self_pubkey) {
            WatchForward::Immediate { label: "長文" }
        } else {
            WatchForward::Bundle { label: "長文" }
        };
    }
    if to_self {
        return if has_e_tag(event) {
            WatchForward::Immediate {
                label: "リプライ"
            }
        } else {
            WatchForward::Immediate {
                label: "メンション",
            }
        };
    }
    if !watches_beyond_self_mentions && !has_e_tag(event) {
        return WatchForward::Immediate {
            label: "メンション",
        };
    }
    WatchForward::Bundle {
        label: watch_kind_label(event),
    }
}
