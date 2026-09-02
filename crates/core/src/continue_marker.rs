//! `CONTINUE` 継続マーカー（DESIGN-TURN-CONTINUATION §11 / §11.7・末尾マーカー方式・#890）。
//!
//! `CONTINUE` は `NO_REPLY` と対の制御トークン。生成 content の**最終行が `CONTINUE` 単独**
//! （その行に他の文字が無い・行頭行末の空白と末尾の改行/空白は無視）なら「このターンで作業を
//! 続ける意思」を表す。エンジンはその行を剥がして次イテレーションへ進み、剥がした本文だけを
//! 配送・保存する。同一行に他の文字がある `CONTINUE`（例「…です CONTINUE」）や途中出現は
//! 継続マーカーではない（剥がさず本文に残し、配送層が WARN を残す）。
//!
//! ## 配置（§11.5 レビュー裁定・core へ一元化）
//! 継続を判定する engine は `crates/core` にあり core は actions に依存できない（依存は
//! `actions → core` の一方向）。よって両者から使える最下層 `crates/core` に単一実装として
//! 置く（設計 §11.5 の「no_reply.rs の隣」は core へ改定）。3 配送点（`session_inbound` /
//! discord / nostr）はいずれも core に依存するので `opencrab_core::strip_trailing_continue`
//! を共有する。NO_REPLY 優先（§11.1）の判定に要る [`NO_REPLY_SENTINEL`] も含め、NO_REPLY /
//! CONTINUE の両センチネルは core が一元管理する。`opencrab_actions::NO_REPLY_SENTINEL` は
//! ここを re-export する（複製定数は撤去した）。

/// 「このターンを続ける」継続センチネル。最終行がこれ単独なら次イテレーションへ進む。
pub const CONTINUE_SENTINEL: &str = "CONTINUE";

/// 継続マーカーログの tracing target。
pub const CONTINUE_LOG_TARGET: &str = "opencrab::continue_marker";

/// プロジェクト全体の「読んで黙る」センチネル（正本・core が一元管理）。
///
/// `opencrab_actions::NO_REPLY_SENTINEL` はこの定数を re-export する。engine の NO_REPLY 優先
/// （§11.1）判定と、配送層の `terminate_at_no_reply` が同じ実体を参照する。
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

/// `NO_REPLY` 終端解釈の結果（純粋・DESIGN-RESUME-SETTLE §3.1・R4「出現＝終端」）。
///
/// 正本は core（NO_REPLY / CONTINUE の両センチネルと判定を core が一元管理する・§11.5）。
/// `opencrab_actions::{terminate_at_no_reply, NoReplyTermination}` はここを re-export し、配送層
/// （`visible_speech_after_markers` / discord `on_response_text` / nostr 完了 sink）と engine の
/// holding 配送・NO_REPLY 優先判定が**同じ 1 実装**を参照する（部分文字列の別実装を作らない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoReplyTermination {
    /// 終端前の全文（前段）。`NO_REPLY` が無ければ応答全文。前段が空なら空文字。
    kept: String,
    /// 破棄した全文（`NO_REPLY` トークンを含む終端以降）。`NO_REPLY` が無ければ `None`。
    discarded: Option<String>,
}

/// 応答文字列を最初の `NO_REPLY` で終端解釈する（純粋・ログは出さない・R4「出現＝終端」）。
///
/// 判定はプロジェクト唯一の実装。`.find(NO_REPLY_SENTINEL)`＝文中引用も終端（R4）。呼び出し側が
/// 部分文字列の別判定を書かず必ずここを通す（単一実装・#916 レビュー）。
pub fn terminate_at_no_reply(response: &str) -> NoReplyTermination {
    match response.find(NO_REPLY_SENTINEL) {
        None => NoReplyTermination {
            kept: response.to_string(),
            discarded: None,
        },
        Some(idx) => NoReplyTermination {
            // `NO_REPLY` はすべて ASCII なので `idx` はバイト境界。
            kept: response[..idx].to_string(),
            discarded: Some(response[idx..].to_string()),
        },
    }
}

