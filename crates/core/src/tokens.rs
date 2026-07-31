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
}
