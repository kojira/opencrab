//! トークン計算（tiktoken `o200k_base`）。
//!
//! コンテキスト予算を食うものは**すべて同じ物差しで測る**。会話履歴のコンパクション
//! （`build_conversation_string`）も、tool_result の上限（[`crate::tool_result_log`]）も
//! ここを通す。片方をバイト、片方をトークンで判定すると、同じ 10KB でも中身が日本語か
//! 英数字か base64 かで実効トークン量が数倍ぶれ、「予算内のはずが溢れる／まだ余裕が
//! あるのに切る」が起きる（#294）。
//!
//! 元は `crates/server/src/process.rs` の private fn だったが、core の
//! `tool_result_log` からも必要になったため core へ移設した（依存方向は server → core
//! なので逆は書けない）。

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

/// プロセス内で 1 度だけ構築する tokenizer。
///
/// `o200k_base` のロードは BPE テーブルの構築を伴い数十 ms 級。tool_result ごとに
/// 作り直すと目に見えて遅くなるため `OnceLock` で共有する。
fn get_tokenizer() -> &'static CoreBPE {
    static TOKENIZER: OnceLock<CoreBPE> = OnceLock::new();
    TOKENIZER
        .get_or_init(|| tiktoken_rs::o200k_base().expect("failed to load o200k_base tokenizer"))
}

/// 文字列の正確なトークン数を返す (tiktoken o200k_base)。
pub fn estimate_tokens(s: &str) -> usize {
    get_tokenizer().encode_with_special_tokens(s).len()
}

/// 1 回の encode に渡す入力の上限バイト数（#576）。
///
/// BPE の `byte_pair_merge` は 1 個の pre-token（空白・記号で区切られない連続）に対して
/// 最悪 O(バイト²) 近い。巨大入力（本番実績 486MB）や長い単一文字ラン（`a`×300,000 等）を
/// 一括で encode すると同期 CPU を大量に消費する（本番 486MB は 5 分でも終わらない実測）。
/// 入力をこの窓で刻んでから encode すれば、**1 回の encode コストが窓サイズで頭打ち**になり、
/// 全体サイズにも単一 pre-token の長さにも依存しなくなる。
///
/// 値は 2 KiB。O(バイト²) なので窓を小さくするほど最悪ケースの 1 回 encode が軽くなる一方、
/// [`tokens_reach_limit`] は上限に達するまで窓を積むので実運用のコストは変わらない
/// （区切りのある実データは先頭数窓で早期打ち切り）。実測（median, PR 本文）:
/// - 実データ相当（`"word "`×100k, 500KB）: **~10ms**（窓サイズにほぼ非依存＝早期打ち切り）。
/// - 病的な単一ラン（`あ`×100k / `a`×300k, 300KB）: 2 KiB で **~40 / ~120ms**。
///   16 KiB だと ~1s まで伸びるため小さめに採る。本番でこの手の長大ランは未発火
///   （最長で 647 字）なので、実効は前者。
///
/// 窓を跨ぐと BPE のマージが分断され累計トークンが**わずかに上振れ**しうるが、CJK や空白
/// 区切りのトークンは窓境界を跨がないので実質ゼロ、上振れが出るのは base64/単一文字の
/// 長大ランのみ（窓ごとに高々数トークン）。判定の安全側なので producer の余白で吸収される
/// （[`tokens_reach_limit`] の false-negative 無し保証を参照）。
pub const BOUNDED_TOKENIZE_WINDOW: usize = 2 * 1024;