impl NoReplyTermination {
    /// 応答に `NO_REPLY` が現れたか（＝終端したか）。
    pub fn terminated(&self) -> bool {
        self.discarded.is_some()
    }

    /// 配送すべき発言本文。前段が空（沈黙）なら `None`。
    ///
    /// - `NO_REPLY` 無し: 応答全文をそのまま返す（原挙動保存・空白のみでも `Some`）。
    /// - `NO_REPLY` 有り: 前段を前後空白除去し、空なら `None`（沈黙）。
    pub fn speech(&self) -> Option<&str> {
        match &self.discarded {
            None => Some(&self.kept),
            Some(_) => {
                let s = self.kept.trim();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
        }
    }

    /// 破棄ログを出すべき破棄全文（`NO_REPLY` の後に非空テキストが続く場合だけ `Some`）。
    ///
    /// 単独 `NO_REPLY`・末尾 `NO_REPLY`（後続が空白のみ）は正常沈黙なので `None`。
    pub fn trailing_discard(&self) -> Option<&str> {
        let discarded = self.discarded.as_deref()?;
        // discarded は必ず先頭が `NO_REPLY`。その後続を見る。
        let after = &discarded[NO_REPLY_SENTINEL.len()..];
        if after.trim().is_empty() {
            None
        } else {
            Some(discarded)
        }
    }

    /// 終端前の全文（配送層 `log_trailing_discard` の `kept_len` 用アクセサ）。
    pub fn kept(&self) -> &str {
        &self.kept
    }
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

    // ---- NO_REPLY 終端解釈（core 一元化・#916 で actions から移設） ----

    #[test]
    fn no_no_reply_keeps_full_text_verbatim() {
        let t = terminate_at_no_reply("普通の本文\nです");
        assert!(!t.terminated());
        assert_eq!(t.speech(), Some("普通の本文\nです"));
        assert_eq!(t.trailing_discard(), None);
    }

    #[test]
    fn standalone_no_reply_is_silence_without_warn() {
        let t = terminate_at_no_reply("NO_REPLY");
        assert!(t.terminated());
        assert_eq!(t.speech(), None);
        assert_eq!(t.trailing_discard(), None);
    }

    #[test]
    fn leading_body_then_no_reply_keeps_body_no_warn() {
        let t = terminate_at_no_reply("本文だけ話す NO_REPLY");
        assert_eq!(t.speech(), Some("本文だけ話す"));
        assert_eq!(t.trailing_discard(), None);
    }

    #[test]
    fn body_then_no_reply_then_trailing_cuts_and_warns() {
        let t = terminate_at_no_reply("これは本文 NO_REPLY これはゴミ");
        assert_eq!(t.speech(), Some("これは本文"));
        assert_eq!(t.trailing_discard(), Some("NO_REPLY これはゴミ"));
        assert_eq!(t.kept(), "これは本文 ");
    }

    #[test]
    fn leading_no_reply_with_trailing_is_silence_but_warns() {
        let t = terminate_at_no_reply("NO_REPLY まだ続くゴミ");
        assert_eq!(t.speech(), None);
        assert_eq!(t.trailing_discard(), Some("NO_REPLY まだ続くゴミ"));
    }

    #[test]
    fn only_first_occurrence_terminates() {
        let t = terminate_at_no_reply("A NO_REPLY B NO_REPLY C");
        assert_eq!(t.speech(), Some("A"));
        assert_eq!(t.trailing_discard(), Some("NO_REPLY B NO_REPLY C"));
    }

    #[test]
    fn no_reply_midword_still_terminates_per_r4() {
        let t = terminate_at_no_reply("説明: NO_REPLYという語について");
        assert_eq!(t.speech(), Some("説明:"));
        assert_eq!(t.trailing_discard(), Some("NO_REPLYという語について"));
    }
}
