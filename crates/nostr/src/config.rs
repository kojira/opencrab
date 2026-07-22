//! Nostr ゲートウェイの設定（リレー・購読フィルタ・鍵の場所）。

use serde::{Deserialize, Serialize};

/// 既定リレー。ダッシュボードから変更可能な**初期値**にすぎない（不変の
/// allowlist ではない）。稼働中のリレー2つを既定にする。
pub const DEFAULT_RELAYS: &[&str] = &["wss://yabu.me", "wss://r.kojira.io"];

/// エージェント1体の Nostr ゲートウェイ設定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NostrConfig {
    /// 購読するリレー。空なら [`DEFAULT_RELAYS`] を使う。
    #[serde(default)]
    pub relays: Vec<String>,
    /// 購読フィルタ。
    #[serde(default)]
    pub filter: NostrFilter,
}

/// 受信イベントの絞り込み。空の項目は「その軸では絞らない」を意味する。
/// 少なくとも1軸は指定させる想定（全 kind の全 author を拾うと洪水になるため、
/// 呼び出し側で空フィルタを弾く）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NostrFilter {
    /// 対象 author（npub か hex pubkey）。空なら author で絞らない。
    #[serde(default)]
    pub authors: Vec<String>,
    /// content に含まれるべきキーワード（いずれか1つでも一致で採用）。
    #[serde(default)]
    pub keywords: Vec<String>,
    /// 対象 kind。空なら kind:1（テキストノート）を既定にする。
    #[serde(default)]
    pub kinds: Vec<u32>,
}

impl NostrConfig {
    /// 実効リレー（設定が空なら既定）。
    pub fn effective_relays(&self) -> Vec<String> {
        if self.relays.is_empty() {
            DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
        } else {
            self.relays.clone()
        }
    }

    /// 実効 kind（設定が空なら kind:1）。
    pub fn effective_kinds(&self) -> Vec<u32> {
        if self.filter.kinds.is_empty() {
            vec![1]
        } else {
            self.filter.kinds.clone()
        }
    }

    /// フィルタが実質空（author も keyword も無い）か。全ノート洪水を防ぐため
    /// 呼び出し側でこれを弾く。
    pub fn filter_is_unbounded(&self) -> bool {
        self.filter.authors.is_empty() && self.filter.keywords.is_empty()
    }
}

impl Default for NostrConfig {
    fn default() -> Self {
        Self {
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            filter: NostrFilter::default(),
        }
    }
}

/// DB 行（relays_json / filter_json）を [`NostrConfig`] にパースする。
/// 壊れた JSON は既定（空）にフォールバックする。
pub fn config_from_row(row: &opencrab_db::queries::AgentNostrConfigRow) -> NostrConfig {
    let relays: Vec<String> = serde_json::from_str(&row.relays_json).unwrap_or_default();
    let filter: NostrFilter = serde_json::from_str(&row.filter_json).unwrap_or_default();
    NostrConfig { relays, filter }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_relays_are_the_two_live_ones() {
        let c = NostrConfig::default();
        assert_eq!(
            c.effective_relays(),
            vec!["wss://yabu.me", "wss://r.kojira.io"]
        );
    }

    #[test]
    fn test_effective_relays_uses_config_when_set() {
        let c = NostrConfig {
            relays: vec!["wss://relay.example".to_string()],
            filter: NostrFilter::default(),
        };
        assert_eq!(c.effective_relays(), vec!["wss://relay.example"]);
    }

    #[test]
    fn test_effective_kinds_defaults_to_text_note() {
        let c = NostrConfig::default();
        assert_eq!(c.effective_kinds(), vec![1]);
        let c2 = NostrConfig {
            relays: vec![],
            filter: NostrFilter {
                kinds: vec![1, 30023],
                ..Default::default()
            },
        };
        assert_eq!(c2.effective_kinds(), vec![1, 30023]);
    }

    #[test]
    fn test_unbounded_filter_detection() {
        assert!(NostrConfig::default().filter_is_unbounded());
        let bounded = NostrConfig {
            relays: vec![],
            filter: NostrFilter {
                keywords: vec!["opencrab".to_string()],
                ..Default::default()
            },
        };
        assert!(!bounded.filter_is_unbounded());
    }
}
