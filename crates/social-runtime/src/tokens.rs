//! トークン計数（tiktoken `o200k_base`）。文脈予算の物差し（`TokenCounter`）の本番実装。
//!
//! opencrab 本体（`crates/core/src/tokens.rs`）と**同じ物差し**を使う——会話予算・記憶索引
//! 予算・文脈の観測（§06/§10）を o200k 見積りで数える。予算は soft limit なので近似で足りる。
//! 本体と単位が揃うので、将来この budgeting を本体へ還流するとき両者の予算計算が合流できる。
//!
//! o200k は OpenAI 系のトークナイザだが、Anthropic 系（hermit haiku 等）でも予算の近似として
//! 妥当——本体 #562 の実測で「バックエンド実 prompt_tokens ÷ o200k = 中央 1.06・最大 1.12、
//! しかもその差はトークナイザ違いではなく API 構造オーバーヘッド」と分かっている。厳密さは
//! 予算には要らない。厳密なプロバイダ別トークナイザが要る日が来たら `TokenCounter` を差し替える。

use opencrab_port::TokenCounter;
use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// プロセス内で 1 度だけ構築する tokenizer。`o200k_base` のロードは BPE テーブルの構築を
/// 伴い数十 ms 級なので、数えるたびに作り直さず `OnceLock` で共有する（本体と同じ）。
fn tokenizer() -> &'static CoreBPE {
    static TOKENIZER: OnceLock<CoreBPE> = OnceLock::new();
    TOKENIZER
        .get_or_init(|| tiktoken_rs::o200k_base().expect("failed to load o200k_base tokenizer"))
}

/// 文字列の正確なトークン数（tiktoken o200k_base）。
fn estimate_tokens(s: &str) -> usize {
    tokenizer().encode_with_special_tokens(s).len()
}

/// 1 回の encode に渡す入力の上限バイト数（本体 tokens.rs の `BOUNDED_TOKENIZE_WINDOW` と同値・2 KiB）。
///
/// BPE の `byte_pair_merge` は 1 個の pre-token（空白・記号で区切られない連続）に対して最悪
/// O(バイト²) 近い。巨大入力（本番実績 486MB）や長い単一文字ラン（`a`×300,000 等）を一括で encode
/// すると同期 CPU を大量に消費し GB 級のメモリを確保しうる。入力をこの窓で刻んでから encode すれば
/// **1 回の encode コストが窓サイズで頭打ち**になり、全体サイズにも単一 pre-token の長さにも依存
/// しない（本体 #576 と同型）。値は本体に揃える。
const BOUNDED_TOKENIZE_WINDOW: usize = 2 * 1024;

/// `TokenCounter` の本番実装（o200k 見積り）。フォールバックは持たない——ロードに失敗したら
/// 起動時に `expect` で fail loud させる（数えられないのに文字数へ黙って戻す、をしない）。
#[derive(Clone, Copy, Debug, Default)]
pub struct O200kCounter;

impl O200kCounter {
    pub fn new() -> O200kCounter {
        O200kCounter
    }
}

impl TokenCounter for O200kCounter {
    fn count(&self, s: &str) -> usize {
        estimate_tokens(s)
    }

