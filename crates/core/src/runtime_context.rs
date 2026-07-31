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

/// 人間からの直接メッセージで始まるターンの system prompt へ足す注意書き（#287）。
///
/// `## Silent Reply`（NO_REPLY の規約）は Bot 同士のループを止めるために必要だが、
/// **人間の直接の質問にまで効いてしまう**。実測（#284 の llm_logs）では、オーナーの
/// 「既存フォローはわたしだけなのでは？」に対しエージェントが `NO_REPLY` を返し、
/// 黙ったままツールを回し続けていた。
///
/// 「送信者が Bot かどうか」は受信側が**知っている事実**であり、LLM に推測させる必要は
/// ない。そこで人間からの受信ターンに限りこの注意書きを system prompt へ足し、
/// Silent Reply の例外をそのターンの事実として宣言する。Bot からの受信ターンや、
/// ハートビート等の自発ターンには足さない（従来どおり NO_REPLY が正常）。
pub const HUMAN_INBOUND_TURN_NOTE: &str = "## Direct Message From Human\n\
     このターンは人間（Bot ではない送信者）があなたに宛てて送ったメッセージで始まっています。\n\
     - NO_REPLY を返してはいけません。**Silent Reply よりこの指示が優先されます。**\n\
     - 作業中でも必ず返してください: 質問には答える。答えがまだ出ていなければ「今これをやっている」と現状を返す。\n\
     - 同じ文言の繰り返しは避けますが、「繰り返しになるから黙る」は禁止です。新しく報告できることが無くても、相手の発言には必ず一言返してください。";

/// 直近の受信が人間からなら [`HUMAN_INBOUND_TURN_NOTE`]、Bot からなら空文字を返す。
///
/// 呼び出し側は `opencrab_gateway::Sender::is_bot` をそのまま渡せばよい。空文字を
/// 返すのは「Bot ループ防止のための NO_REPLY は従来どおり残す」という意図の表明であり、
/// 呼び出し側で `if` を書き分けさせないためである。
pub fn human_inbound_turn_note(sender_is_bot: bool) -> &'static str {
    if sender_is_bot {
        ""
    } else {
        HUMAN_INBOUND_TURN_NOTE
    }
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

    /// 人間からの受信ターンには「NO_REPLY を返すな」が載る（#287）。
    #[test]
    fn human_sender_gets_the_no_silence_note() {
        let note = human_inbound_turn_note(false);
        assert!(!note.is_empty());
        assert!(note.contains("NO_REPLY を返してはいけません"));
        assert!(note.contains("Silent Reply よりこの指示が優先されます"));
    }

    /// Bot からの受信ターンには載らない（Bot 同士のループ防止を壊さない / #287）。
    #[test]
    fn bot_sender_gets_no_note() {
        assert_eq!(human_inbound_turn_note(true), "");
    }
}
