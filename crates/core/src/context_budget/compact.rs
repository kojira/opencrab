//! 二水位圧縮（#826-B）。
//!
//! 高水位超過でだけ刈り、低水位まで落とす。車線は字句順の優先:
//! 到達点チェックポイント不可侵 → 直近逐語 → エコー参照化 → 古い履歴の要約。
//! 合計は [`crate::context_budget::TokenLedger`] の加減算だけを使い、全文再 encode しない。
//! assistant の said と同一応答の tool calls / results は [`ExchangeGroup`] として原子的に扱う。

use sha2::{Digest, Sha256};

use super::checkpoint::{CheckpointLane, CHECKPOINT_EMPTY_MARKER};
use super::ledger::TokenLedger;

/// 圧縮車線。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactLane {
    Checkpoint,
    RecentVerbatim,
    Echoable,
    OldHistory,
}

/// キャッシュ済み token を持つ会話単位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactItem {
    pub key: String,
    pub tokens: usize,
    pub text: String,
    pub lane: CompactLane,
    pub log_id: Option<i64>,
    /// 直近ユーザー発言など、同車線内でも先に枠を取る。
    pub must_keep: bool,
    /// 同一 [`ExchangeGroup`] に属する単位。`None` は単独。
    pub group_id: Option<u64>,
}

/// assistant の said と同一応答の tool calls、および対応する全 tool results の原子単位。
///
/// call ID の対応を保ち、片側だけを落としたり、未決着 group を要約したりしない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExchangeGroup {
    pub id: u64,
    pub items: Vec<CompactItem>,
    pub unresolved: bool,
}

impl ExchangeGroup {
    pub fn tokens(&self) -> usize {
        self.items.iter().map(|i| i.tokens).sum()
    }

    pub fn lane(&self) -> CompactLane {
        if self.unresolved || self.items.iter().any(|i| i.must_keep) {
            return CompactLane::RecentVerbatim;
        }
        self.items
            .iter()
            .map(|i| i.lane)
            .find(|l| *l != CompactLane::Checkpoint)
            .unwrap_or(CompactLane::OldHistory)
    }

    pub fn must_keep(&self) -> bool {
        self.unresolved || self.items.iter().any(|i| i.must_keep)
    }

    pub fn newest_log_id(&self) -> Option<i64> {
        self.items.iter().filter_map(|i| i.log_id).max()
    }
}

/// 圧縮の発火点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactPhase {
    TurnStart,
    MidTurn,
    TurnEnd,
}

/// 圧縮結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactOutcome {
    pub fired: bool,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub text: String,
    pub through_log_id: Option<i64>,
    pub checkpoint_empty: bool,
    pub low_water_unreachable: bool,
    pub exhausted: bool,
}

impl CompactOutcome {
    pub fn reduction(&self) -> usize {
        self.before_tokens.saturating_sub(self.after_tokens)
    }
}

/// `tokens > high` のときだけ刈る（ちょうど high は非発火）。
pub fn should_compact(tokens: usize, conversation_high: usize) -> bool {
    tokens > conversation_high
}