    /// トークン数が `limit` 以上かを、**全体をトークナイズせずに**判定する（本体 `tokens_reach_limit`
    /// と同型・#576）。先頭から [`BOUNDED_TOKENIZE_WINDOW`] バイトずつ（文字境界へ丸め）encode し、
    /// 累計が `limit` に達した時点で `true` を返して打ち切る。コストは「`limit` トークンぶんの入力＋
    /// 端数 1 窓」で頭打ちになり、`s` 全体のサイズにも単一 pre-token の長さにも依存しない。
    ///
    /// **false negative が無い**: 窓境界で BPE のマージが分断されるため累計は本来のトークン数を
    /// わずかに上回りうる（境界ごとに高々数トークン）が、下回ることはない。真に `limit` 以上の入力を
    /// 「未満」と取りこぼさない。退避判定はこの bool しか使わず、退避されない側の producer は上限に
    /// 余白を持つので、境界での上振れは無害。
    fn count_reaches(&self, s: &str, limit: usize) -> bool {
        if limit == 0 {
            return true;
        }
        let mut total = 0usize;
        let mut start = 0usize;
        while start < s.len() {
            let mut end = (start + BOUNDED_TOKENIZE_WINDOW).min(s.len());
            // 窓の末尾がマルチバイト文字を割らないよう、次の文字境界まで伸ばす（`s.len()` は常に
            // 境界なので、この分岐に入るのは末尾未満のときだけ＝必ず前進する）。
            while end < s.len() && !s.is_char_boundary(end) {
                end += 1;
            }
            total += estimate_tokens(&s[start..end]);
            if total >= limit {
                return true;
            }
            start = end;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// トークン数はバイト数を超えない（1 トークンは 1 バイト以上）。
    #[test]
    fn tokens_never_exceed_bytes() {
        for s in [
            "",
            "hello world",
            "日本語のテキストです。",
            r#"{"success":true,"data":{"list":[1,2,3]}}"#,
            &"あ".repeat(200),
        ] {
            assert!(O200kCounter.count(s) <= s.len(), "token > byte for {s:?}");
        }
    }

    /// 非空の文字列は 1 トークン以上——記憶索引のフェッチ上限（1 行 ≥ 1 トークン）が依存する性質。
    #[test]
    fn non_empty_has_at_least_one_token() {
        assert_eq!(O200kCounter.count(""), 0);
        for s in ["a", "あ", "- #1 x\n"] {
            assert!(O200kCounter.count(s) >= 1, "empty count for {s:?}");
        }
    }

    /// golden: 具体トークン数を固定する。狙いは「別 vocab へすり替わった／エンコーディングを
    /// 取り違えた（cl100k 混入等）」の検知——性質テスト（≤バイト・非空≥1）は通ってしまうので、
    /// 実値そのものを打っておく。値は o200k_base での実測（値が変わったら vocab を疑う）。
    #[test]
    fn golden_token_counts_pin_o200k() {
        let c = O200kCounter::new();
        assert_eq!(c.count("hello world"), 2);
        assert_eq!(c.count("The quick brown fox."), 5);
        // 日本語も 1 ケース固定（マルチバイトのエンコーディング取り違えを捕まえる）。
        assert_eq!(c.count("金曜日"), 2);
        assert_eq!(c.count("こんにちは、世界"), 3);
    }

    /// count_reaches（有界判定）は exact な count と**同じ側**に判定する（false negative 無し）。
    /// 複数窓を跨ぐ入力でも「以上/未満」が一致することを、CJK・空白区切りで固定する。
    #[test]
    fn count_reaches_matches_exact_side() {
        let c = O200kCounter::new();
        let cases: &[String] = &[
            String::new(),
            "hello world".into(),
            "日本語のテキストです。".into(),
            "a ".repeat(5_000),
            "あ".repeat(5_000),
            "needle ".repeat(3_000),
        ];
        for s in cases {
            let exact = c.count(s);
            for &limit in &[1usize, 100, 2_500, 10_000] {
                assert_eq!(
                    c.count_reaches(s, limit),
                    exact >= limit,
                    "mismatch limit={limit} exact={exact} len={}",
                    s.len()
                );
            }
        }
    }

    /// 端: limit=0 は常に true、空文字は 1 に達しない。
    #[test]
    fn count_reaches_edges() {
        let c = O200kCounter::new();
        assert!(c.count_reaches("", 0));
        assert!(c.count_reaches("anything", 0));
        assert!(!c.count_reaches("", 1));
    }

    /// 区切りの無い長い単一ラン（1 pre-token）でも即座に返る＝全体を encode しない（#576 同型）。
    /// panic せず bool を返せば十分（有界性の証明——O(n²)/OOM なら完走しない）。
    #[test]
    fn count_reaches_returns_on_long_single_run() {
        let c = O200kCounter::new();
        for s in [&"a".repeat(300_000), &"あ".repeat(100_000)] {
            assert!(c.count_reaches(s, 2_500), "len={}", s.len());
        }
    }
}
