//! アイテム毎に 1 回計測してキャッシュし、総量は加減算で維持する（#826-A）。
//!
//! 巨大アイテムの上限判定は既存 [`crate::tokens::tokens_reach_limit`]（2KiB 窓）。
//! append のたびに全文を再 encode しない（O(n²) 禁止）。

use crate::tokens::{measure_item_tokens, tokens_reach_limit};

/// 1 件分のキャッシュ済み token 数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerItem {
    pub key: String,
    pub tokens: usize,
}

/// 費目別 token 台帳。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenLedger {
    items: Vec<LedgerItem>,
    total: usize,
}

impl TokenLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn items(&self) -> &[LedgerItem] {
        &self.items
    }

    /// 本文を 1 回測ってキャッシュし、合計へ加算する。
    pub fn record(&mut self, key: impl Into<String>, text: &str) -> usize {
        self.record_tokens(key, measure_item_tokens(text))
    }

    /// 既に測った token 数をキャッシュする。同一キーは上書きし、合計を差し替えて二重加算しない。
    pub fn record_tokens(&mut self, key: impl Into<String>, tokens: usize) -> usize {
        let key = key.into();
        if let Some(item) = self.items.iter_mut().find(|item| item.key == key) {
            self.total = self
                .total
                .saturating_sub(item.tokens)
                .saturating_add(tokens);
            item.tokens = tokens;
            return tokens;
        }
        self.total = self.total.saturating_add(tokens);
        self.items.push(LedgerItem { key, tokens });
        tokens
    }

    /// キー一致の最初の 1 件を外し、合計から減算する。
    pub fn remove_key(&mut self, key: &str) -> Option<usize> {
        let idx = self.items.iter().position(|item| item.key == key)?;
        let item = self.items.remove(idx);
        self.total = self.total.saturating_sub(item.tokens);
        Some(item.tokens)
    }

    /// 巨大アイテムの上限判定。全文 encode しない。
    pub fn text_reaches_limit(text: &str, limit: usize) -> bool {
        tokens_reach_limit(text, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{estimate_tokens, BOUNDED_TOKENIZE_WINDOW};

    #[test]
    fn record_measures_once_and_total_is_sum() {
        let mut ledger = TokenLedger::new();
        let a = "hello world";
        let b = "日本語の一文";
        let ta = ledger.record("a", a);
        let tb = ledger.record("b", b);
        assert_eq!(ta, estimate_tokens(a));
        assert_eq!(tb, estimate_tokens(b));
        assert_eq!(ledger.total(), ta + tb);
        assert_eq!(ledger.remove_key("a"), Some(ta));
        assert_eq!(ledger.total(), tb);
        assert_eq!(ledger.remove_key("missing"), None);
        assert_eq!(ledger.total(), tb);
    }

    #[test]
    fn record_same_key_replaces_instead_of_double_count() {
        let mut ledger = TokenLedger::new();
        ledger.record_tokens("a", 10);
        ledger.record_tokens("a", 7);
        assert_eq!(ledger.total(), 7);
        assert_eq!(ledger.items().len(), 1);
        assert_eq!(ledger.items()[0].tokens, 7);
        ledger.record_tokens("b", 3);
        ledger.record_tokens("a", 4);
        assert_eq!(ledger.total(), 7);
        assert_eq!(ledger.items().len(), 2);
    }

    #[test]
    fn huge_item_limit_uses_windowed_predicate() {
        let huge = "word ".repeat(20_000);
        assert!(huge.len() > BOUNDED_TOKENIZE_WINDOW);
        assert!(TokenLedger::text_reaches_limit(&huge, 2_500));
        assert!(!TokenLedger::text_reaches_limit("short", 2_500));
        let mut ledger = TokenLedger::new();
        let tokens = ledger.record("huge", &huge);
        assert!(tokens >= 2_500);
        assert_eq!(ledger.total(), tokens);
    }

    #[test]
    fn totals_match_add_subtract_property() {
        let mut seed = 7_u64;
        let mut next = || {
            seed = seed.wrapping_mul(0x5851F42D4C957F2D).wrapping_add(1);
            (seed % 4_000) as usize
        };
        let mut ledger = TokenLedger::new();
        let mut expected = 0usize;
        for i in 0..64 {
            let tokens = next();
            ledger.record_tokens(format!("k{i}"), tokens);
            expected = expected.saturating_add(tokens);
            assert_eq!(ledger.total(), expected);
        }
        for i in (0..64).step_by(3) {
            if let Some(removed) = ledger.remove_key(&format!("k{i}")) {
                expected = expected.saturating_sub(removed);
            }
            assert_eq!(ledger.total(), expected);
        }
        assert_eq!(
            ledger.total(),
            ledger.items().iter().map(|i| i.tokens).sum::<usize>()
        );
    }
}
