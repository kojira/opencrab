//! 二水位圧縮（#826-B）。
//!
//! 高水位超過でだけ刈り、低水位まで落とす。車線は字句順の優先:
//! 到達点チェックポイント不可侵 → 直近逐語 → エコー参照化 → 古い履歴の要約。
//! 合計は [`crate::context_budget::TokenLedger`] の加減算だけを使い、全文再 encode しない。

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

/// 高水位超過なら低水位まで刈る。超過していなければ入力をそのまま返す。
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

    let mut kept: Vec<CompactItem> = Vec::new();
    let mut used = cp_tokens;
    let mut low_water_unreachable = false;

    if used > conversation_low {
        low_water_unreachable = true;
    }

    let mut remaining_budget = conversation_low.saturating_sub(used);
    let mut dropped: Vec<&CompactItem> = Vec::new();

    // 直近逐語のうち must_keep（直近ユーザー発言）を先に残し、残りを新しい順で埋める。
    let mut claimed = std::collections::HashSet::<String>::new();
    for item in items.iter().rev() {
        if item.lane != CompactLane::RecentVerbatim || !item.must_keep {
            continue;
        }
        if item.tokens <= remaining_budget {
            kept.push(item.clone());
            remaining_budget -= item.tokens;
            used += item.tokens;
            claimed.insert(item.key.clone());
        }
    }
    for item in items.iter().rev() {
        if item.lane == CompactLane::Checkpoint || claimed.contains(&item.key) {
            continue;
        }
        if item.lane == CompactLane::RecentVerbatim && item.tokens <= remaining_budget {
            kept.push(item.clone());
            remaining_budget -= item.tokens;
            used += item.tokens;
            claimed.insert(item.key.clone());
        } else if item.lane == CompactLane::Echoable {
            let echo = echo_item(item);
            if echo.tokens <= remaining_budget {
                remaining_budget -= echo.tokens;
                used += echo.tokens;
                kept.push(echo);
            } else {
                dropped.push(item);
            }
        } else {
            dropped.push(item);
        }
    }
    kept.sort_by(|a, b| a.log_id.cmp(&b.log_id));

    if !dropped.is_empty() && remaining_budget > 0 {
        let summary = old_history_summary(dropped.len(), remaining_budget);
        if summary.tokens <= remaining_budget {
            used += summary.tokens;
            kept.insert(0, summary);
        }
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

fn echo_item(item: &CompactItem) -> CompactItem {
    let digest = argument_digest(&item.text);
    let bytes = item.text.len();
    let text = format!(
        "[echo] key={} digest={} bytes={} tokens={}",
        item.key, digest, bytes, item.tokens
    );
    let tokens = crate::tokens::estimate_tokens(&text);
    CompactItem {
        key: format!("echo:{}", item.key),
        tokens,
        text,
        lane: CompactLane::Echoable,
        log_id: item.log_id,
        must_keep: false,
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

    fn item(key: &str, tokens: usize, lane: CompactLane) -> CompactItem {
        CompactItem {
            key: key.into(),
            tokens,
            text: format!("[{key}:{tokens}]"),
            lane,
            log_id: None,
            must_keep: false,
        }
    }

    #[test]
    fn high_exactly_does_not_fire_and_over_cuts_to_low() {
        let high = 45_000;
        let low = 20_000;
        let empty = select_checkpoint_lane(None, None);
        let at_high: Vec<CompactItem> = (0..45)
            .map(|i| item(&format!("h{i}"), 1_000, CompactLane::RecentVerbatim))
            .collect();
        let stay = compact_to_low_water(&at_high, &empty, high, low);
        assert!(!stay.fired, "45,000 ちょうどは非発火");
        assert_eq!(stay.before_tokens, 45_000);
        assert_eq!(stay.after_tokens, 45_000);

        let mut over = at_high;
        over.push(item("extra", 1, CompactLane::OldHistory));
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
}
