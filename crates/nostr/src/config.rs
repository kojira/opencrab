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

/// 受信イベントの**上乗せ**購読条件。空の項目は「その軸では上乗せしない」を意味する。
///
/// 全項目が空でも購読は無制限にならない。`nostaro watch` は **mention-only 既定**
/// （自分宛の p タグ）で購読し、opencrab は `--no-mention-only` を渡さないので、
/// 空フィルタ＝「自分宛のみ」という**最も狭い**購読になる（#271/#278）。
/// authors / keywords は `--match=any`（OR）で**その上に足す**条件であり、
/// 絞り込みではない。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NostrFilter {
    /// 追加で拾う author（npub か hex pubkey）。空なら author では上乗せしない。
    #[serde(default)]
    pub authors: Vec<String>,
    /// 追加で拾う content キーワード（いずれか1つでも一致で採用）。
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
    ///
    /// #514: DM の kind（[`crate::event::DM_KINDS`] = 4 / 1059）は**購読から必ず外す**。
    /// 設定（DB 行）に混ざっていても、そもそもリレーへ DM を要求しない（「要求してから
    /// 捨てる」より「要求しない」方が漏れ余地が無い）。DB を手編集された場合や、
    /// `configure_nostr` / REST の書き込み側ストリップをすり抜けた場合の最終防壁でもある。
    /// 受信ループ（`manager::handle_event`）の破棄と合わせて二重に効く。
    /// 除外後に空になったら kind:1（テキストノート）へフォールバックする。
    pub fn effective_kinds(&self) -> Vec<u32> {
        let filtered: Vec<u32> = self
            .filter
            .kinds
            .iter()
            .copied()
            .filter(|k| !crate::event::DM_KINDS.contains(k))
            .collect();
        if filtered.is_empty() {
            vec![1]
        } else {
            filtered
        }
    }

    /// 自分宛（p タグ）**以外**も拾う設定か（authors か keywords を上乗せしている）。
    ///
    /// かつてここには「author も keyword も無い＝全ノート洪水」として起動を拒否する
    /// `filter_is_unbounded()` があった。旧 nostaro の `--json` は mention-only を
    /// 無視して kind:1 を全件購読していたので、その時は正しい判定だった。
    ///
    /// **新 nostaro では判定が逆転する**（#271/#278）。`--json` でも mention-only が
    /// 既定で効き、opencrab は `--no-mention-only` を渡さないので、
    ///
    /// - フィルタ**未指定** → 購読は「自分宛の p タグのみ」＝**最も狭い**（洪水ではない）、
    /// - keywords 指定 → nostaro が keyword 用に kind 全体の購読を別途張る（内容一致は
    ///   ローカル判定）＝相対的に**広い**、
    ///
    /// となる。つまり旧ガードは「一番狭い設定だけを拒否する」ものになっていたので撤去し、
    /// 「自分宛は必ず届く」という不変条件は
    /// [`NostaroCli::build_watch_command`](crate::cli::NostaroCli::build_watch_command) が
    /// `--no-mention-only` を渡さないこと（テストで固定）で担保する。
    ///
    /// この述語自体は「運用者が上乗せ条件を設定済みか」を見るためだけに残す（判定であって
    /// ガードではない）。
    pub fn watches_beyond_self_mentions(&self) -> bool {
        !self.filter.authors.is_empty() || !self.filter.keywords.is_empty()
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

    /// [#514] DM kind（4 / 1059）は購読から必ず外れる。エージェントが `configure_nostr` で
    /// 混ぜても、DB を手編集しても、そもそもリレーへ要求しない。除外後に空なら kind:1。
    #[test]
    fn test_effective_kinds_strips_dm_kinds() {
        // 通常 kind と DM が混在 → DM だけ落ちる。
        let mixed = NostrConfig {
            relays: vec![],
            filter: NostrFilter {
                kinds: vec![1, 4, 7, 1059, 30023],
                ..Default::default()
            },
        };
        assert_eq!(mixed.effective_kinds(), vec![1, 7, 30023]);
        // DM だけ → 空になるので kind:1 へフォールバック（DM は絶対に購読しない）。
        let only_dm = NostrConfig {
            relays: vec![],
            filter: NostrFilter {
                kinds: vec![4, 1059],
                ..Default::default()
            },
        };
        assert_eq!(only_dm.effective_kinds(), vec![1]);
    }

    /// [#271/#278] フィルタ未指定は「自分宛のみ」＝**最も狭い**購読であって洪水ではない。
    /// 旧 `filter_is_unbounded()` はここを起動拒否していた（旧 nostaro の json が
    /// mention-only を無視していた頃の判定）。上乗せ条件の有無だけを見る述語に置き換えた。
    #[test]
    fn test_empty_filter_is_self_mentions_only() {
        assert!(
            !NostrConfig::default().watches_beyond_self_mentions(),
            "未指定は自分宛のみ（上乗せ無し）"
        );
        let with_keyword = NostrConfig {
            relays: vec![],
            filter: NostrFilter {
                keywords: vec!["opencrab".to_string()],
                ..Default::default()
            },
        };
        assert!(with_keyword.watches_beyond_self_mentions());
        let with_author = NostrConfig {
            relays: vec![],
            filter: NostrFilter {
                authors: vec!["npub1abc".to_string()],
                ..Default::default()
            },
        };
        assert!(with_author.watches_beyond_self_mentions());
    }
}
