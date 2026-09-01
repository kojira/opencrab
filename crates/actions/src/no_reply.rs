//! `NO_REPLY` 終端解釈（DESIGN-RESUME-SETTLE §3.1 / §3.1.1・第一柱）。
//!
//! センチネル `NO_REPLY` は「読んだが黙る」を表すプロジェクト全体の制御トークン。
//! R4 統括裁定: **出現＝終端**（例外規則なし・文中引用も終端扱い）。応答に最初の
//! `NO_REPLY` が現れた地点で発言を打ち切り、
//!
//! - 前段（終端前）が空なら **沈黙**（`NoReply`）、
//! - 前段が非空なら **その前段のみ**を発言とし、
//! - `NO_REPLY` 以降は**常に破棄**する。
//!
//! 破棄した内容は黙って捨てず（§3.1.1・オーナー要件）、後続に非空テキストがある場合だけ
//! 固定タグ [`NO_REPLY_TRAILING_DISCARDED_TAG`] の WARN をサーバローカルログへ残す。
//! 破棄テキストは wire・配送・gateway 通知には一切載せない。
//!
//! 判定は完全一致ではなく終端解釈に集約する。散在していた `trim() == "NO_REPLY"` の各経路
//! （`session_inbound::delivery_effect` / discord `on_response_text` / nostr 完了 sink）は
//! すべて [`terminate_at_no_reply`] を通す。

/// プロジェクト全体の「読んで黙る」センチネル。下流はこの終端解釈で判定する。
pub const NO_REPLY_SENTINEL: &str = "NO_REPLY";

/// 破棄ログの固定タグ（`grep -c` で頻度集計できるよう 1 語不変・§3.1.1(b)）。
pub const NO_REPLY_TRAILING_DISCARDED_TAG: &str = "no_reply_trailing_discarded";

/// 破棄ログの tracing target（qc harness 等が拾う識別子）。
pub const NO_REPLY_LOG_TARGET: &str = "opencrab::no_reply";

/// `NO_REPLY` 終端解釈の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoReplyTermination {
    /// 終端前の全文（前段）。`NO_REPLY` が無い場合は応答全文をそのまま保持する。
    /// 前段が空（＝単独 `NO_REPLY` や行頭 `NO_REPLY`）のときは空文字。
    kept: String,
    /// 破棄した全文（`NO_REPLY` トークンを含む終端以降）。`NO_REPLY` が無い場合は `None`。
    discarded: Option<String>,
}

/// 破棄ログの相関コンテキスト（§3.1.1(a)・突き合わせ識別子）。
///
/// `session_id` / `agent_id` は `llm_logs` の生応答と突き合わせるための相関キー、
/// `origin` は発生経路（`discord` / `nostr` / `extgate` など）。
#[derive(Debug, Clone, Copy, Default)]
pub struct DeliveryContext<'a> {
    pub session_id: &'a str,
    pub agent_id: &'a str,
    pub origin: &'a str,
}

/// 応答文字列を最初の `NO_REPLY` で終端解釈する（純粋・ログは出さない）。
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

    /// §3.1.1: `NO_REPLY` の後に非空テキストが続いていたら破棄ログを WARN で残す。
    ///
    /// 破棄テキストは **サーバローカルログのみ**（wire・配送・gateway 通知には載せない）。
    /// 単独 `NO_REPLY` では WARN を出さない。
    pub fn log_trailing_discard(&self, ctx: DeliveryContext<'_>) {
        let Some(discarded) = self.trailing_discard() else {
            return;
        };
        tracing::warn!(
            target: NO_REPLY_LOG_TARGET,
            discarded = %discarded,
            discarded_len = discarded.chars().count(),
            kept_len = self.kept.trim().chars().count(),
            session_id = %ctx.session_id,
            agent_id = %ctx.agent_id,
            origin = %ctx.origin,
            "no_reply_trailing_discarded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // 単独 NO_REPLY は破棄ログを出さない（発火条件の負検証）。
        assert_eq!(t.trailing_discard(), None);
    }

    #[test]
    fn leading_body_then_no_reply_keeps_body_no_warn() {
        // 末尾 NO_REPLY（後続なし）は前段のみ発言・破棄ログ無し。
        let t = terminate_at_no_reply("本文だけ話す NO_REPLY");
        assert_eq!(t.speech(), Some("本文だけ話す"));
        assert_eq!(t.trailing_discard(), None);
    }

    #[test]
    fn body_then_no_reply_then_trailing_cuts_and_warns() {
        // 前段本文で確定・以降は破棄・破棄ログ発火。
        let t = terminate_at_no_reply("これは本文 NO_REPLY これはゴミ");
        assert_eq!(t.speech(), Some("これは本文"));
        assert_eq!(t.trailing_discard(), Some("NO_REPLY これはゴミ"));
    }

    #[test]
    fn leading_no_reply_with_trailing_is_silence_but_warns() {
        // 前段なし + 後続あり → 沈黙（NoReply）だが破棄ログは出す。
        let t = terminate_at_no_reply("NO_REPLY まだ続くゴミ");
        assert_eq!(t.speech(), None);
        assert_eq!(t.trailing_discard(), Some("NO_REPLY まだ続くゴミ"));
    }

    #[test]
    fn only_first_occurrence_terminates() {
        // 複数出現は最初の位置で終端（残りは破棄全文に含む）。
        let t = terminate_at_no_reply("A NO_REPLY B NO_REPLY C");
        assert_eq!(t.speech(), Some("A"));
        assert_eq!(t.trailing_discard(), Some("NO_REPLY B NO_REPLY C"));
    }

    #[test]
    fn no_reply_midword_still_terminates_per_r4() {
        // R4: 出現で終端・例外規則なし。文中引用（誤終端）も終端する（破棄ログで追跡）。
        let t = terminate_at_no_reply("説明: NO_REPLYという語について");
        assert_eq!(t.speech(), Some("説明:"));
        assert_eq!(t.trailing_discard(), Some("NO_REPLYという語について"));
    }
}
