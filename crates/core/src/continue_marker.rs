//! `NO_REPLY` 明示終端の単一実装。
//!
//! core が終了判定を行い、actions の配送層も同じ解釈を再利用する。

/// プロジェクト全体の明示終端センチネル。
pub const NO_REPLY_SENTINEL: &str = "NO_REPLY";

/// 終端後の破棄を記録する tracing target。
pub const NO_REPLY_LOG_TARGET: &str = "opencrab::no_reply";

/// `NO_REPLY` 終端解釈の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoReplyTermination {
    kept: String,
    discarded: Option<String>,
}

/// 応答文字列を最初の `NO_REPLY` で終端解釈する。
pub fn terminate_at_no_reply(response: &str) -> NoReplyTermination {
    match response.find(NO_REPLY_SENTINEL) {
        None => NoReplyTermination {
            kept: response.to_string(),
            discarded: None,
        },
        Some(idx) => NoReplyTermination {
            kept: response[..idx].to_string(),
            discarded: Some(response[idx..].to_string()),
        },
    }
}

impl NoReplyTermination {
    pub fn terminated(&self) -> bool {
        self.discarded.is_some()
    }

    /// 配送すべき発言本文。終端前が空なら沈黙。
    pub fn speech(&self) -> Option<&str> {
        match &self.discarded {
            None => Some(&self.kept),
            Some(_) => {
                let speech = self.kept.trim();
                if speech.is_empty() {
                    None
                } else {
                    Some(speech)
                }
            }
        }
    }

    /// 終端後に非空テキストが続く場合だけ、破棄内容を返す。
    pub fn trailing_discard(&self) -> Option<&str> {
        let discarded = self.discarded.as_deref()?;
        let after = &discarded[NO_REPLY_SENTINEL.len()..];
        if after.trim().is_empty() {
            None
        } else {
            Some(discarded)
        }
    }

    pub fn kept(&self) -> &str {
        &self.kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_no_reply_keeps_full_text_verbatim() {
        let termination = terminate_at_no_reply("普通の本文\nです");
        assert!(!termination.terminated());
        assert_eq!(termination.speech(), Some("普通の本文\nです"));
        assert_eq!(termination.trailing_discard(), None);
    }

    #[test]
    fn standalone_no_reply_is_silence_without_warn() {
        let termination = terminate_at_no_reply("NO_REPLY");
        assert!(termination.terminated());
        assert_eq!(termination.speech(), None);
        assert_eq!(termination.trailing_discard(), None);
    }

    #[test]
    fn leading_body_then_no_reply_keeps_body_no_warn() {
        let termination = terminate_at_no_reply("本文だけ話す NO_REPLY");
        assert_eq!(termination.speech(), Some("本文だけ話す"));
        assert_eq!(termination.trailing_discard(), None);
    }

    #[test]
    fn body_then_no_reply_then_trailing_cuts_and_warns() {
        let termination = terminate_at_no_reply("これは本文 NO_REPLY これはゴミ");
        assert_eq!(termination.speech(), Some("これは本文"));
        assert_eq!(termination.trailing_discard(), Some("NO_REPLY これはゴミ"));
        assert_eq!(termination.kept(), "これは本文 ");
    }

    #[test]
    fn leading_no_reply_with_trailing_is_silence_but_warns() {
        let termination = terminate_at_no_reply("NO_REPLY まだ続くゴミ");
        assert_eq!(termination.speech(), None);
        assert_eq!(
            termination.trailing_discard(),
            Some("NO_REPLY まだ続くゴミ")
        );
    }

    #[test]
    fn only_first_occurrence_terminates() {
        let termination = terminate_at_no_reply("A NO_REPLY B NO_REPLY C");
        assert_eq!(termination.speech(), Some("A"));
        assert_eq!(
            termination.trailing_discard(),
            Some("NO_REPLY B NO_REPLY C")
        );
    }

    #[test]
    fn no_reply_midword_still_terminates() {
        let termination = terminate_at_no_reply("説明: NO_REPLYという語について");
        assert_eq!(termination.speech(), Some("説明:"));
        assert_eq!(
            termination.trailing_discard(),
            Some("NO_REPLYという語について")
        );
    }
}