/// アイテム列を [`ExchangeGroup`] にまとめる。同じ `group_id` は原子単位。
pub fn group_items(items: &[CompactItem]) -> Vec<ExchangeGroup> {
    let mut groups: Vec<ExchangeGroup> = Vec::new();
    let mut by_id: std::collections::BTreeMap<u64, usize> = std::collections::BTreeMap::new();
    let mut next_id = items
        .iter()
        .filter_map(|i| i.group_id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    for item in items {
        if let Some(gid) = item.group_id {
            if let Some(&idx) = by_id.get(&gid) {
                groups[idx].items.push(item.clone());
                if item.lane == CompactLane::RecentVerbatim && item.must_keep {
                    groups[idx].unresolved = true;
                }
            } else {
                by_id.insert(gid, groups.len());
                groups.push(ExchangeGroup {
                    id: gid,
                    items: vec![item.clone()],
                    unresolved: item.must_keep && item.lane == CompactLane::RecentVerbatim,
                });
            }
        } else {
            groups.push(ExchangeGroup {
                id: next_id,
                items: vec![item.clone()],
                unresolved: false,
            });
            next_id += 1;
        }
    }
    groups
}

/// 高水位超過なら低水位まで刈る。超過していなければ入力をそのまま返す。
///
/// 刈る順は車線優先: 到達点 → 直近逐語 → エコー参照化 → 古い要約。
/// 新しい echo が古い逐語を押し出さない。[`ExchangeGroup`] はまとめて残すか落とす。
pub fn compact_to_low_water(
    items: &[CompactItem],
    checkpoint: &CheckpointLane,
    conversation_high: usize,
    conversation_low: usize,
) -> CompactOutcome {
    let mut ledger = TokenLedger::new();
    for item in items {
        ledger.record_tokens(&item.key, item.tokens);
    }
    let before = ledger.total();
    if !should_compact(before, conversation_high) {
        return CompactOutcome {
            fired: false,
            before_tokens: before,
            after_tokens: before,
            text: join_items(items, checkpoint, false),
            through_log_id: items.iter().rev().find_map(|i| i.log_id),
            checkpoint_empty: checkpoint.is_empty(),
            low_water_unreachable: false,
            exhausted: false,
        };
    }

    let cp_text = checkpoint.render();
    let cp_tokens = checkpoint.tokens();
    let checkpoint_empty = checkpoint.is_empty();

    if cp_tokens > conversation_high {
        return CompactOutcome {
            fired: true,
            before_tokens: before,
            after_tokens: cp_tokens,
            text: cp_text,
            through_log_id: items.iter().rev().find_map(|i| i.log_id),
            checkpoint_empty,
            low_water_unreachable: false,
            exhausted: true,
        };
    }

    let mut used = cp_tokens;
    let mut low_water_unreachable = false;
    if used > conversation_low {
        low_water_unreachable = true;
    }
    let mut remaining_budget = conversation_low.saturating_sub(used);

    let groups = group_items(items);
    let mut kept: Vec<CompactItem> = Vec::new();
    let mut dropped: Vec<&ExchangeGroup> = Vec::new();
    let mut claimed = std::collections::HashSet::<u64>::new();

    // 1. 直近逐語。must_keep（未決着 group / 直近ユーザー）を先に、残りを新しい順。
    take_lane(
        &groups,
        CompactLane::RecentVerbatim,
        true,
        &mut remaining_budget,
        &mut used,
        &mut kept,
        &mut claimed,
        &mut dropped,
        false,
    );
    take_lane(
        &groups,
        CompactLane::RecentVerbatim,
        false,
        &mut remaining_budget,
        &mut used,
        &mut kept,
        &mut claimed,
        &mut dropped,
        false,
    );

    // 2. エコー参照化。完了済み group を {ref,digest,bytes} にして新しい順。
    take_lane(
        &groups,
        CompactLane::Echoable,
        false,
        &mut remaining_budget,
        &mut used,
        &mut kept,
        &mut claimed,
        &mut dropped,
        true,
    );

    // 3. 古い履歴は要約だけ。残量があれば 1 件。
    for g in &groups {
        if claimed.contains(&g.id) {
            continue;
        }
        if g.lane() == CompactLane::Checkpoint {
            claimed.insert(g.id);
            continue;
        }
        dropped.push(g);
        claimed.insert(g.id);
    }
    if !dropped.is_empty() && remaining_budget > 0 {
        let summary = old_history_summary(dropped.len(), remaining_budget);
        if summary.tokens <= remaining_budget {
            used += summary.tokens;
            kept.insert(0, summary);
        }
    }

    kept.sort_by_key(|a| a.log_id);
    if used > conversation_low {
        low_water_unreachable = true;
    }

    let text = join_kept(&cp_text, checkpoint_empty, &kept);
    CompactOutcome {
        fired: true,
        before_tokens: before,
        after_tokens: used,
        text,
        through_log_id: items.iter().rev().find_map(|i| i.log_id),
        checkpoint_empty,
        low_water_unreachable,
        exhausted: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn take_lane<'a>(
    groups: &'a [ExchangeGroup],
    lane: CompactLane,
    must_keep_only: bool,
    remaining_budget: &mut usize,
    used: &mut usize,
    kept: &mut Vec<CompactItem>,
    claimed: &mut std::collections::HashSet<u64>,
    dropped: &mut Vec<&'a ExchangeGroup>,
    as_echo: bool,
) {
    let mut candidates: Vec<&ExchangeGroup> = groups
        .iter()
        .filter(|g| {
            !claimed.contains(&g.id)
                && g.lane() == lane
                && if must_keep_only {
                    g.must_keep()
                } else {
                    !g.must_keep()
                }
        })
        .collect();
    candidates.sort_by_key(|b| std::cmp::Reverse(b.newest_log_id()));
    for g in candidates {
        claimed.insert(g.id);
        if g.unresolved && as_echo {
            if g.tokens() <= *remaining_budget {
                *remaining_budget -= g.tokens();
                *used += g.tokens();
                kept.extend(g.items.iter().cloned());
            } else {
                dropped.push(g);
            }
            continue;
        }
        if as_echo {
            let echo = echo_group(g);
            if echo.tokens <= *remaining_budget {
                *remaining_budget -= echo.tokens;
                *used += echo.tokens;
                kept.push(echo);
            } else {
                dropped.push(g);
            }
        } else if g.must_keep() {
            // 直近ユーザー発話は残量が 0 でも落とさない。tool 予約が user 車線を
            // 食い潰したとき、質問本文まで消えるのを防ぐ。
            *remaining_budget = 0;
            *used += g.tokens();
            kept.extend(g.items.iter().cloned());
        } else if g.tokens() <= *remaining_budget {
            *remaining_budget -= g.tokens();
            *used += g.tokens();
            kept.extend(g.items.iter().cloned());
        } else {
            dropped.push(g);
        }
    }
}

fn echo_group(group: &ExchangeGroup) -> CompactItem {
    let body = group
        .items
        .iter()
        .map(|i| i.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let log_id = group.newest_log_id().unwrap_or(0);
    let text = argument_reference(log_id, &body);
    let tokens = crate::tokens::estimate_tokens(&text);
    CompactItem {
        key: format!("echo:group:{}", group.id),
        tokens,
        text,
        lane: CompactLane::Echoable,
        log_id: Some(log_id),
        must_keep: false,
        group_id: Some(group.id),
    }
}

fn argument_digest(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    hash.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn old_history_summary(dropped: usize, budget: usize) -> CompactItem {
    let text =
        format!("[old_history_summary] {dropped} older units omitted after higher-priority lanes");
    let mut tokens = crate::tokens::estimate_tokens(&text);
    if tokens > budget {
        tokens = budget;
    }
    CompactItem {
        key: "old_history_summary".into(),
        tokens,
        text,
        lane: CompactLane::OldHistory,
        log_id: None,
        must_keep: false,
        group_id: None,
    }
}

fn join_items(items: &[CompactItem], checkpoint: &CheckpointLane, inject: bool) -> String {
    let mut parts = Vec::new();
    if inject || !checkpoint.is_empty() {
        parts.push(checkpoint.render());
    } else if checkpoint.is_empty() {
        parts.push(CHECKPOINT_EMPTY_MARKER.to_string());
    }
    for item in items {
        if item.lane != CompactLane::Checkpoint {
            parts.push(item.text.clone());
        }
    }
    parts.join("\n")
}

fn join_kept(cp_text: &str, checkpoint_empty: bool, kept: &[CompactItem]) -> String {
    let mut parts = Vec::new();
    if checkpoint_empty {
        parts.push(CHECKPOINT_EMPTY_MARKER.to_string());
    } else {
        parts.push(cp_text.to_string());
    }
    for item in kept {
        parts.push(item.text.clone());
    }
    parts.join("\n")
}

/// 完了済み tool_call.arguments を `{ref,digest,bytes}` の JSON へ置換する。
pub fn argument_reference(log_id: i64, arguments: &str) -> String {
    let digest = argument_digest(arguments);
    serde_json::json!({
        "ref": format!("log:{log_id}"),
        "digest": digest,
        "bytes": arguments.len(),
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_budget::checkpoint::select_checkpoint_lane;

    fn item_at(key: &str, tokens: usize, lane: CompactLane, log_id: i64) -> CompactItem {
        CompactItem {
            key: key.into(),
            tokens,
            text: format!("[{key}:{tokens}]"),
            lane,
            log_id: Some(log_id),
            must_keep: false,
            group_id: None,
        }
    }

    #[test]
    fn high_exactly_does_not_fire_and_over_cuts_to_low() {
        let high = 45_000;
        let low = 20_000;
        let empty = select_checkpoint_lane(None, None);
        let at_high: Vec<CompactItem> = (0..45)
            .map(|i| item_at(&format!("h{i}"), 1_000, CompactLane::RecentVerbatim, i))
            .collect();
        let stay = compact_to_low_water(&at_high, &empty, high, low);
        assert!(!stay.fired, "45,000 ちょうどは非発火");
        assert_eq!(stay.before_tokens, 45_000);
        assert_eq!(stay.after_tokens, 45_000);

        let mut over = at_high;
        over.push(item_at("extra", 1, CompactLane::OldHistory, 45));
        let cut = compact_to_low_water(&over, &empty, high, low);
        assert!(cut.fired, "45,001 は発火");
        assert_eq!(cut.before_tokens, 45_001);
        assert!(
            cut.after_tokens <= 20_000,
            "after={} が低水位を超えた",
            cut.after_tokens
        );
        assert!(
            cut.reduction() >= 25_001,
            "reduction={} が足りない",
            cut.reduction()
        );
    }

    #[test]
    fn lane_priority_keeps_verbatim_over_newer_echo() {
        let empty = select_checkpoint_lane(None, None);
        let items = vec![
            item_at("old_said", 80, CompactLane::RecentVerbatim, 1),
            item_at("new_echo", 80, CompactLane::Echoable, 99),
        ];
        let out = compact_to_low_water(&items, &empty, 100, 90);
        assert!(out.fired);
        assert!(
            out.text.contains("[old_said:80]"),
            "直近逐語が先: {}",
            out.text
        );
        let echo_json: serde_json::Value = serde_json::from_str(
            out.text
                .lines()
                .find(|l| l.starts_with('{') && l.contains("\"ref\""))
                .unwrap_or("{}"),
        )
        .unwrap_or(serde_json::json!({}));
        if out.text.contains("[new_echo:80]") {
            panic!(
                "新しい echo 全文が古い逐語を押し出してはいけない: {}",
                out.text
            );
        }
        if echo_json.get("ref").is_some() {
            assert!(echo_json.get("digest").is_some());
            assert!(echo_json.get("bytes").is_some());
        }
    }

    #[test]
    fn exchange_group_is_atomic() {
        let empty = select_checkpoint_lane(None, None);
        let items = vec![
            CompactItem {
                key: "call".into(),
                tokens: 40,
                text: "[call]".into(),
                lane: CompactLane::Echoable,
                log_id: Some(1),
                must_keep: false,
                group_id: Some(7),
            },
            CompactItem {
                key: "result".into(),
                tokens: 40,
                text: "[result]".into(),
                lane: CompactLane::Echoable,
                log_id: Some(2),
                must_keep: false,
                group_id: Some(7),
            },
            item_at("keep_me", 50, CompactLane::RecentVerbatim, 10),
        ];
        let out = compact_to_low_water(&items, &empty, 80, 60);
        assert!(out.fired);
        let has_call = out.text.contains("[call]");
        let has_result = out.text.contains("[result]");
        assert_eq!(
            has_call, has_result,
            "group の片側だけ残ってはいけない: {}",
            out.text
        );
        assert!(out.text.contains("[keep_me:50]"), "{}", out.text);
    }

    #[test]
    fn echo_is_valid_ref_digest_bytes_json() {
        let empty = select_checkpoint_lane(None, None);
        let items = vec![item_at("tool", 5_000, CompactLane::Echoable, 12)];
        let out = compact_to_low_water(&items, &empty, 100, 80);
        assert!(out.fired);
        let json_line = out
            .text
            .lines()
            .find(|l| l.starts_with('{'))
            .expect("echo JSON");
        let v: serde_json::Value = serde_json::from_str(json_line).unwrap();
        assert!(v
            .get("ref")
            .and_then(|x| x.as_str())
            .unwrap()
            .contains("log:"));
        assert!(v.get("digest").is_some());
        assert!(v.get("bytes").is_some());
    }

    /// user 車線の残量が 0 でも must_keep（直近ユーザー発話）は残る。
    #[test]
    fn must_keep_survives_zero_remaining_budget() {
        let empty = select_checkpoint_lane(None, None);
        let items = vec![
            CompactItem {
                key: "origin".into(),
                tokens: 20,
                text: "[owner]: 東京！".into(),
                lane: CompactLane::RecentVerbatim,
                log_id: Some(1),
                must_keep: true,
                group_id: Some(1),
            },
            item_at("old", 80, CompactLane::OldHistory, 0),
        ];
        let out = compact_to_low_water(&items, &empty, 10, 0);
        assert!(out.fired);
        assert!(
            out.text.contains("東京！"),
            "残量 0 でも発端は残る: {}",
            out.text
        );
        assert!(out.low_water_unreachable);
    }
}
