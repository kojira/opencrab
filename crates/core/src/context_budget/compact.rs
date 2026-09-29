//! 二水位圧縮（#826-B）。
//!
//! 高水位超過でだけ刈り、低水位まで落とす。車線は字句順の優先:
//! 直近逐語 → エコー参照化 → 古い履歴の要約。
//! 合計は [`crate::context_budget::TokenLedger`] の加減算だけを使い、全文再 encode しない。
//! assistant の said と同一応答の tool calls / results は [`ExchangeGroup`] として原子的に扱う。

use sha2::{Digest, Sha256};

use super::ledger::TokenLedger;

/// 圧縮車線。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactLane {
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
            .first()
            .map(|i| i.lane)
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

/// 圧縮しても必ず逐語で残す直近の会話単位数（#1049）。
///
/// 話者や種別で抜き出さず、最新から連続した履歴を残す。抜き出しは文脈を欠き、
/// 完了結果や自分の発話が消えて何に応えるのか分からなくなる。
pub const MIN_RECENT_ITEMS: usize = 20;

/// 高水位超過なら刈る。橋渡しの要約なし版（途中圧縮など log id を持たない経路）。
pub fn compact_to_low_water(
    items: &[CompactItem],
    conversation_high: usize,
    conversation_low: usize,
) -> CompactOutcome {
    compact_with_bridge(items, conversation_high, conversation_low, &[])
}