/// `s` のトークン数が `limit` **以上**か否かを、**全体をトークナイズせずに**判定する（#576）。
///
/// 先頭から [`BOUNDED_TOKENIZE_WINDOW`] バイトずつ（文字境界へ丸め）encode し、累計が
/// `limit` に達した時点で `true` を返して打ち切る。コストは「`limit` トークンぶんの入力＋
/// 端数 1 窓」で頭打ちになり、`s` 全体のサイズにも単一 pre-token の長さにも依存しない。
///
/// **false negative が無いこと**が肝心: チャンク境界で BPE のマージが分断されるため累計は
/// 本来のトークン数を**わずかに上回りうる**（境界ごとに高々数トークン）が、逆に下回ることは
/// ない（本来のトークン数 ≤ 累計）。したがって真に `limit` 以上の入力を「未満」と取りこぼす
/// ことはない。退避判定はこの bool しか使わず、退避されない側に載せる producer は上限に対し
/// 余白（`RANGE_CONTENT_TOKEN_CEILING` の `-400` 等）を持つので、境界での上振れは無害。
///
/// **窓サイズはトレードオフ**: 窓を小さくすると 1 回の encode は軽くなるが、跨ぐ境界の数が
/// 増えて上振れも増える。[`BOUNDED_TOKENIZE_WINDOW`]（2 KiB）は、その上振れが producer の
/// 余白（400〜500 トークン）に収まる範囲で選んでいる。
///
/// 表示用の「約 N トークン」など**数を返したい**用途には [`estimate_tokens_bounded`] を使う。
pub fn tokens_reach_limit(s: &str, limit: usize) -> bool {
    if limit == 0 {
        return true;
    }
    let mut total = 0usize;
    let mut start = 0usize;
    while start < s.len() {
        let mut end = (start + BOUNDED_TOKENIZE_WINDOW).min(s.len());
        // 窓の末尾がマルチバイト文字を割らないよう、次の文字境界まで伸ばす（`s.len()` は
        // 常に境界なので、この分岐に入るのは末尾未満のときだけ＝必ず前進する）。
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

/// 巨大入力でトークナイズが O(n²) 級に膨らむのを避けるための、**全体トークン数の概算**（#576）。
///
/// `s` が [`BOUNDED_TOKENIZE_WINDOW`] 以下なら正確な [`estimate_tokens`]。超えるなら先頭
/// [`BOUNDED_TOKENIZE_WINDOW`] バイト（文字境界へ丸め）だけを encode し、そのトークン密度で
/// 全体を線形スケールする。1 回の encode で済むので巨大入力でも軽い。
///
/// 用途は**表示・見積り**（退避案内の「約 N トークン」／`ws_read` の `estimated_tokens`）。
/// 先頭サンプルが全体を代表する前提の概算なので、**判定（yes/no）には使わない** — 先頭が
/// 疎で末尾が密な入力を過小評価しうる。判定は取りこぼしの無い [`tokens_reach_limit`] を使う。
pub fn estimate_tokens_bounded(s: &str) -> usize {
    let total = s.len();
    if total <= BOUNDED_TOKENIZE_WINDOW {
        return estimate_tokens(s);
    }
    let mut cut = BOUNDED_TOKENIZE_WINDOW;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let sample = estimate_tokens(&s[..cut]);
    ((sample as u128 * total as u128) / cut.max(1) as u128) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// バイト数はトークン数の上界（1 トークンは 1 バイト以上）。
    /// [`crate::tool_result_log`] の「バイト数で早期 return」はこの性質に依存する。
    #[test]
    fn tokens_never_exceed_bytes() {
        for s in [
            "",
            "hello world",
            "日本語のテキストです。",
            r#"{"success":true,"data":{"list":[1,2,3]}}"#,
            &"あ".repeat(500),
            &"x".repeat(500),
        ] {
            assert!(
                estimate_tokens(s) <= s.len(),
                "token count exceeded byte count for {s:?}"
            );
        }
    }

    /// `tokens_reach_limit` は正確な `estimate_tokens` と同じ側に判定する（#576）。
    /// CJK・空白区切りの入力はトークンが窓境界を跨がないので、複数窓でも「以上/未満」が一致する。
    #[test]
    fn tokens_reach_limit_matches_exact_side() {
        let cases: &[&str] = &[
            "",
            "hello world",
            "日本語のテキストです。",
            &"あ".repeat(500),
            &"a ".repeat(5_000),
            &"あ".repeat(5_000),
            &"needle ".repeat(3_000),
        ];
        for s in cases {
            let exact = estimate_tokens(s);
            for &limit in &[1usize, 100, 2_500, 10_000] {
                assert_eq!(
                    tokens_reach_limit(s, limit),
                    exact >= limit,
                    "mismatch for limit={limit}, exact={exact}, len={}",
                    s.len()
                );
            }
        }
    }

    /// チャンク境界の上振れは「以上」判定を反転させない: 真に上限以上なら必ず true
    /// （false negative が無い）。境界ごとの上振れは高々わずか。
    #[test]
    fn tokens_reach_limit_has_no_false_negative() {
        // 複数窓を跨ぐ ASCII。累計は本来のトークン数以上になる。
        let s = "word ".repeat(20_000);
        let exact = estimate_tokens(&s);
        assert!(exact >= 2_500, "前提: 上限を超える入力 (exact={exact})");
        assert!(tokens_reach_limit(&s, 2_500));
        // 本来のトークン数ちょうどを limit にしても true（累計 >= 本来）。
        assert!(tokens_reach_limit(&s, exact));
    }

    /// 長い単一文字ラン（区切りの無い 1 pre-token）でも即座に返る＝全体を encode しない。
    #[test]
    fn tokens_reach_limit_returns_on_long_single_run() {
        for s in [&"a".repeat(300_000), &"あ".repeat(100_000)] {
            // panic せず bool を返せば十分（時間は PR で実測）。上限は必ず超える長さ。
            assert!(tokens_reach_limit(s, 2_500), "len={}", s.len());
        }
    }

    /// 空文字列や limit=0 の端。
    #[test]
    fn tokens_reach_limit_edges() {
        assert!(!tokens_reach_limit("", 1));
        assert!(tokens_reach_limit("", 0));
        assert!(tokens_reach_limit("anything", 0));
    }

    /// 概算は窓以下で正確、窓超で線形スケール（下振れしすぎない）。
    #[test]
    fn estimate_tokens_bounded_is_exact_under_window_and_scales_over() {
        let small = "日本語のテキストです。".repeat(10);
        assert!(small.len() <= BOUNDED_TOKENIZE_WINDOW);
        assert_eq!(estimate_tokens_bounded(&small), estimate_tokens(&small));

        let big = "word ".repeat(50_000);
        assert!(big.len() > BOUNDED_TOKENIZE_WINDOW);
        let est = estimate_tokens_bounded(&big);
        let exact = estimate_tokens(&big);
        // 均質な入力なので概算は実測の近傍（±20%）に収まる。
        assert!(
            est as f64 >= exact as f64 * 0.8 && est as f64 <= exact as f64 * 1.2,
            "est={est} exact={exact}"
        );
    }
}
