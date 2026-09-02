//! `CONTINUE` 継続マーカー（DESIGN-TURN-CONTINUATION §11 / §11.7・末尾マーカー方式・#890）。
//!
//! `CONTINUE` は `NO_REPLY` と対の制御トークン。生成 content の**最終行が `CONTINUE` 単独**
//! （その行に他の文字が無い・行頭行末の空白と末尾の改行/空白は無視）なら「このターンで作業を
//! 続ける意思」を表す。エンジンはその行を剥がして次イテレーションへ進み、剥がした本文だけを
//! 配送・保存する。同一行に他の文字がある `CONTINUE`（例「…です CONTINUE」）や途中出現は
//! 継続マーカーではない（剥がさず本文に残し、配送層が WARN を残す）。
//!
//! ## 配置（§11.5 との差分・要レビュー）
//! 設計 §11.5 は `no_reply.rs`（`crates/actions`）の隣を指示するが、継続を判定する engine は
//! `crates/core` にあり core は actions に依存できない（依存は `actions → core` の一方向）。両者
//! から使える最下層 `crates/core` に単一実装として置く。3 配送点（`session_inbound` / discord /
//! nostr）はいずれも core に依存するので `opencrab_core::strip_trailing_continue` を共有する。
//! `NO_REPLY` の実体は `opencrab_actions::NO_REPLY_SENTINEL` にあるが core からは参照できない
//! ため、優先判定用に [`NO_REPLY_SENTINEL`] へ複製する（一致は actions 側テストで固定）。

/// 「このターンを続ける」継続センチネル。最終行がこれ単独なら次イテレーションへ進む。
pub const CONTINUE_SENTINEL: &str = "CONTINUE";

/// 継続マーカーログの tracing target。
pub const CONTINUE_LOG_TARGET: &str = "opencrab::continue_marker";

/// `NO_REPLY` センチネルのミラー（`opencrab_actions::NO_REPLY_SENTINEL` の複製）。
///
/// core は actions に依存できないため NO_REPLY 優先（§11.1）の判定用にここへ複製する。
/// 両者の値が一致することは `opencrab_actions` 側の consistency テストで固定する。
pub const NO_REPLY_SENTINEL: &str = "NO_REPLY";

/// content の**最終行が `CONTINUE` 単独**なら、その行を除いた本文（末尾空白除去）を返す（＝継続）。
///
/// - 最終行が `CONTINUE` 単独（行頭行末の空白と末尾の改行/空白は無視）→ `Some(本文)`。
/// - それ以外（同一行に他の文字・途中出現・出現無し）→ `None`（継続しない・本文はそのまま）。
pub fn strip_trailing_continue(content: &str) -> Option<&str> {
    // 末尾の改行・空白は無視して最終行を取り出す。
    let trimmed = content.trim_end();
    let last_line_start = trimmed.rfind('\n').map_or(0, |i| i + 1);
    // 最終行が CONTINUE 単独（行頭行末空白のみ許容）でなければ継続マーカーではない。
    if trimmed[last_line_start..].trim() != CONTINUE_SENTINEL {
        return None;
    }
    // 最終行（と直前の改行）を除いた本文。`\n` は 1 バイト ASCII なので境界は安全。
    Some(trimmed[..last_line_start.saturating_sub(1)].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_marker_returns_none() {
        assert_eq!(strip_trailing_continue("普通の本文\nです"), None);
    }

    #[test]
    fn last_line_alone_is_stripped() {
        assert_eq!(
            strip_trailing_continue("感想を返すね⚡\nCONTINUE"),
            Some("感想を返すね⚡")
        );
    }

    #[test]
    fn same_line_marker_is_not_stripped() {
        // §11.7: 同一行に他の文字がある CONTINUE は継続マーカーではない。
        assert_eq!(strip_trailing_continue("作業を続ける CONTINUE"), None);
    }

    #[test]
    fn trailing_whitespace_is_ignored() {
        assert_eq!(
            strip_trailing_continue("続ける\nCONTINUE  \n\t"),
            Some("続ける")
        );
    }

    #[test]
    fn line_leading_trailing_space_is_allowed() {
        assert_eq!(
            strip_trailing_continue("続ける\n  CONTINUE  "),
            Some("続ける")
        );
    }

    #[test]
    fn standalone_marker_yields_empty_body() {
        assert_eq!(strip_trailing_continue("CONTINUE"), Some(""));
    }

    #[test]
    fn midtext_marker_returns_none() {
        assert_eq!(
            strip_trailing_continue("まず CONTINUE を確認してから続けます"),
            None
        );
    }

    #[test]
    fn substring_of_word_returns_none() {
        assert_eq!(strip_trailing_continue("これはDISCONTINUE"), None);
    }
}
