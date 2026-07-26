//! 会話の先頭に付ける実行時コンテキスト（#190 S2）。
//!
//! LLM は「今が何時か」を知らないので、会話文字列の先頭に現在日時とトピックを
//! 前置する。純関数（時計とタイムゾーンの取得だけ）であり DB もゲートウェイも
//! 使わないため、`crates/server` ではなくここに置く。ゲートウェイ側のクレート
//! （web / Nostr など）が `crates/server` を参照せずに使えるようにするのが目的。
//!
//! Discord 向けの `message_id` 込みの変種は Discord 側に残っている（形が違い、
//! 共通化すると引数が増えるだけなので触らない）。

/// 変動コンテキスト（現在日時・トピック）を会話文字列の先頭へ前置する。
pub fn prepend_runtime_context(user_message: &str, session_theme: &str) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let tz_name = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
    let now = format!("{now} ({tz_name})");
    format!(
        "[Context]\nCurrent date and time: {now}\nCurrent discussion topic: {session_theme}\n\n{user_message}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 形（ヘッダ・トピック・本文の順）が変わらないこと。プロンプト先頭の形は
    /// 会話ログの再構築結果と LLM ログの検証（E2E）が依存している。
    #[test]
    fn context_header_precedes_message() {
        let out = prepend_runtime_context("こんにちは", "web_conversation");
        assert!(out.starts_with("[Context]\nCurrent date and time: "));
        assert!(out.contains("\nCurrent discussion topic: web_conversation\n\n"));
        assert!(out.ends_with("こんにちは"));
    }

    /// 本文が空でも壊れない（前置きだけが残る）。
    #[test]
    fn empty_message_keeps_header() {
        let out = prepend_runtime_context("", "theme");
        assert!(out.contains("Current discussion topic: theme"));
    }
}