/// 高水位超過なら刈る。超過していなければ入力をそのまま返す。
///
/// 最新から連続して積む（#1049）:
/// 1. 直近 [`MIN_RECENT_ITEMS`] 件は予算にかかわらず逐語で残す。
/// 2. それより前は低水位まで逐語で積む。入らない完了済みツール組は参照化して続ける。
/// 3. 低水位を越えても、トピック要約がまだ覆っていない区間は高水位（許容マージン）まで逐語で残す
///    （要約との間に隙間を作らない）。
/// 4. 逐語で残せなかった区間は、トピック要約（`bridge`）を新しい順に高水位まで入れてつなぐ。
///    それより古い部分は [Memory Index]（宣言ユニット・月次要約）が担う。
///
/// 未決着のツール組は API 対のため必ず残す。[`ExchangeGroup`] はまとめて残すか落とす。
pub fn compact_with_bridge(
    items: &[CompactItem],
    conversation_high: usize,
    conversation_low: usize,
    bridge: &[super::bridge::BridgeLine],
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
            text: join_items(items),
            through_log_id: items.iter().rev().find_map(|i| i.log_id),
            low_water_unreachable: false,
            exhausted: false,
        };
    }

    let groups = group_items(items);
    let mut order: Vec<&ExchangeGroup> = groups.iter().collect();
    order.sort_by_key(|g| std::cmp::Reverse(g.newest_log_id()));

    let mut used = 0usize;
    let mut kept_items = 0usize;
    let mut kept: Vec<CompactItem> = Vec::new();
    let mut oldest_verbatim: Option<i64> = None;
    let mut cut = false;
    let mut gap = false;
    for g in order {
        if cut {
            if g.unresolved {
                used += g.tokens();
                kept.extend(g.items.iter().cloned());
            }
            continue;
        }
        let tokens = g.tokens();
        let newest = g.newest_log_id().unwrap_or(0);
        if kept_items < MIN_RECENT_ITEMS || g.unresolved || used + tokens <= conversation_low {
            used += tokens;
            kept_items += g.items.len();
            kept.extend(g.items.iter().cloned());
            oldest_verbatim = g
                .items
                .iter()
                .filter_map(|i| i.log_id)
                .min()
                .or(oldest_verbatim);
            continue;
        }
        if g.items.iter().any(|i| i.lane == CompactLane::Echoable) {
            let echo = echo_group(g);
            if used + echo.tokens <= conversation_low {
                used += echo.tokens;
                kept_items += 1;
                kept.push(echo);
                oldest_verbatim = g
                    .items
                    .iter()
                    .filter_map(|i| i.log_id)
                    .min()
                    .or(oldest_verbatim);
                continue;
            }
        }
        // 許容マージン: 要約が覆っていない区間、または境目の 1 組は高水位まで逐語で残す。
        if used + tokens <= conversation_high {
            used += tokens;
            kept.extend(g.items.iter().cloned());
            oldest_verbatim = g
                .items
                .iter()
                .filter_map(|i| i.log_id)
                .min()
                .or(oldest_verbatim);
            if !bridge.is_empty() && !super::bridge::is_covered(bridge, newest) {
                continue;
            }
        } else if !bridge.is_empty() && !super::bridge::is_covered(bridge, newest) {
            gap = true;
        }
        cut = true;
    }
    if cut {
        // 古い側から: [Memory Index] への案内（隙間があるときだけ）→ トピック要約 → 逐語。
        if gap || bridge.is_empty() {
            let note = older_history_note();
            used += note.tokens;
            kept.push(note);
        }
        let boundary = oldest_verbatim.unwrap_or(i64::MAX);
        let room = conversation_high.saturating_sub(used);
        if let Some((text, tokens)) = super::bridge::render_bridge(bridge, boundary, room) {
            used += tokens;
            kept.push(CompactItem {
                key: "bridge".into(),
                tokens,
                text,
                lane: CompactLane::OldHistory,
                log_id: None,
                must_keep: false,
                group_id: None,
            });
        }
    }

    kept.sort_by_key(|a| a.log_id);
    let low_water_unreachable = used > conversation_high;

    CompactOutcome {
        fired: true,
        before_tokens: before,
        after_tokens: used,
        text: join_kept(&kept),
        through_log_id: items.iter().rev().find_map(|i| i.log_id),
        low_water_unreachable,
        exhausted: false,
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

/// トピック要約が用意できない（途中圧縮・索引未作成）ときだけ、古い履歴の在りかを示す。
fn older_history_note() -> CompactItem {
    let text = "[Earlier conversation is in your memory: see [Memory Index], search_memory_index / retrieve_memory_nodes]".to_string();
    let tokens = crate::tokens::estimate_tokens(&text);
    CompactItem {
        key: "older_history_note".into(),
        tokens,
        text,
        lane: CompactLane::OldHistory,
        log_id: None,
        must_keep: false,
        group_id: None,
    }
}

fn join_items(items: &[CompactItem]) -> String {
    items
        .iter()
        .map(|item| item.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

fn join_kept(kept: &[CompactItem]) -> String {
    kept.iter()
        .map(|item| item.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
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
    fn high_exactly_does_not_fire_and_over_cuts_within_margin() {
        let high = 45_000;
        let low = 20_000;
        let at_high: Vec<CompactItem> = (0..45)
            .map(|i| item_at(&format!("h{i}"), 1_000, CompactLane::RecentVerbatim, i))
            .collect();
        let stay = compact_to_low_water(&at_high, high, low);
        assert!(!stay.fired, "45,000 ちょうどは非発火");
        assert_eq!(stay.after_tokens, 45_000);

        let mut over = at_high;
        over.push(item_at("extra", 1, CompactLane::OldHistory, 45));
        let cut = compact_to_low_water(&over, high, low);
        assert!(cut.fired, "45,001 は発火");
        assert!(
            cut.after_tokens > low,
            "低水位を越える境目はマージン内で残す"
        );
        assert!(cut.after_tokens <= high, "after={}", cut.after_tokens);
        assert!(cut.text.contains("[extra:1]") && cut.text.contains("[h44:1000]"));
        assert!(!cut.text.contains("[h0:1000]"), "古い側から省略する");
        assert!(cut.text.contains("[Earlier conversation"));
    }

    /// 逐語で残せない区間はトピック要約でつなぐ。要約が覆っていない区間は
    /// マージンまで逐語で残し、要約と逐語の間に隙間を作らない（#1049）。
    #[test]
    fn topic_bridge_fills_older_history_without_gap() {
        let items: Vec<CompactItem> = (0..60)
            .map(|i| item_at(&format!("m{i}"), 100, CompactLane::RecentVerbatim, i))
            .collect();
        let line = |a: i64, b: i64| super::super::bridge::BridgeLine {
            start_log_id: a,
            end_log_id: b,
            text: format!("- [t{a}-{b}] summary"),
            tokens: 5,
        };
        // 要約は log 0..=34 まで（35 以降は索引が追いついていない）。
        let bridge = vec![line(0, 9), line(10, 19), line(20, 29), line(30, 34)];
        let out = compact_with_bridge(&items, 5_000, 2_000, &bridge);
        assert!(out.fired);
        // 直近 20 + 低水位 = m40..m59 が逐語。未要約の m35..m39 もマージンで逐語に残る。
        for i in 35..60 {
            assert!(
                out.text.contains(&format!("[m{i}:100]")),
                "m{i} missing: {}",
                out.text
            );
        }
        assert!(
            out.text.contains("[t30-34]") && out.text.contains("[t0-9]"),
            "{}",
            out.text
        );
        assert!(
            !out.text.contains("[Earlier conversation is in your memory"),
            "隙間なし: {}",
            out.text
        );
        let summary_at = out.text.find("[t30-34]").unwrap();
        let verbatim_at = out.text.find("[m35:100]").unwrap();
        assert!(summary_at < verbatim_at, "古い要約 → 新しい逐語の順");
        assert!(out.after_tokens <= 5_000);
    }

    /// 直近 20 件は種別・話者によらず連続で逐語に残す（#1049）。
    #[test]
    fn newest_twenty_are_kept_contiguously_regardless_of_budget() {
        let mut items: Vec<CompactItem> = (0..30)
            .map(|i| item_at(&format!("n{i}"), 100, CompactLane::OldHistory, i))
            .collect();
        items[29].lane = CompactLane::Echoable;
        let out = compact_to_low_water(&items, 500, 200);
        assert!(out.fired);
        for i in 10..30 {
            assert!(
                out.text.contains(&format!("[n{i}:100]")),
                "n{i}: {}",
                out.text
            );
        }
        assert!(!out.text.contains("[n9:100]"), "{}", out.text);
        assert!(
            out.text.starts_with("[Earlier conversation"),
            "{}",
            out.text
        );
    }

    #[test]
    fn exchange_group_is_atomic() {
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
        let out = compact_to_low_water(&items, 80, 60);
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
        let mut items = vec![item_at("tool", 5_000, CompactLane::Echoable, 0)];
        items.extend((1..=20).map(|i| item_at(&format!("r{i}"), 1, CompactLane::OldHistory, i)));
        let out = compact_to_low_water(&items, 100, 80);
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

    /// サブタスク完了など、ユーザー発言より新しい履歴が消えない（#1049）。
    #[test]
    fn completion_newer_than_user_speech_is_kept() {
        let mut items: Vec<CompactItem> = (0..30)
            .map(|i| item_at("old", 5_000, CompactLane::OldHistory, i))
            .collect();
        items.push(item_at("owner_speech", 20, CompactLane::RecentVerbatim, 30));
        items.push(item_at("my_ack", 20, CompactLane::RecentVerbatim, 31));
        items.push(item_at(
            "subtask_completed",
            30,
            CompactLane::RecentVerbatim,
            32,
        ));
        let out = compact_to_low_water(&items, 100, 60);
        assert!(out.fired);
        for k in ["owner_speech", "my_ack", "subtask_completed"] {
            assert!(out.text.contains(k), "{k}: {}", out.text);
        }
        assert_eq!(out.text.matches("[old:5000]").count(), 17, "{}", out.text);
    }

    /// user 車線の残量が 0 でも must_keep（直近ユーザー発話）は残る。
    #[test]
    fn must_keep_survives_zero_remaining_budget() {
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
        let out = compact_to_low_water(&items, 10, 0);
        assert!(out.fired);
        assert!(
            out.text.contains("東京！"),
            "残量 0 でも発端は残る: {}",
            out.text
        );
        assert!(out.low_water_unreachable);
    }
}
